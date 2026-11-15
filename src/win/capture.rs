//! Screen capture: DXGI Desktop Duplication (FP16 on HDR monitors, BGRA8 on
//! SDR ones), GDI as fallback, plus multi-monitor compositing + tone mapping.

use super::display::Monitor;
use hdrsnap_core::imageio::RgbImage;
use hdrsnap_core::tonemap::{self, HdrParams, ToneMapReport};
use std::time::Instant;
use windows::core::{Interface, Result};
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::*;

pub enum Pixels {
    /// B8G8R8A8_UNORM, sRGB, tightly packed
    Bgra8(Vec<u8>),
    /// R16G16B16A16_FLOAT, scRGB linear, tightly packed
    Rgba16f(Vec<u16>),
}

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub pixels: Pixels,
}

impl Frame {
    pub fn format_name(&self) -> &'static str {
        match self.pixels {
            Pixels::Bgra8(_) => "BGRA8",
            Pixels::Rgba16f(_) => "RGBA16F (scRGB)",
        }
    }
}

// ---------------------------------------------------------------------------
// DXGI Desktop Duplication

/// Create a Desktop Duplication for `output`. Returns it with the format its
/// frames will arrive in.
///
/// On HDR monitors the list holds **only** FP16. Per the DXGI docs, when the
/// buffer being scanned out is not in the list DXGI converts it to one of the
/// listed formats. With `[FP16, BGRA8]` that may be BGRA8 (e.g. while a full
/// screen app or a window transition is scanned out directly), and an 8-bit
/// frame from an HDR desktop is clipped to 0..1 without the SDR white level:
/// the washed out, over-bright "ShareX look". Offering only FP16 rules that
/// out; callers still check each frame's format.
pub fn duplicate_output(o5: &IDXGIOutput5, dev: &ID3D11Device, hdr: bool) -> Result<(IDXGIOutputDuplication, DXGI_FORMAT)> {
    unsafe {
        let dup = if hdr {
            match o5.DuplicateOutput1(dev, 0, &[DXGI_FORMAT_R16G16B16A16_FLOAT]) {
                Ok(d) => d,
                Err(e) => {
                    crate::log!("FP16-only duplication refused ({e}), allowing BGRA8 as well");
                    o5.DuplicateOutput1(dev, 0, &[DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM])?
                }
            }
        } else {
            o5.DuplicateOutput1(dev, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])?
        };
        let fmt = dup.GetDesc().ModeDesc.Format;
        if hdr && fmt != DXGI_FORMAT_R16G16B16A16_FLOAT {
            crate::log!("warning: HDR monitor duplicated as format {} instead of FP16", fmt.0);
        }
        Ok((dup, fmt))
    }
}

