//! ShareX-style region selector: a frozen, dimmed image of all monitors.
//! Drag to select, or hover + click to pick a window; Enter = monitor under
//! the cursor; Esc / right click cancels.

use hdrsnap_core::imageio::RgbImage;
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const ACCENT: COLORREF = COLORREF(0x0047_A0FF); // BGR: orange (#FFA047)
const MAG_SRC: i32 = 15; // magnifier source size (pixels)
const MAG_ZOOM: i32 = 10;

struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
}

impl Dib {
    unsafe fn new(w: i32, h: i32) -> Option<Self> {
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(dc, bmp.into());
            Some(Self { dc, bmp, old, bits: bits as *mut u8 })
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

struct State {
    origin: (i32, i32),
    w: i32,
    h: i32,
    bright: Dib,
    dim: Dib,
    back: Dib,
    monitors: Vec<RECT>,
    windows: Vec<RECT>,
    cursor: POINT, // client coords
    anchor: Option<POINT>,
    hover: Option<RECT>, // client coords
    result: Option<RECT>,
    done: bool,
    font: HFONT,
    small_font: HFONT,
    hint: String,
}

fn norm(a: POINT, b: POINT) -> RECT {
    RECT { left: a.x.min(b.x), top: a.y.min(b.y), right: a.x.max(b.x) + 1, bottom: a.y.max(b.y) + 1 }
}

fn contains(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

/// Top-level windows in z-order (front first), desktop coordinates.
fn collect_windows(exclude: HWND) -> Vec<RECT> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> windows::core::BOOL {
        unsafe {
            let ctx = &mut *(lp.0 as *mut (HWND, Vec<RECT>));
            if h == ctx.0 || !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
                return TRUE;
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
            if cloaked != 0 {
                return TRUE;
            }
            let mut cls = [0u16; 64];
            let n = GetClassNameW(h, &mut cls) as usize;
            let cls = String::from_utf16_lossy(&cls[..n]);
            if cls == "Progman" || cls == "WorkerW" {
                return TRUE; // desktop: falls through to monitor snapping
            }
            let mut r = RECT::default();
            if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as *mut _, size_of::<RECT>() as u32).is_err()
            {
                let _ = GetWindowRect(h, &mut r);
            }
            if r.right - r.left > 8 && r.bottom - r.top > 8 {
                ctx.1.push(r);
            }
            TRUE
        }
    }
    let mut ctx: (HWND, Vec<RECT>) = (exclude, Vec::new());
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.1
}

/// Show the selector. Returns the chosen rectangle in desktop coordinates.
/// `img` is the frozen screen with its top-left at desktop `origin`.
pub fn select_region(img: &RgbImage, origin: (i32, i32), monitors: &[RECT], hint: &str) -> Option<RECT> {
    let (w, h) = (img.width as i32, img.height as i32);
    unsafe {
        let bright = Dib::new(w, h)?;
        let dim = Dib::new(w, h)?;
        let back = Dib::new(w, h)?;
        let n = (w * h) as usize;
        let bb = std::slice::from_raw_parts_mut(bright.bits, n * 4);
        let db = std::slice::from_raw_parts_mut(dim.bits, n * 4);
        for i in 0..n {
            let (r, g, b) = (img.data[i * 3], img.data[i * 3 + 1], img.data[i * 3 + 2]);
            bb[i * 4] = b;
            bb[i * 4 + 1] = g;
            bb[i * 4 + 2] = r;
            db[i * 4] = (b as u16 * 2 / 5) as u8;
            db[i * 4 + 1] = (g as u16 * 2 / 5) as u8;
            db[i * 4 + 2] = (r as u16 * 2 / 5) as u8;
        }

        let to_client = |r: &RECT| RECT {
            left: r.left - origin.0,
            top: r.top - origin.1,
            right: r.right - origin.0,
            bottom: r.bottom - origin.1,
        };
        let windows = collect_windows(HWND::default()).iter().map(to_client).collect();
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let dpi = {
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            dx as i32
        };
        let font = CreateFontW(-(15 * dpi / 96), 0, 0, 0, 600, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, w!("Segoe UI"));
        let small_font = CreateFontW(-(13 * dpi / 96), 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, w!("Segoe UI"));

        let mut state = Box::new(State {
            origin,
            w,
            h,
            bright,
            dim,
            back,
            monitors: monitors.iter().map(to_client).collect(),
            windows,
            cursor: POINT { x: pt.x - origin.0, y: pt.y - origin.1 },
            anchor: None,
            hover: None,
            result: None,
            done: false,
            font,
            small_font,
            hint: hint.to_string(),
        });
        update_hover(&mut state);

        let hinst = GetModuleHandleW(None).ok()?;
        let class = w!("HDRSnapRegionOverlay");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("HDRSnap region"),
            WS_POPUP,
            origin.0,
            origin.1,
            w,
            h,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .ok()?;
        let sp: *mut State = &mut *state;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, sp as isize);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));

        let mut msg = MSG::default();
        while !(*sp).done {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                // WM_QUIT: re-post so the main loop sees it too
                PostQuitMessage(msg.wParam.0 as i32);
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let _ = DestroyWindow(hwnd);
        let _ = DeleteObject(state.font.into());
        let _ = DeleteObject(state.small_font.into());
        state.result.map(|r| RECT {
            left: r.left + origin.0,
            top: r.top + origin.1,
            right: r.right + origin.0,
            bottom: r.bottom + origin.1,
        })
    }
}

