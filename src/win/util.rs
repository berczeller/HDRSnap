//! Small Win32 helpers: log file, clipboard, autostart, strings, time.

use hdrsnap_core::imageio::RgbImage;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::Graphics::Gdi::{BITMAPINFOHEADER, BI_RGB};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::CF_DIB;
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_Pictures, FOLDERID_RoamingAppData, FOLDERID_Videos, KNOWN_FOLDER_FLAG};

// ---------------------------------------------------------------------------
// Portable vs installed

/// Folder of HDRSnap.exe.
pub fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from("."))
}

/// The installer drops this file next to the exe. Without it HDRSnap is
/// portable and keeps everything in its own folder.
pub const INSTALLED_MARKER: &str = "HDRSnap.installed";

pub fn is_installed() -> bool {
    exe_dir().join(INSTALLED_MARKER).is_file()
}

/// Where settings.json, logs and (by default) captures live.
/// Portable: next to the exe. Installed: %APPDATA%\HDRSnap, with screenshots
/// in Pictures\HDRSnap and recordings in Videos\HDRSnap.
pub fn data_dir() -> PathBuf {
    if !is_installed() {
        return exe_dir();
    }
    let dir = known_folder(&FOLDERID_RoamingAppData).map(|p| p.join("HDRSnap")).unwrap_or_else(exe_dir);
    let _ = std::fs::create_dir_all(&dir);
    if let (Some(pics), Some(vids)) = (known_folder(&FOLDERID_Pictures), known_folder(&FOLDERID_Videos)) {
        let _ = hdrsnap_core::settings::DEFAULT_DIRS.set((pics.join("HDRSnap"), vids.join("HDRSnap")));
    }
    dir
}

fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None).ok()?;
        let s = p.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

// ---------------------------------------------------------------------------
// Log file (hdrsnap.log in the data folder)

static LOG: Mutex<Option<File>> = Mutex::new(None);

pub fn log_init(path: &Path) {
    let truncate = std::fs::metadata(path).map(|m| m.len() > 512 * 1024).unwrap_or(false);
    let f = OpenOptions::new().create(true).append(!truncate).write(true).truncate(truncate).open(path).ok();
    *LOG.lock().unwrap() = f;
}

pub fn log_write(msg: &str) {
    if let Ok(mut g) = LOG.lock() {
        if let Some(f) = g.as_mut() {
            let t = unsafe { GetLocalTime() };
            let _ = writeln!(
                f,
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}  {msg}",
                t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
            );
            let _ = f.flush();
        }
    }
}

/// "2026-10-28_14-03-22"
pub fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

pub fn wide_fill(dst: &mut [u16], s: &str) {
    let n = dst.len() - 1;
    let mut i = 0;
    for c in s.encode_utf16().take(n) {
        dst[i] = c;
        i += 1;
    }
    dst[i] = 0;
}

// ---------------------------------------------------------------------------
// Clipboard (CF_DIB, 24-bit bottom-up)

pub fn copy_image_to_clipboard(hwnd: HWND, img: &RgbImage) -> windows::core::Result<()> {
    let row = (img.width * 3).div_ceil(4) * 4;
    let hdr_size = size_of::<BITMAPINFOHEADER>();
    let total = hdr_size + row * img.height;
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, total)?;
        let p = GlobalLock(mem) as *mut u8;
        if p.is_null() {
            let e = windows::core::Error::from_thread();
            let _ = GlobalFree(Some(mem));
            return Err(e);
        }
        let bih = BITMAPINFOHEADER {
            biSize: hdr_size as u32,
            biWidth: img.width as i32,
            biHeight: img.height as i32, // bottom-up
            biPlanes: 1,
            biBitCount: 24,
            biCompression: BI_RGB.0,
            biSizeImage: (row * img.height) as u32,
            ..Default::default()
        };
        std::ptr::copy_nonoverlapping(&bih as *const _ as *const u8, p, hdr_size);
        let bits = std::slice::from_raw_parts_mut(p.add(hdr_size), row * img.height);
        for y in 0..img.height {
            let src = &img.data[y * img.width * 3..(y + 1) * img.width * 3];
            let dst = &mut bits[(img.height - 1 - y) * row..(img.height - 1 - y) * row + img.width * 3];
            for (s, d) in src.chunks_exact(3).zip(dst.chunks_exact_mut(3)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
            }
        }
        let _ = GlobalUnlock(mem);
        // Other apps (clipboard history, RDP, managers) can hold the
        // clipboard briefly; retry before giving up.
        let mut opened = OpenClipboard(Some(hwnd));
        for _ in 0..10 {
            if opened.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
            opened = OpenClipboard(Some(hwnd));
        }
        if let Err(e) = opened {
            let _ = GlobalFree(Some(mem));
            return Err(e);
        }
        let _ = EmptyClipboard();
        let r = SetClipboardData(CF_DIB.0 as u32, Some(HANDLE(mem.0)));
        let _ = CloseClipboard();
        if r.is_err() {
            let _ = GlobalFree(Some(mem));
        }
        r.map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Start with Windows (HKCU Run key, pointing at wherever the exe lives now)

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "HDRSnap";

pub fn set_autostart(enable: bool, exe: &Path) -> Result<(), String> {
    unsafe {
        let mut key = HKEY::default();
        let r = RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(RUN_KEY), None, KEY_SET_VALUE | KEY_QUERY_VALUE, &mut key);
        if r.is_err() {
            return Err(format!("open Run key: {r:?}"));
        }
        let name = HSTRING::from(RUN_VALUE);
        let result = if enable {
            let cmd = format!("\"{}\" --autostart", exe.display());
            let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
            let r = RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes));
            if r.is_err() { Err(format!("set Run value: {r:?}")) } else { Ok(()) }
        } else {
            let _ = RegDeleteValueW(key, PCWSTR(name.as_ptr()));
            Ok(())
        };
        let _ = RegCloseKey(key);
        result
    }
}