pub fn capture_dxgi(mon: &Monitor) -> Result<Frame> {
    let (Some(adapter), Some(output)) = (&mon.adapter, &mon.output) else {
        return Err(windows::core::Error::from_hresult(DXGI_ERROR_UNSUPPORTED));
    };
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        let adapter: IDXGIAdapter = adapter.cast()?;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
        let device = device.unwrap();
        let ctx = ctx.unwrap();

        let dup = match output.cast::<IDXGIOutput5>() {
            Ok(o5) => duplicate_output(&o5, &device, mon.hdr)?.0,
            Err(_) => output.cast::<IDXGIOutput1>()?.DuplicateOutput(&device)?,
        };
        let dup_desc = dup.GetDesc();

        let mut staging: Option<ID3D11Texture2D> = None;
        let mut tex_desc = D3D11_TEXTURE2D_DESC::default();
        let mut got = false;
        for attempt in 0..12 {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            // After creating a duplication the first frame normally arrives
            // immediately; keep waiting a bit in case the GPU was idle.
            let timeout = if got { 50 } else { 250 };
            match dup.AcquireNextFrame(timeout, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    if got {
                        break;
                    }
                    continue;
                }
                Err(e) => return Err(e),
            }
            let tex: ID3D11Texture2D = res.unwrap().cast()?;
            if staging.is_none() {
                tex.GetDesc(&mut tex_desc);
                let sd = D3D11_TEXTURE2D_DESC {
                    Width: tex_desc.Width,
                    Height: tex_desc.Height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: tex_desc.Format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut t = None;
                device.CreateTexture2D(&sd, None, Some(&mut t))?;
                staging = t;
            }
            ctx.CopyResource(staging.as_ref().unwrap(), &tex);
            let _ = dup.ReleaseFrame();
            got = true;
            // A frame with a real present is a complete desktop image. If the
            // first one only carried a mouse update, grab one more if available.
            if info.LastPresentTime != 0 || attempt > 0 {
                break;
            }
        }
        if !got {
            return Err(windows::core::Error::from_hresult(DXGI_ERROR_WAIT_TIMEOUT));
        }
        let staging = staging.unwrap();
        if mon.hdr && tex_desc.Format != DXGI_FORMAT_R16G16B16A16_FLOAT {
            // 8-bit frame from an HDR monitor: already clipped by DXGI and
            // can't be tone mapped correctly. Let the caller retry.
            crate::log!("{}: got an 8-bit frame from an HDR monitor (format {})", mon.device_name, tex_desc.Format.0);
            return Err(windows::core::Error::from_hresult(DXGI_ERROR_UNSUPPORTED));
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let w = tex_desc.Width as usize;
        let h = tex_desc.Height as usize;
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *const u8;
        let pixels = if tex_desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            let mut v = vec![0u16; w * h * 4];
            for y in 0..h {
                let src = std::slice::from_raw_parts(base.add(y * pitch) as *const u16, w * 4);
                v[y * w * 4..(y + 1) * w * 4].copy_from_slice(src);
            }
            Pixels::Rgba16f(v)
        } else {
            let mut v = vec![0u8; w * h * 4];
            for y in 0..h {
                let src = std::slice::from_raw_parts(base.add(y * pitch), w * 4);
                v[y * w * 4..(y + 1) * w * 4].copy_from_slice(src);
            }
            Pixels::Bgra8(v)
        };
        ctx.Unmap(&staging, 0);

        Ok(rotate_to_desktop(Frame { width: w, height: h, pixels }, dup_desc.Rotation))
    }
}

/// Desktop Duplication returns the image in the panel's native orientation;
/// turn it back into desktop orientation for rotated monitors.
fn rotate_to_desktop(f: Frame, rot: DXGI_MODE_ROTATION) -> Frame {
    fn rotate_buf<T: Copy + Default>(src: &[T], w: usize, h: usize, r: i32) -> (Vec<T>, usize, usize) {
        let (dw, dh) = if r == 2 || r == 4 { (h, w) } else { (w, h) };
        let mut out = vec![T::default(); src.len()];
        for dy in 0..dh {
            for dx in 0..dw {
                let (sx, sy) = match r {
                    2 => (dy, h - 1 - dx),         // ROTATE90
                    3 => (w - 1 - dx, h - 1 - dy), // ROTATE180
                    4 => (w - 1 - dy, dx),         // ROTATE270
                    _ => (dx, dy),
                };
                let s = (sy * w + sx) * 4;
                let d = (dy * dw + dx) * 4;
                out[d..d + 4].copy_from_slice(&src[s..s + 4]);
            }
        }
        (out, dw, dh)
    }
    let r = rot.0;
    if !(2..=4).contains(&r) {
        return f;
    }
    match f.pixels {
        Pixels::Bgra8(v) => {
            let (o, w, h) = rotate_buf(&v, f.width, f.height, r);
            Frame { width: w, height: h, pixels: Pixels::Bgra8(o) }
        }
        Pixels::Rgba16f(v) => {
            let (o, w, h) = rotate_buf(&v, f.width, f.height, r);
            Frame { width: w, height: h, pixels: Pixels::Rgba16f(o) }
        }
    }
}

// ---------------------------------------------------------------------------
// GDI fallback (SDR only; on HDR monitors Windows gives a clipped SDR image)

pub fn capture_gdi(rect: &RECT) -> Result<Frame> {
    let w = (rect.right - rect.left).max(1);
    let h = (rect.bottom - rect.top).max(1);
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let result: Result<Vec<u8>> = (|| {
            let bmp = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let old = SelectObject(mem, bmp.into());
            let r = BitBlt(mem, 0, 0, w, h, Some(screen), rect.left, rect.top, SRCCOPY | CAPTUREBLT);
            let data = if r.is_ok() {
                Some(std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec())
            } else {
                None
            };
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            r?;
            Ok(data.unwrap())
        })();
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        Ok(Frame { width: w as usize, height: h as usize, pixels: Pixels::Bgra8(result?) })
    }
}

// ---------------------------------------------------------------------------
// Multi-monitor capture

