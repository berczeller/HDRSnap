//! Monitor enumeration: position, HDR state, SDR white level, peak brightness.

use windows::core::Interface;
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{ERROR_SUCCESS, LUID, RECT};
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

pub struct Monitor {
    /// None when DXGI is unavailable (GDI-only fallback).
    pub adapter: Option<IDXGIAdapter1>,
    pub output: Option<IDXGIOutput>,
    /// GDI device name, e.g. \\.\DISPLAY1
    pub device_name: String,
    pub friendly_name: String,
    /// Desktop rectangle in physical pixels (process is per-monitor DPI aware).
    pub rect: RECT,
    pub rotation: DXGI_MODE_ROTATION,
    /// Windows HDR ("Use HDR") is on for this display.
    pub hdr: bool,
    /// SDR content brightness, in scRGB units (1.0 = 80 nits).
    pub sdr_white_scale: f32,
    pub max_nits: f32,
    pub bits_per_color: u32,
}

impl Monitor {
    pub fn width(&self) -> i32 {
        self.rect.right - self.rect.left
    }
    pub fn height(&self) -> i32 {
        self.rect.bottom - self.rect.top
    }
    pub fn describe(&self) -> String {
        format!(
            "{} [{}] {}x{} at ({},{}) rot={} hdr={} sdr_white={:.0} nits peak={:.0} nits bpc={}",
            self.device_name,
            self.friendly_name,
            self.width(),
            self.height(),
            self.rect.left,
            self.rect.top,
            self.rotation.0,
            self.hdr,
            self.sdr_white_scale * 80.0,
            self.max_nits,
            self.bits_per_color
        )
    }
}

fn wstr(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

struct ConfigInfo {
    gdi_name: String,
    friendly: String,
    sdr_white_scale: Option<f32>,
}

/// Query the display configuration API for SDR white level + friendly names,
/// keyed by GDI device name.
fn query_display_config() -> Vec<ConfigInfo> {
    let mut out = Vec::new();
    unsafe {
        let mut np = 0u32;
        let mut nm = 0u32;
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != ERROR_SUCCESS {
            return out;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None) != ERROR_SUCCESS {
            return out;
        }
        paths.truncate(np as usize);
        for p in &paths {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header = header(DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, size_of_val(&src), p.sourceInfo.adapterId, p.sourceInfo.id);
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
                continue;
            }
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header = header(DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, size_of_val(&tgt), p.targetInfo.adapterId, p.targetInfo.id);
            let friendly =
                if DisplayConfigGetDeviceInfo(&mut tgt.header) == 0 { wstr(&tgt.monitorFriendlyDeviceName) } else { String::new() };
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
            white.header =
                header(DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, size_of_val(&white), p.targetInfo.adapterId, p.targetInfo.id);
            let sdr = if DisplayConfigGetDeviceInfo(&mut white.header) == 0 && white.SDRWhiteLevel > 0 {
                Some(white.SDRWhiteLevel as f32 / 1000.0)
            } else {
                None
            };
            out.push(ConfigInfo { gdi_name: wstr(&src.viewGdiDeviceName), friendly, sdr_white_scale: sdr });
        }
    }
    out
}

fn header(ty: DISPLAYCONFIG_DEVICE_INFO_TYPE, size: usize, adapter: LUID, id: u32) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
    DISPLAYCONFIG_DEVICE_INFO_HEADER { r#type: ty, size: size as u32, adapterId: adapter, id }
}

/// All active monitors. Uses DXGI; if that fails (remote sessions, odd
/// drivers) falls back to plain GDI monitor enumeration (SDR, GDI capture).
pub fn enumerate() -> windows::core::Result<Vec<Monitor>> {
    // DXGI enumeration can fail for a moment during display mode / full
    // screen transitions; the GDI list has no HDR info, so retry first.
    let mut r = enumerate_dxgi();
    for _ in 0..3 {
        if matches!(&r, Ok(v) if !v.is_empty()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        r = enumerate_dxgi();
    }
    match r {
        Ok(v) if !v.is_empty() => Ok(v),
        Ok(_) => {
            crate::log!("DXGI reported no outputs, using GDI monitor list");
            enumerate_gdi()
        }
        Err(e) => {
            crate::log!("DXGI enumeration failed ({e}), using GDI monitor list");
            enumerate_gdi()
        }
    }
}

fn enumerate_gdi() -> windows::core::Result<Vec<Monitor>> {
    use windows::Win32::Foundation::{LPARAM, TRUE};
    use windows::Win32::Graphics::Gdi::*;
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> windows::core::BOOL {
        unsafe {
            let v = &mut *(lp.0 as *mut Vec<Monitor>);
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(h, &mut mi.monitorInfo).as_bool() {
                v.push(Monitor {
                    adapter: None,
                    output: None,
                    device_name: wstr(&mi.szDevice),
                    friendly_name: String::new(),
                    rect: mi.monitorInfo.rcMonitor,
                    rotation: DXGI_MODE_ROTATION_IDENTITY,
                    hdr: false,
                    sdr_white_scale: 1.0,
                    max_nits: 0.0,
                    bits_per_color: 8,
                });
            }
            TRUE
        }
    }
    let mut v: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    Ok(v)
}

fn enumerate_dxgi() -> windows::core::Result<Vec<Monitor>> {
    let config = query_display_config();
    let mut monitors = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            ai += 1;
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                oi += 1;
                let Ok(desc) = output.GetDesc() else { continue };
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let device_name = wstr(&desc.DeviceName);
                let (mut hdr, mut max_nits, mut bpc) = (false, 0.0, 8);
                if let Ok(o6) = output.cast::<IDXGIOutput6>() {
                    if let Ok(d1) = o6.GetDesc1() {
                        hdr = d1.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
                        max_nits = d1.MaxLuminance;
                        bpc = d1.BitsPerColor;
                    }
                }
                let cfg = config.iter().find(|c| c.gdi_name.eq_ignore_ascii_case(&device_name));
                monitors.push(Monitor {
                    adapter: Some(adapter.clone()),
                    output: Some(output),
                    friendly_name: cfg.map(|c| c.friendly.clone()).unwrap_or_default(),
                    sdr_white_scale: if hdr { cfg.and_then(|c| c.sdr_white_scale).unwrap_or(2.5) } else { 1.0 },
                    device_name,
                    rect: desc.DesktopCoordinates,
                    rotation: desc.Rotation,
                    hdr,
                    max_nits,
                    bits_per_color: bpc,
                });
            }
        }
    }
    Ok(monitors)
}