fn update_hover(s: &mut State) {
    let p = s.cursor;
    s.hover = s
        .windows
        .iter()
        .find(|r| contains(r, p))
        .or_else(|| s.monitors.iter().find(|r| contains(r, p)))
        .map(|r| RECT { left: r.left.max(0), top: r.top.max(0), right: r.right.min(s.w), bottom: r.bottom.min(s.h) });
}

fn finish(s: &mut State, r: Option<RECT>) {
    s.result = r.filter(|r| r.right - r.left >= 2 && r.bottom - r.top >= 2);
    s.done = true;
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let sp = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        if sp.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let s = &mut *sp;
        let pt = || POINT { x: (lp.0 & 0xffff) as i16 as i32, y: ((lp.0 >> 16) & 0xffff) as i16 as i32 };
        match msg {
            WM_MOUSEMOVE => {
                s.cursor = pt();
                if s.anchor.is_none() {
                    update_hover(s);
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                s.anchor = Some(pt());
                s.cursor = pt();
                SetCapture(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let _ = ReleaseCapture();
                if let Some(a) = s.anchor.take() {
                    let p = pt();
                    if (p.x - a.x).abs() < 4 && (p.y - a.y).abs() < 4 {
                        // click: take the hovered window / monitor
                        finish(s, s.hover);
                    } else {
                        let mut r = norm(a, p);
                        r.left = r.left.max(0);
                        r.top = r.top.max(0);
                        r.right = r.right.min(s.w);
                        r.bottom = r.bottom.min(s.h);
                        finish(s, Some(r));
                    }
                }
                LRESULT(0)
            }
            WM_RBUTTONDOWN => {
                if s.anchor.is_some() {
                    s.anchor = None;
                    let _ = ReleaseCapture();
                    update_hover(s);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else {
                    finish(s, None);
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                match VIRTUAL_KEY(wp.0 as u16) {
                    VK_ESCAPE => finish(s, None),
                    VK_RETURN => {
                        let p = s.cursor;
                        let m = s.monitors.iter().find(|r| contains(r, p)).copied();
                        finish(s, m);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_ACTIVATEAPP if wp.0 == 0 => {
                // Alt+Tab away cancels (like ShareX).
                finish(s, None);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                paint(s);
                let _ = BitBlt(dc, 0, 0, s.w, s.h, Some(s.back.dc), 0, 0, SRCCOPY);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn text_box(dc: HDC, font: HFONT, text: &str, x: i32, y: i32, bounds: (i32, i32)) {
    unsafe {
        let old = SelectObject(dc, font.into());
        let mut t: Vec<u16> = text.encode_utf16().collect();
        let mut rc = RECT::default();
        DrawTextW(dc, &mut t, &mut rc, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
        let (tw, th) = (rc.right - rc.left, rc.bottom - rc.top);
        let pad = th / 3;
        let bx = x.clamp(0, (bounds.0 - tw - pad * 2).max(0));
        let by = y.clamp(0, (bounds.1 - th - pad * 2).max(0));
        let bg = CreateSolidBrush(COLORREF(0x0020_1C1C));
        let mut br = RECT { left: bx, top: by, right: bx + tw + pad * 2, bottom: by + th + pad * 2 };
        FillRect(dc, &br, bg);
        let _ = DeleteObject(bg.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00FF_FFFF));
        br.left += pad;
        br.top += pad;
        DrawTextW(dc, &mut t, &mut br, DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(dc, old);
    }
}

unsafe fn frame(dc: HDC, r: &RECT, color: COLORREF, width: i32) {
    unsafe {
        let b = CreateSolidBrush(color);
        for i in 0..width {
            let rr = RECT { left: r.left - i - 1, top: r.top - i - 1, right: r.right + i + 1, bottom: r.bottom + i + 1 };
            FrameRect(dc, &rr, b);
        }
        let _ = DeleteObject(b.into());
    }
}

unsafe fn paint(s: &mut State) {
    unsafe {
        let dc = s.back.dc;
        let _ = BitBlt(dc, 0, 0, s.w, s.h, Some(s.dim.dc), 0, 0, SRCCOPY);
        let c = s.cursor;
        let sel = match s.anchor {
            Some(a) => Some((norm(a, c), true)),
            None => s.hover.map(|r| (r, false)),
        };
        if let Some((r, dragging)) = sel {
            let _ = BitBlt(dc, r.left, r.top, r.right - r.left, r.bottom - r.top, Some(s.bright.dc), r.left, r.top, SRCCOPY);
            frame(dc, &r, if dragging { ACCENT } else { COLORREF(0x00C0_C0C0) }, 2);
            let label = format!("{} × {}", r.right - r.left, r.bottom - r.top);
            text_box(dc, s.font, &label, r.left, r.top - 34, (s.w, s.h));
        }

        // crosshair
        let pen = CreatePen(PS_DOT, 1, COLORREF(0x00FF_FFFF));
        let old = SelectObject(dc, pen.into());
        SetBkMode(dc, TRANSPARENT);
        let _ = MoveToEx(dc, 0, c.y, None);
        let _ = LineTo(dc, s.w, c.y);
        let _ = MoveToEx(dc, c.x, 0, None);
        let _ = LineTo(dc, c.x, s.h);
        SelectObject(dc, old);
        let _ = DeleteObject(pen.into());

        // magnifier, kept inside the monitor under the cursor
        let mon = s.monitors.iter().find(|r| contains(r, c)).copied().unwrap_or(RECT { left: 0, top: 0, right: s.w, bottom: s.h });
        let size = MAG_SRC * MAG_ZOOM;
        let mut mx = c.x + 28;
        let mut my = c.y + 28;
        if mx + size > mon.right {
            mx = c.x - 28 - size;
        }
        if my + size + 30 > mon.bottom {
            my = c.y - 28 - size - 30;
        }
        SetStretchBltMode(dc, COLORONCOLOR);
        let _ = StretchBlt(dc, mx, my, size, size, Some(s.bright.dc), c.x - MAG_SRC / 2, c.y - MAG_SRC / 2, MAG_SRC, MAG_SRC, SRCCOPY);
        let center = RECT {
            left: mx + (MAG_SRC / 2) * MAG_ZOOM,
            top: my + (MAG_SRC / 2) * MAG_ZOOM,
            right: mx + (MAG_SRC / 2 + 1) * MAG_ZOOM,
            bottom: my + (MAG_SRC / 2 + 1) * MAG_ZOOM,
        };
        frame(dc, &center, ACCENT, 1);
        frame(dc, &RECT { left: mx, top: my, right: mx + size, bottom: my + size }, COLORREF(0x00FF_FFFF), 1);
        text_box(dc, s.small_font, &format!("{}, {}", c.x + s.origin.0, c.y + s.origin.1), mx, my + size + 4, (s.w, s.h));

        // hint at the top of the current monitor
        text_box(dc, s.small_font, &s.hint, mon.left + 16, mon.top + 16, (s.w, s.h));
    }
}

// ---------------------------------------------------------------------------
// Recording border: four thin topmost click-through windows just outside the
// recorded region, excluded from capture so they never end up in the video.

unsafe extern "system" fn border_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}

pub struct Border {
    windows: Vec<HWND>,
}

impl Border {
    pub fn show(r: RECT) -> Option<Self> {
        const T: i32 = 3;
        unsafe {
            let hinst = GetModuleHandleW(None).ok()?;
            let class = w!("HDRSnapRecordBorder");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(border_proc),
                hInstance: hinst.into(),
                lpszClassName: class,
                hbrBackground: CreateSolidBrush(COLORREF(0x003C_3CE8)), // red
                ..Default::default()
            };
            RegisterClassW(&wc);
            let rects = [
                (r.left - T, r.top - T, r.right - r.left + 2 * T, T),
                (r.left - T, r.bottom, r.right - r.left + 2 * T, T),
                (r.left - T, r.top, T, r.bottom - r.top),
                (r.right, r.top, T, r.bottom - r.top),
            ];
            let mut windows = Vec::new();
            for (x, y, w, h) in rects {
                if let Ok(hwnd) = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_LAYERED | WS_EX_NOACTIVATE,
                    class,
                    w!(""),
                    WS_POPUP,
                    x,
                    y,
                    w,
                    h,
                    None,
                    None,
                    Some(hinst.into()),
                    None,
                ) {
                    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 220, LWA_ALPHA);
                    let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    windows.push(hwnd);
                }
            }
            Some(Self { windows })
        }
    }
}

impl Drop for Border {
    fn drop(&mut self) {
        for h in &self.windows {
            unsafe {
                let _ = DestroyWindow(*h);
            }
        }
    }
}