pub struct MonitorResult {
    pub name: String,
    pub method: &'static str,
    pub format: &'static str,
    pub report: Option<ToneMapReport>,
    pub millis: u128,
    /// Untouched HDR frame (only kept when requested).
    pub raw_hdr: Option<Frame>,
}

pub struct Capture {
    pub image: RgbImage,
    /// Desktop coordinate of image pixel (0,0). Used by region capture (M2).
    #[allow(dead_code)]
    pub origin: (i32, i32),
    pub monitors: Vec<MonitorResult>,
}

pub struct CaptureOptions {
    pub force_gdi: bool,
    pub keep_raw_hdr: bool,
    pub params: HdrParams,
}

/// Capture the given monitors, convert each to SDR (tone mapping HDR ones
/// with their own SDR white level / peak) and composite them into one image
/// covering their bounding box.
pub fn capture_monitors(monitors: &[&Monitor], opts: &CaptureOptions) -> Result<Capture> {
    let left = monitors.iter().map(|m| m.rect.left).min().unwrap_or(0);
    let top = monitors.iter().map(|m| m.rect.top).min().unwrap_or(0);
    let right = monitors.iter().map(|m| m.rect.right).max().unwrap_or(1);
    let bottom = monitors.iter().map(|m| m.rect.bottom).max().unwrap_or(1);
    let cw = (right - left).max(1) as usize;
    let ch = (bottom - top).max(1) as usize;
    let mut image = RgbImage::new(cw, ch);
    let mut results = Vec::new();

    // Grab every monitor first (so they're as close in time as possible),
    // then do the heavier conversion.
    let mut frames = Vec::new();
    for m in monitors {
        let t = Instant::now();
        let res = if opts.force_gdi || m.output.is_none() {
            capture_gdi(&m.rect).map(|f| (f, "GDI"))
        } else {
            // A fresh duplication usually fixes a one-off bad frame (mode
            // change mid-capture, 8-bit frame on an HDR monitor).
            // GDI on an HDR monitor gives a clipped, washed out image, so try
            // a few fresh duplications first (they fail for a moment while
            // windows switch between composed and full screen presentation).
            let retry = |_| {
                std::thread::sleep(std::time::Duration::from_millis(150));
                capture_dxgi(m)
            };
            capture_dxgi(m).or_else(retry).or_else(retry).map(|f| (f, "DXGI")).or_else(|e| {
                crate::log!("{}: Desktop Duplication failed ({e}), falling back to GDI", m.device_name);
                capture_gdi(&m.rect).map(|f| (f, "GDI"))
            })
        };
        match res {
            Ok((frame, method)) => frames.push(Some((frame, method, t.elapsed().as_millis()))),
            Err(e) => {
                // Leave this monitor black rather than losing the others.
                crate::log!("{}: capture failed ({e}), leaving it black", m.device_name);
                frames.push(None);
            }
        }
    }
    if frames.iter().all(Option::is_none) {
        return Err(windows::core::Error::from_hresult(DXGI_ERROR_UNSUPPORTED));
    }

    for (m, entry) in monitors.iter().zip(frames) {
        let Some((frame, method, grab_ms)) = entry else { continue };
        let t = Instant::now();
        let ox = (m.rect.left - left) as usize;
        let oy = (m.rect.top - top) as usize;
        // Guard against a mode change between enumeration and capture.
        let w = frame.width.min(cw - ox);
        let h = frame.height.min(ch - oy);
        let pitch = cw * 3;
        let dst = &mut image.data[oy * pitch + ox * 3..];
        let format = frame.format_name();
        let mut report = None;
        match &frame.pixels {
            Pixels::Bgra8(px) => tonemap::bgra_to_rgb(px, frame.width * 4, w, h, dst, pitch),
            Pixels::Rgba16f(px) => {
                let mut p = opts.params;
                p.sdr_white_scale = m.sdr_white_scale;
                p.display_peak_nits = m.max_nits;
                report = Some(tonemap::tonemap_scrgb(px, frame.width * 4, w, h, &p, dst, pitch));
            }
        }
        let is_hdr = matches!(frame.pixels, Pixels::Rgba16f(_));
        results.push(MonitorResult {
            name: m.device_name.clone(),
            method,
            format,
            report,
            millis: grab_ms + t.elapsed().as_millis(),
            raw_hdr: if opts.keep_raw_hdr && is_hdr { Some(frame) } else { None },
        });
    }

    Ok(Capture { image, origin: (left, top), monitors: results })
}
