//! Tray application: hidden message window, tray icon + menu, global
//! hotkeys, screenshots (full screen / region) and recording.

use super::capture::{self, Capture, CaptureOptions};
use super::display::{self, Monitor};
use super::overlay::{self, Border};
use super::record::{self, RecordConfig, RecordResult, Recorder};
use super::{cursor, gpu, util};
use crate::log;
use hdrsnap_core::encoders::Container;
use hdrsnap_core::hotkey;
use hdrsnap_core::imageio::{self, RgbImage};
use hdrsnap_core::settings::Settings;
use hdrsnap_core::tonemap::{HdrParams, ToneMapMode};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MAIN_CLASS: PCWSTR = w!("HDRSnapMessageWindow");

const WM_TRAY: u32 = WM_APP + 1;
const WM_CAPTURE_DONE: u32 = WM_APP + 2;
const WM_RECORD_DONE: u32 = WM_APP + 3;
const NIN_BALLOONUSERCLICK: u32 = WM_USER + 5;
const TRAY_ID: u32 = 1;
const TIMER_TRAY_RETRY: usize = 1;
const TIMER_HOTKEY_RESUME: usize = 2;
const TIMER_REC_TIP: usize = 3;

const HK_FULL: i32 = 1;
const HK_REGION: i32 = 2;
const HK_REC_FULL: i32 = 3;
const HK_REC_REGION: i32 = 4;

// menu command ids
const CMD_REGION: usize = 1;
const CMD_FULL: usize = 2;
const CMD_REC_REGION: usize = 3;
const CMD_REC_FULL: usize = 4;
const CMD_REC_STOP: usize = 5;
const CMD_OPEN_SHOTS: usize = 6;
const CMD_OPEN_VIDEOS: usize = 7;
const CMD_SETTINGS: usize = 8;
const CMD_EXIT: usize = 9;

/// Messages other HDRSnap processes (settings window, a second launch) send.
pub const MSG_RELOAD: PCWSTR = w!("HDRSnap.Reload");
pub const MSG_SUSPEND_HOTKEYS: PCWSTR = w!("HDRSnap.SuspendHotkeys");
pub const MSG_OPEN_SETTINGS: PCWSTR = w!("HDRSnap.OpenSettings");

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static TRAY_TRIES: AtomicU32 = AtomicU32::new(0);
static MSG_IDS: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];

struct Recording {
    rec: Recorder,
    _border: Option<Border>,
}

struct App {
    exe: PathBuf,
    exe_dir: PathBuf,
    settings: Settings,
    hwnd: HWND,
    icon: HICON,
    icon_rec: HICON,
    busy: Arc<AtomicBool>,
    in_overlay: bool,
    last_file: Option<PathBuf>,
    recording: Option<Recording>,
    starting_record: bool,
    hotkeys_suspended: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Borrow app state briefly. Never call anything that pumps messages
/// (MessageBox, TrackPopupMenu, the overlay...) inside the closure.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|a| f(a.borrow_mut().as_mut().expect("app not initialised")))
}

struct Outcome {
    path: Option<PathBuf>,
    error: Option<String>,
    clipboard: Option<RgbImage>,
    summary: String,
}

// ---------------------------------------------------------------------------

