//! Mouse cursor capture. Desktop Duplication doesn't include the cursor, so
//! we render it ourselves: draw the cursor on black and on white, and derive
//! colour + alpha from the difference (works for colour, alpha and classic
//! monochrome cursors). Inverting pixels (e.g. the text I-beam) become black
//! with a white outline so they stay visible on any background.

use hdrsnap_core::imageio::RgbImage;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub struct CursorImage {
    pub handle: isize,
    pub width: usize,
    pub height: usize,
    pub hotspot: (i32, i32),
    /// Straight-alpha RGBA8
    pub rgba: Vec<u8>,
}

/// Current cursor position (desktop pixels) and handle, if visible.
pub fn current() -> Option<(POINT, HCURSOR)> {
    unsafe {
        let mut ci = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
        GetCursorInfo(&mut ci).ok()?;
        if ci.flags.0 & CURSOR_SHOWING.0 == 0 || ci.hCursor.is_invalid() {
            return None;
        }
        Some((ci.ptScreenPos, ci.hCursor))
    }
}

pub fn render(h: HCURSOR) -> Option<CursorImage> {
    unsafe {
        let mut ii = ICONINFO::default();
        GetIconInfo(HICON(h.0), &mut ii).ok()?;
        let mut bm = BITMAP::default();
        let (w, hgt) = if !ii.hbmColor.is_invalid() {
            GetObjectW(ii.hbmColor.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _));
            (bm.bmWidth, bm.bmHeight)
        } else {
            GetObjectW(ii.hbmMask.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _));
            (bm.bmWidth, bm.bmHeight / 2)
        };
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(ii.hbmColor.into());
        }
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(ii.hbmMask.into());
        }
        if w <= 0 || hgt <= 0 || w > 512 || hgt > 512 {
            return None;
        }
        let on_black = draw(h, w, hgt, 0x00)?;
        let on_white = draw(h, w, hgt, 0xff)?;
        let n = (w * hgt) as usize;
        let mut rgba = vec![0u8; n * 4];
        let mut inverted = vec![false; n];
        for i in 0..n {
            let b = &on_black[i * 4..i * 4 + 3];
            let wh = &on_white[i * 4..i * 4 + 3];
            let inv = (0..3).any(|c| (wh[c] as i32) < (b[c] as i32) - 64);
            if inv {
                inverted[i] = true;
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[0, 0, 0, 255]);
                continue;
            }
            // alpha = 1 - (white - black) / 255, colour = black / alpha
            let diff = (0..3).map(|c| wh[c] as i32 - b[c] as i32).sum::<i32>() as f32 / 3.0;
            let a = (1.0 - diff / 255.0).clamp(0.0, 1.0);
            if a > 0.0 {
                for c in 0..3 {
                    // BGRA -> RGBA
                    rgba[i * 4 + c] = ((b[2 - c] as f32 / a).min(255.0)) as u8;
                }
            }
            rgba[i * 4 + 3] = (a * 255.0).round() as u8;
        }
        // White outline around inverted pixels.
        let (wu, hu) = (w as usize, hgt as usize);
        for y in 0..hu {
            for x in 0..wu {
                let i = y * wu + x;
                if inverted[i] || rgba[i * 4 + 3] > 0 {
                    continue;
                }
                let near = [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)].iter().any(|(dx, dy)| {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    nx >= 0 && ny >= 0 && (nx as usize) < wu && (ny as usize) < hu && inverted[ny as usize * wu + nx as usize]
                });
                if near {
                    rgba[i * 4..i * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
        }
        Some(CursorImage { handle: h.0 as isize, width: wu, height: hu, hotspot: (ii.xHotspot as i32, ii.yHotspot as i32), rgba })
    }
}

unsafe fn draw(h: HCURSOR, w: i32, hgt: i32, bg: u8) -> Option<Vec<u8>> {
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        ReleaseDC(None, screen);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -hgt,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            return None;
        };
        let old = SelectObject(dc, bmp.into());
        let len = (w * hgt * 4) as usize;
        std::ptr::write_bytes(bits as *mut u8, bg, len);
        let ok = DrawIconEx(dc, 0, 0, HICON(h.0), 0, 0, 0, None, DI_NORMAL).is_ok();
        let _ = GdiFlush();
        let out = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        ok.then_some(out)
    }
}

/// Blend the cursor into an RGB image whose top-left is at desktop `origin`.
pub fn blend_into(img: &mut RgbImage, origin: (i32, i32), pos: POINT, cur: &CursorImage) {
    let x0 = pos.x - cur.hotspot.0 - origin.0;
    let y0 = pos.y - cur.hotspot.1 - origin.1;
    for cy in 0..cur.height as i32 {
        let y = y0 + cy;
        if y < 0 || y >= img.height as i32 {
            continue;
        }
        for cx in 0..cur.width as i32 {
            let x = x0 + cx;
            if x < 0 || x >= img.width as i32 {
                continue;
            }
            let s = ((cy as usize) * cur.width + cx as usize) * 4;
            let a = cur.rgba[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let d = (y as usize * img.width + x as usize) * 3;
            for c in 0..3 {
                img.data[d + c] = ((cur.rgba[s + c] as u32 * a + img.data[d + c] as u32 * (255 - a) + 127) / 255) as u8;
            }
        }
    }
}