pub fn run() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
    }
    // From here on `exe_dir` is the data folder (settings, logs, default
    // output folders): next to the exe for the portable zip, %APPDATA%\HDRSnap
    // for the installed copy. ffmpeg is always looked up next to the exe.
    let exe_dir = util::data_dir();
    let args: Vec<String> = std::env::args().collect();
    let has = |a: &str| args.iter().any(|x| x == a);

    // Settings window / setup wizard run as a separate short-lived process
    // so the tray app itself stays tiny.
    if has("--settings") || has("--setup") {
        crate::ui::run(&exe_dir, has("--setup"));
        return;
    }

    util::log_init(&exe_dir.join("hdrsnap.log"));
    std::panic::set_hook(Box::new(|info| log!("PANIC: {info}")));

    if has("--selftest-gpu") {
        let r = gpu::selftest();
        let text = match &r {
            Ok(t) => format!("GPU tone mapping self test passed.\n\n{t}"),
            Err(e) => format!("GPU tone mapping self test FAILED.\n\n{e}"),
        };
        log!("selftest: {}", text.replace('\n', " "));
        unsafe {
            MessageBoxW(None, &HSTRING::from(text), w!("HDRSnap"), MB_OK);
        }
        return;
    }

    // `HDRSnap.exe --capture`: one full screen shot with current settings.
    if has("--capture") {
        let (settings, _) = Settings::load(&exe_dir);
        let r = capture_screens(&settings, false).and_then(|c| save_capture(&settings, &exe_dir, c));
        match r {
            Ok(o) => log!("--capture: {}", o.path.map(|p| p.display().to_string()).unwrap_or_default()),
            Err(e) => log!("--capture failed: {e}"),
        }
        return;
    }

    // `HDRSnap.exe --record=<seconds>`: record the primary monitor (or the one
    // set in Settings) for that long with current settings, then exit.
    if let Some(secs) = std::env::args().find_map(|a| a.strip_prefix("--record=").and_then(|v| v.parse::<f64>().ok())) {
        let (settings, _) = Settings::load(&exe_dir);
        match record_for(&settings, &exe_dir, Duration::from_secs_f64(secs.clamp(0.5, 3600.0))) {
            Ok(p) => log!("--record: {p}"),
            Err(e) => log!("--record failed: {e}"),
        }
        return;
    }

    unsafe {
        let mutex = CreateMutexW(None, false, w!("Local\\HDRSnap.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Already running: open its settings instead of a second tray icon.
            if let Ok(h) = FindWindowW(MAIN_CLASS, None) {
                let _ = PostMessageW(Some(h), RegisterWindowMessageW(MSG_OPEN_SETTINGS), WPARAM(0), LPARAM(0));
            }
            return;
        }
        std::mem::forget(mutex);
    }

    log!("---- HDRSnap {VERSION} starting from {}", exe_dir.display());
    let (settings, err) = Settings::load(&exe_dir);
    if let Some(e) = &err {
        log!("{e}");
    }
    if err.is_none() {
        if let Err(e) = settings.save(&exe_dir) {
            log!("could not write settings.json: {e}");
        }
    }
    log_displays();

    let hwnd = match unsafe { create_window() } {
        Ok(h) => h,
        Err(e) => {
            log!("CreateWindow failed: {e}");
            return;
        }
    };
    let first_run = !settings.setup_completed;
    APP.with(|a| {
        *a.borrow_mut() = Some(App {
            exe: std::env::current_exe().unwrap_or_default(),
            exe_dir,
            settings,
            hwnd,
            icon: unsafe { load_icon(1) },
            icon_rec: unsafe { load_icon(2) },
            busy: Arc::new(AtomicBool::new(false)),
            in_overlay: false,
            last_file: None,
            recording: None,
            starting_record: false,
            hotkeys_suspended: false,
        })
    });

    apply_autostart();
    ensure_tray_icon(hwnd);
    register_hotkeys(true);
    if let Some(e) = err {
        balloon("Settings problem", &e);
    }
    if first_run {
        launch_ui("--setup");
    }
    let exe_dir = with_app(|a| a.exe_dir.clone());
    if let Some(ff) = record::ffmpeg_path(&exe_dir) {
        let w = display::enumerate().map(|m| m.iter().map(|m| m.width().max(0) as u32).max().unwrap_or(1920)).unwrap_or(1920);
        record::warm_up_probes(ff, w);
    }

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    log!("exit");
}

unsafe fn create_window() -> windows::core::Result<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: MAIN_CLASS, ..Default::default() };
        RegisterClassW(&wc);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
        for (i, name) in [MSG_RELOAD, MSG_SUSPEND_HOTKEYS, MSG_OPEN_SETTINGS].iter().enumerate() {
            MSG_IDS[i].store(RegisterWindowMessageW(*name), Ordering::Relaxed);
        }
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE::default(), MAIN_CLASS, w!("HDRSnap"), WS_OVERLAPPED, 0, 0, 0, 0, None, None, Some(hinst.into()), None)?;
        // Let the (same user, same integrity) settings process reach us.
        for i in 0..3 {
            let _ = ChangeWindowMessageFilterEx(hwnd, MSG_IDS[i].load(Ordering::Relaxed), MSGFLT_ALLOW, None);
        }
        Ok(hwnd)
    }
}

unsafe fn load_icon(id: u16) -> HICON {
    unsafe {
        let hinst = GetModuleHandleW(None).ok();
        let cx = GetSystemMetrics(SM_CXSMICON);
        let cy = GetSystemMetrics(SM_CYSMICON);
        match LoadImageW(hinst.map(Into::into), PCWSTR(id as usize as _), IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR) {
            Ok(h) => HICON(h.0),
            Err(_) => LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_HOTKEY => {
                match wp.0 as i32 {
                    HK_FULL => screenshot(false),
                    HK_REGION => screenshot(true),
                    HK_REC_FULL => toggle_record(false),
                    HK_REC_REGION => toggle_record(true),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_TRAY => {
                match lp.0 as u32 {
                    WM_LBUTTONUP => {
                        if with_app(|a| a.recording.is_some()) {
                            stop_recording();
                        }
                    }
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    WM_LBUTTONDBLCLK => {
                        if with_app(|a| a.recording.is_none()) {
                            launch_ui("--settings");
                        }
                    }
                    NIN_BALLOONUSERCLICK => {
                        if let Some(p) = with_app(|a| a.last_file.clone()) {
                            shell_open(&p);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CAPTURE_DONE => {
                let out = Box::from_raw(lp.0 as *mut Outcome);
                finish_capture(*out);
                LRESULT(0)
            }
            WM_RECORD_DONE => {
                let r = Box::from_raw(lp.0 as *mut Result<RecordResult, String>);
                finish_recording(*r);
                LRESULT(0)
            }
            WM_TIMER => {
                match wp.0 {
                    TIMER_TRAY_RETRY => {
                        if add_tray_icon() || TRAY_TRIES.fetch_add(1, Ordering::Relaxed) > 60 {
                            let _ = KillTimer(Some(hwnd), TIMER_TRAY_RETRY);
                        }
                    }
                    TIMER_HOTKEY_RESUME => {
                        let _ = KillTimer(Some(hwnd), TIMER_HOTKEY_RESUME);
                        with_app(|a| a.hotkeys_suspended = false);
                        register_hotkeys(false);
                    }
                    TIMER_REC_TIP => update_record_tooltip(),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Some(r) = with_app(|a| a.recording.take()) {
                    // let ffmpeg finish the file (GIF palette pass included)
                    r.rec.stop_and_wait(std::time::Duration::from_secs(60));
                }
                remove_tray_icon();
                PostQuitMessage(0);
                LRESULT(0)
            }
            m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                ensure_tray_icon(hwnd);
                LRESULT(0)
            }
            m if m != 0 && m == MSG_IDS[0].load(Ordering::Relaxed) => {
                reload_settings();
                LRESULT(0)
            }
            m if m != 0 && m == MSG_IDS[1].load(Ordering::Relaxed) => {
                if wp.0 != 0 {
                    // The settings window is recording a new hotkey: release
                    // ours so the key press reaches it. Auto-resume as a safety net.
                    with_app(|a| a.hotkeys_suspended = true);
                    unregister_hotkeys();
                    SetTimer(Some(hwnd), TIMER_HOTKEY_RESUME, 60_000, None);
                } else {
                    let _ = KillTimer(Some(hwnd), TIMER_HOTKEY_RESUME);
                    with_app(|a| a.hotkeys_suspended = false);
                    register_hotkeys(false);
                }
                LRESULT(0)
            }
            m if m != 0 && m == MSG_IDS[2].load(Ordering::Relaxed) => {
                launch_ui("--settings");
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ---------------------------------------------------------------------------
// Tray

fn tray_data() -> NOTIFYICONDATAW {
    with_app(|a| NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: a.hwnd,
        uID: TRAY_ID,
        hIcon: if a.recording.is_some() { a.icon_rec } else { a.icon },
        ..Default::default()
    })
}

fn add_tray_icon() -> bool {
    let mut nid = tray_data();
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    util::wide_fill(&mut nid.szTip, &format!("HDRSnap {VERSION}"));
    unsafe { Shell_NotifyIconW(NIM_ADD, &nid).as_bool() || Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() }
}

fn ensure_tray_icon(hwnd: HWND) {
    if !add_tray_icon() {
        log!("tray not ready, retrying");
        TRAY_TRIES.store(0, Ordering::Relaxed);
        unsafe {
            SetTimer(Some(hwnd), TIMER_TRAY_RETRY, 2000, None);
        }
    }
}

fn remove_tray_icon() {
    let nid = tray_data();
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

fn set_tray(tip: &str) {
    let mut nid = tray_data();
    nid.uFlags = NIF_ICON | NIF_TIP;
    util::wide_fill(&mut nid.szTip, tip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn balloon(title: &str, text: &str) {
    let mut nid = tray_data();
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
    util::wide_fill(&mut nid.szInfoTitle, title);
    util::wide_fill(&mut nid.szInfo, text);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn hk_label(s: &str) -> String {
    hotkey::parse(s).map(hotkey::format).unwrap_or_default()
}

fn show_menu(hwnd: HWND) {
    let (s, recording, has_ffmpeg) = with_app(|a| (a.settings.clone(), a.recording.as_ref().map(|r| r.rec.started), record::ffmpeg_path(&a.exe_dir).is_some()));
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let add = |id: usize, text: &str, enabled: bool| {
            let mut flags = MF_STRING;
            if !enabled {
                flags |= MF_GRAYED;
            }
            let _ = AppendMenuW(menu, flags, id, &HSTRING::from(text));
        };
        let sep = || {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        };
        add(CMD_REGION, &format!("Capture region\t{}", hk_label(&s.hotkey_region)), true);
        add(CMD_FULL, &format!("Capture full screen\t{}", hk_label(&s.hotkey_fullscreen)), true);
        sep();
        if let Some(t) = recording {
            let secs = t.elapsed().as_secs();
            add(CMD_REC_STOP, &format!("Stop recording ({}:{:02})", secs / 60, secs % 60), true);
        } else {
            let suffix = if has_ffmpeg { "" } else { " (needs ffmpeg.exe)" };
            add(CMD_REC_REGION, &format!("Record region{suffix}\t{}", hk_label(&s.hotkey_record_region)), has_ffmpeg);
            add(CMD_REC_FULL, &format!("Record full screen{suffix}\t{}", hk_label(&s.hotkey_record_fullscreen)), has_ffmpeg);
        }
        sep();
        add(CMD_OPEN_SHOTS, "Open screenshots folder", true);
        add(CMD_OPEN_VIDEOS, "Open recordings folder", true);
        sep();
        add(CMD_SETTINGS, "Settings...", true);
        add(CMD_EXIT, "Exit", true);
        let _ = SetMenuDefaultItem(menu, CMD_SETTINGS as u32, 0);

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        match cmd.0 as usize {
            CMD_REGION => screenshot(true),
            CMD_FULL => screenshot(false),
            CMD_REC_REGION => toggle_record(true),
            CMD_REC_FULL => toggle_record(false),
            CMD_REC_STOP => stop_recording(),
            CMD_OPEN_SHOTS => open_dir(with_app(|a| a.settings.output_dir(&a.exe_dir))),
            CMD_OPEN_VIDEOS => open_dir(with_app(|a| a.settings.video_output_dir(&a.exe_dir))),
            CMD_SETTINGS => launch_ui("--settings"),
            CMD_EXIT => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

fn launch_ui(arg: &str) {
    let exe = with_app(|a| a.exe.clone());
    if let Err(e) = std::process::Command::new(exe).arg(arg).spawn() {
        log!("could not open settings: {e}");
    }
}

fn reload_settings() {
    let err = with_app(|a| {
        let (s, err) = Settings::load(&a.exe_dir);
        a.settings = s;
        err
    });
    log!("settings reloaded");
    apply_autostart();
    register_hotkeys(true);
    if let Some(e) = err {
        balloon("Settings problem", &e);
    }
}

fn unregister_hotkeys() {
    let hwnd = with_app(|a| a.hwnd);
    for id in [HK_FULL, HK_REGION, HK_REC_FULL, HK_REC_REGION] {
        unsafe {
            let _ = UnregisterHotKey(Some(hwnd), id);
        }
    }
}

fn register_hotkeys(notify_failures: bool) {
    unregister_hotkeys();
    let (hwnd, s, suspended) = with_app(|a| (a.hwnd, a.settings.clone(), a.hotkeys_suspended));
    if suspended {
        return; // settings window is listening for a key combo
    }
    let mut failed = Vec::new();
    for (id, text, name) in [
        (HK_FULL, &s.hotkey_fullscreen, "full screen"),
        (HK_REGION, &s.hotkey_region, "region"),
        (HK_REC_FULL, &s.hotkey_record_fullscreen, "record full screen"),
        (HK_REC_REGION, &s.hotkey_record_region, "record region"),
    ] {
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let Some(hk) = hotkey::parse(text) else {
            log!("could not parse hotkey '{text}'");
            failed.push(format!("{text} ({name})"));
            continue;
        };
        match unsafe { RegisterHotKey(Some(hwnd), id, HOT_KEY_MODIFIERS(hk.mods) | MOD_NOREPEAT, hk.vk) } {
            Ok(()) => log!("hotkey {name} = {}", hotkey::format(hk)),
            Err(e) => {
                log!("RegisterHotKey {} failed: {e}", hotkey::format(hk));
                failed.push(format!("{} ({name})", hotkey::format(hk)));
            }
        }
    }
    if notify_failures && !failed.is_empty() {
        balloon("Hotkey unavailable", &format!("{} already used by another app. Change it in Settings > Hotkeys.", failed.join(", ")));
    }
}

fn apply_autostart() {
    let (on, exe) = with_app(|a| (a.settings.start_with_windows, a.exe.clone()));
    if let Err(e) = util::set_autostart(on, &exe) {
        log!("autostart: {e}");
    }
}

fn open_dir(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    shell_open(&dir);
}

fn shell_open(p: &Path) {
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(p.as_os_str()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

fn log_displays() {
    match display::enumerate() {
        Ok(ms) => {
            for m in &ms {
                log!("display: {}", m.describe());
            }
        }
        Err(e) => log!("display enumeration failed: {e}"),
    }
}

fn post_box_to<T>(hwnd_raw: isize, msg: u32, v: T) {
    let p = Box::into_raw(Box::new(v));
    unsafe {
        if PostMessageW(Some(HWND(hwnd_raw as _)), msg, WPARAM(0), LPARAM(p as isize)).is_err() {
            drop(Box::from_raw(p));
        }
    }
}

fn cursor_point() -> POINT {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    pt
}

fn monitor_at(ms: &[Monitor], pt: POINT) -> Option<&Monitor> {
    ms.iter().find(|m| pt.x >= m.rect.left && pt.x < m.rect.right && pt.y >= m.rect.top && pt.y < m.rect.bottom)
}

// ---------------------------------------------------------------------------
// Screenshots

fn hdr_params(s: &Settings) -> HdrParams {
    HdrParams {
        mode: if s.tonemap_mode == "clip" { ToneMapMode::Clip } else { ToneMapMode::Auto },
        knee: s.tonemap_knee,
        highlight_desaturation: s.highlight_desaturation,
        ..Default::default()
    }
}

/// Capture (and tone map) the monitors a screenshot needs. `all` = every
/// monitor (region selection); otherwise follows the full screen setting.
fn capture_screens(s: &Settings, all: bool) -> Result<Capture, String> {
    let monitors = display::enumerate().map_err(|e| format!("display enumeration: {e}"))?;
    if monitors.is_empty() {
        return Err("no displays found".into());
    }
    let selected: Vec<&Monitor> = if !all && s.fullscreen_target == "cursor_monitor" {
        monitor_at(&monitors, cursor_point()).map(|m| vec![m]).unwrap_or_else(|| monitors.iter().collect())
    } else {
        monitors.iter().collect()
    };
    let opts = CaptureOptions { force_gdi: s.capture_method == "gdi", keep_raw_hdr: s.save_raw_hdr_exr, params: hdr_params(s) };
    let mut cap = capture::capture_monitors(&selected, &opts).map_err(|e| format!("capture: {e}"))?;
    if s.include_cursor_screenshot {
        if let Some((pos, h)) = cursor::current() {
            if let Some(img) = cursor::render(h) {
                cursor::blend_into(&mut cap.image, cap.origin, pos, &img);
            }
        }
    }
    Ok(cap)
}

fn screenshot(region: bool) {
    let (s, exe_dir, busy, in_overlay) = with_app(|a| (a.settings.clone(), a.exe_dir.clone(), a.busy.clone(), a.in_overlay));
    if in_overlay || busy.load(Ordering::SeqCst) {
        return;
    }
    let t0 = Instant::now();
    let mut cap = match capture_screens(&s, region) {
        Ok(c) => c,
        Err(e) => {
            log!("capture failed: {e}");
            balloon("Screenshot failed", &e);
            return;
        }
    };
    log!("captured in {} ms", t0.elapsed().as_millis());
    if region {
        let rects: Vec<RECT> = display::enumerate().map(|m| m.iter().map(|m| m.rect).collect()).unwrap_or_default();
        with_app(|a| a.in_overlay = true);
        let sel = overlay::select_region(&cap.image, cap.origin, &rects, "Drag to select  ·  Click a window  ·  Enter: whole monitor  ·  Esc: cancel");
        with_app(|a| a.in_overlay = false);
        let Some(r) = sel else { return };
        let (x, y) = ((r.left - cap.origin.0) as usize, (r.top - cap.origin.1) as usize);
        cap.image = cap.image.crop(x, y, (r.right - r.left) as usize, (r.bottom - r.top) as usize);
    }
    if busy.swap(true, Ordering::SeqCst) {
        return;
    }
    let hwnd_raw = with_app(|a| a.hwnd.0 as isize);
    std::thread::spawn(move || {
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| save_capture(&s, &exe_dir, cap)))
            .unwrap_or_else(|_| Err("internal error while saving (see hdrsnap.log)".into()))
            .unwrap_or_else(|e| {
            log!("save failed: {e}");
            Outcome { path: None, error: Some(e), clipboard: None, summary: String::new() }
        });
        busy.store(false, Ordering::SeqCst);
        post_box_to(hwnd_raw, WM_CAPTURE_DONE, out);
    });
}

fn save_capture(s: &Settings, exe_dir: &Path, cap: Capture) -> Result<Outcome, String> {
    let t0 = Instant::now();
    let dir = s.output_dir(exe_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create folder {}: {e}", dir.display()))?;
    let ext = if s.is_jpg() { "jpg" } else { "png" };
    let stem = unique_stem(&dir, &format!("{}_{}", s.filename_prefix, util::timestamp()), ext);
    let path = dir.join(format!("{stem}.{ext}"));
    let r = if s.is_jpg() { imageio::save_jpg(&cap.image, &path, s.jpg_quality as u8) } else { imageio::save_png(&cap.image, &path, s.png_level()) };
    r.map_err(|e| format!("save {}: {e}", path.display()))?;

    let mut hdr_notes = Vec::new();
    for m in &cap.monitors {
        match &m.report {
            Some(r) => {
                log!(
                    "  {} via {} {} : hdr_pixels={:.3}% peak={:.2}x SDR white rolloff={} ({} ms)",
                    m.name,
                    m.method,
                    m.format,
                    r.hdr_pixel_fraction * 100.0,
                    r.source_peak,
                    r.rolloff_applied,
                    m.millis
                );
                hdr_notes.push(if r.rolloff_applied { "HDR highlights rolled off" } else { "HDR -> SDR" });
            }
            None => log!("  {} via {} {} ({} ms)", m.name, m.method, m.format, m.millis),
        }
        if let Some(raw) = &m.raw_hdr {
            if let capture::Pixels::Rgba16f(px) = &raw.pixels {
                let name: String = m.name.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                let exr = dir.join(format!("{stem}_{name}_raw.exr"));
                if let Err(e) = imageio::save_exr_half(px, raw.width * 4, raw.width, raw.height, &exr) {
                    log!("save exr: {e}");
                }
            }
        }
    }
    log!("saved {} ({}x{}, {} ms)", path.display(), cap.image.width, cap.image.height, t0.elapsed().as_millis());
    let mut summary = format!("{}x{}", cap.image.width, cap.image.height);
    if let Some(n) = hdr_notes.iter().find(|n| n.contains("rolled")).or(hdr_notes.first()) {
        summary += &format!(" · {n}");
    }
    Ok(Outcome { path: Some(path), error: None, clipboard: if s.copy_to_clipboard { Some(cap.image) } else { None }, summary })
}

fn unique_stem(dir: &Path, base: &str, ext: &str) -> String {
    let mut stem = base.to_string();
    let mut i = 2;
    while dir.join(format!("{stem}.{ext}")).exists() {
        stem = format!("{base}_{i}");
        i += 1;
    }
    stem
}

fn finish_capture(out: Outcome) {
    let (hwnd, notify) = with_app(|a| {
        if out.path.is_some() {
            a.last_file = out.path.clone();
        }
        (a.hwnd, a.settings.show_notification)
    });
    if let Some(img) = &out.clipboard {
        if let Err(e) = util::copy_image_to_clipboard(hwnd, img) {
            log!("clipboard: {e}");
        }
    }
    if let Some(e) = &out.error {
        balloon("Screenshot failed", e);
    } else if notify {
        let name = out.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        balloon("Screenshot saved", &format!("{name}\n{}", out.summary));
    }
}

// ---------------------------------------------------------------------------
// Recording

fn toggle_record(region: bool) {
    if with_app(|a| a.recording.is_some()) {
        stop_recording();
        return;
    }
    let (s, exe_dir, in_overlay, starting) = with_app(|a| (a.settings.clone(), a.exe_dir.clone(), a.in_overlay, a.starting_record));
    if in_overlay || starting {
        return;
    }
    let Some(ffmpeg) = record::ffmpeg_path(&exe_dir) else {
        balloon("Recording needs FFmpeg", "Put ffmpeg.exe next to HDRSnap.exe (it ships in the release zip).");
        return;
    };
    let monitors = match display::enumerate() {
        Ok(m) if !m.is_empty() => m,
        _ => {
            balloon("Recording failed", "Could not list displays.");
            return;
        }
    };

    let (mon_idx, rect) = if region {
        let cap = match capture_screens(&s, true) {
            Ok(c) => c,
            Err(e) => {
                balloon("Recording failed", &e);
                return;
            }
        };
        let rects: Vec<RECT> = monitors.iter().map(|m| m.rect).collect();
        with_app(|a| a.in_overlay = true);
        let sel = overlay::select_region(&cap.image, cap.origin, &rects, "Select the area to record  ·  Click a window  ·  Enter: whole monitor  ·  Esc: cancel");
        with_app(|a| a.in_overlay = false);
        let Some(r) = sel else { return };
        let center = POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
        let idx = monitors.iter().position(|m| monitor_at(std::slice::from_ref(m), center).is_some()).unwrap_or(0);
        let m = &monitors[idx].rect;
        if r.left < m.left || r.top < m.top || r.right > m.right || r.bottom > m.bottom {
            balloon("Recording one monitor", "The selection crossed monitors, so it was trimmed to one monitor.");
        }
        (idx, r)
    } else {
        let idx = if s.record_target == "primary" {
            monitors.iter().position(|m| m.rect.left == 0 && m.rect.top == 0)
        } else {
            let pt = cursor_point();
            monitors.iter().position(|m| monitor_at(std::slice::from_ref(m), pt).is_some())
        }
        .unwrap_or(0);
        (idx, monitors[idx].rect)
    };
    let mon = &monitors[mon_idx];
    let rect = record::fit_region(rect, &mon.rect);

    let container = Container::from_str(&s.video_format);
    let dir = s.video_output_dir(&exe_dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        balloon("Recording failed", &format!("Can't create {}: {e}", dir.display()));
        return;
    }
    let ext = container.ext();
    let stem = unique_stem(&dir, &format!("{}_{}", s.video_filename_prefix, util::timestamp()), ext);
    let output = dir.join(format!("{stem}.{ext}"));

    let show_border = s.show_record_border && rect != mon.rect;
    let hwnd_raw = with_app(|a| {
        a.starting_record = true;
        a.hwnd.0 as isize
    });
    let cfg = RecordConfig { device_name: mon.device_name.clone(), region: rect, settings: s, exe_dir, ffmpeg, output };
    let rec = record::start(cfg, Box::new(move |r| post_box_to(hwnd_raw, WM_RECORD_DONE, r)));
    let border = if show_border { Border::show(rect) } else { None };
    with_app(|a| {
        a.starting_record = false;
        a.recording = Some(Recording { rec, _border: border });
    });
    set_tray("Recording... click the icon to stop");
    unsafe {
        SetTimer(Some(with_app(|a| a.hwnd)), TIMER_REC_TIP, 1000, None);
    }
}

/// Command-line recording (`--record=<seconds>`), no tray / border.
fn record_for(s: &Settings, exe_dir: &Path, dur: Duration) -> Result<String, String> {
    let ffmpeg = record::ffmpeg_path(exe_dir).ok_or("ffmpeg.exe not found next to HDRSnap.exe")?;
    let monitors = display::enumerate().map_err(|e| format!("displays: {e}"))?;
    let idx = if s.record_target == "primary" {
        monitors.iter().position(|m| m.rect.left == 0 && m.rect.top == 0)
    } else {
        monitors.iter().position(|m| monitor_at(std::slice::from_ref(m), cursor_point()).is_some())
    }
    .unwrap_or(0);
    let mon = monitors.get(idx).ok_or("no display")?;
    let container = Container::from_str(&s.video_format);
    let dir = s.video_output_dir(exe_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let ext = container.ext();
    let stem = unique_stem(&dir, &format!("{}_{}", s.video_filename_prefix, util::timestamp()), ext);
    let output = dir.join(format!("{stem}.{ext}"));
    let cfg = RecordConfig {
        device_name: mon.device_name.clone(),
        region: mon.rect,
        settings: s.clone(),
        exe_dir: exe_dir.to_path_buf(),
        ffmpeg,
        output,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let rec = record::start(cfg, Box::new(move |r| {
        let _ = tx.send(r);
    }));
    std::thread::sleep(dur);
    rec.stop();
    let r = rx.recv().map_err(|_| "recorder vanished".to_string())??;
    Ok(format!("{} ({} frames, {} dropped, {:.1}s, hdr={})", r.path.display(), r.frames, r.dropped, r.seconds, r.hdr))
}

fn stop_recording() {
    with_app(|a| {
        if let Some(r) = &a.recording {
            r.rec.stop();
        }
    });
    set_tray("Saving recording...");
}

fn update_record_tooltip() {
    if let Some(t) = with_app(|a| a.recording.as_ref().map(|r| r.rec.started)) {
        let s = t.elapsed().as_secs();
        set_tray(&format!("Recording {}:{:02} - click to stop", s / 60, s % 60));
    }
}

fn finish_recording(r: Result<RecordResult, String>) {
    let hwnd = with_app(|a| {
        a.recording = None; // drops the border
        a.hwnd
    });
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_REC_TIP);
    }
    set_tray(&format!("HDRSnap {VERSION}"));
    match r {
        Ok(res) => {
            let notify = with_app(|a| {
                a.last_file = Some(res.path.clone());
                a.settings.show_notification
            });
            if notify {
                let name = res.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                log!("recording finished via {}", res.method);
                let mut text = format!("{name}\n{:.0}s · {}{}", res.seconds, res.encoder, if res.hdr { " · HDR tone mapped" } else { "" });
                if res.frames > 0 && res.dropped * 100 > res.frames {
                    text += &format!("\n{} frames dropped: try a hardware encoder or lower frame rate", res.dropped);
                }
                balloon("Recording saved", &text);
            }
        }
        Err(e) => balloon("Recording failed", &e),
    }
}
