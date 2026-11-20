//! GPU frame converter for recording: one compute pass that (for HDR
//! monitors) applies the same tone mapping as screenshots, blends the
//! cursor, and writes either NV12 (BT.709 limited range, for video encoders)
//! or RGBA (for GIF). A second tiny pass builds a brightness histogram so the
//! highlight roll-off can adapt over time like the screenshot path.

use windows::core::{s, Result, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

pub const HIST_BINS: usize = 64;
pub const HIST_MAX_LOG2: f32 = 8.0;

const HLSL: &str = r#"
Texture2D<float4> Src : register(t0);
Texture2D<float4> Cur : register(t1);
RWTexture2D<float>  OutY    : register(u0);
RWTexture2D<float2> OutUV   : register(u1);
RWTexture2D<float4> OutRGBA : register(u2);
RWBuffer<uint>      Hist    : register(u3);

cbuffer P : register(b0) {
    int4   A; // region origin (desktop orientation, monitor-relative) xy, output size zw
    int4   B; // source texture size xy, rotation z (1..4), isHdr w
    float4 C; // 1/sdrWhite, knee, peak, curve b
    float4 D; // desaturation, rolloff on, histogram step, unused
    int4   E; // cursor top-left in output xy, cursor size zw
    int4   F; // cursor on, mode (0 = NV12, 1 = RGBA)
};

int2 srcCoord(int2 d) {
    int w = B.x, h = B.y;
    if (B.z == 2) return int2(d.y, h - 1 - d.x);
    if (B.z == 3) return int2(w - 1 - d.x, h - 1 - d.y);
    if (B.z == 4) return int2(w - 1 - d.y, d.x);
    return d;
}

float oetf(float v) { return v <= 0.0031308 ? v * 12.92 : 1.055 * pow(v, 1.0 / 2.4) - 0.055; }

float3 toneMap(float3 c) {
    c *= C.x;
    if (!all(abs(c) < 1e30)) return float3(0, 0, 0); // NaN / Inf
    float mn = min(c.r, min(c.g, c.b));
    if (mn < 0) {
        float y = dot(c, float3(0.2126, 0.7152, 0.0722));
        if (y <= 0) c = float3(0, 0, 0);
        else { float t = y / (y - mn); c = y + (c - y) * t; }
    }
    if (D.y > 0.5) {
        float m = max(c.r, max(c.g, c.b));
        float k = C.y, pk = C.z, b = C.w;
        if (m > k) {
            float m2 = 1.0;
            if (m < pk) { float t = (m - k) / (pk - k); m2 = k + (1.0 - k) * (t * (1.0 + b) / (t + b)); }
            c *= m2 / m;
            if (m > 1.0 && D.x > 0) { float w = D.x * saturate((m - 1.0) / (pk - 1.0)); c += (m2 - c) * w; }
        }
    }
    c = saturate(c);
    return float3(oetf(c.r), oetf(c.g), oetf(c.b));
}

float3 fetch(int2 o) {
    float4 s = Src.Load(int3(srcCoord(A.xy + o), 0));
    float3 c = B.w != 0 ? toneMap(s.rgb) : saturate(s.rgb);
    if (F.x != 0) {
        int2 cp = o - E.xy;
        if (cp.x >= 0 && cp.y >= 0 && cp.x < E.z && cp.y < E.w) {
            float4 k = Cur.Load(int3(cp, 0));
            c = lerp(c, k.rgb, k.a);
        }
    }
    return c;
}

float luma(float3 c) { return (16.0 + 219.0 * dot(c, float3(0.2126, 0.7152, 0.0722))) / 255.0; }

[numthreads(8, 8, 1)]
void CSConvert(uint3 id : SV_DispatchThreadID) {
    if (F.y == 0) {
        int2 o = int2(id.xy) * 2;
        if (o.x >= A.z || o.y >= A.w) return;
        float3 p00 = fetch(o), p10 = fetch(o + int2(1, 0)), p01 = fetch(o + int2(0, 1)), p11 = fetch(o + int2(1, 1));
        OutY[o] = luma(p00);
        OutY[o + int2(1, 0)] = luma(p10);
        OutY[o + int2(0, 1)] = luma(p01);
        OutY[o + int2(1, 1)] = luma(p11);
        float3 a = (p00 + p10 + p01 + p11) * 0.25;
        float y = dot(a, float3(0.2126, 0.7152, 0.0722));
        OutUV[id.xy] = float2((128.0 + 224.0 * (a.b - y) / 1.8556) / 255.0, (128.0 + 224.0 * (a.r - y) / 1.5748) / 255.0);
    } else {
        int2 o = int2(id.xy);
        if (o.x >= A.z || o.y >= A.w) return;
        OutRGBA[o] = float4(fetch(o), 1.0);
    }
}

#ifdef HIST
[numthreads(8, 8, 1)]
void CSHist(uint3 id : SV_DispatchThreadID) {
    int stride = (int)D.z;
    int2 o = int2(id.xy) * stride;
    if (o.x >= A.z || o.y >= A.w) return;
    float3 s = Src.Load(int3(srcCoord(A.xy + o), 0)).rgb * C.x;
    float m = max(s.r, max(s.g, s.b));
    if (m > 1.02 && m < 65000.0) {
        uint bin = min(63u, (uint)(log2(m) / 8.0 * 64.0));
        InterlockedAdd(Hist[bin], 1);
        InterlockedAdd(Hist[64], 1);
    }
}
#endif
"#;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Params {
    pub a: [i32; 4],
    pub b: [i32; 4],
    pub c: [f32; 4],
    pub d: [f32; 4],
    pub e: [i32; 4],
    pub f: [i32; 4],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutMode {
    Nv12,
    Rgba,
}

fn compile(entry: PCSTR, hist: bool) -> Result<Vec<u8>> {
    unsafe {
        let mut code = None;
        let mut errors = None;
        let defines = [
            D3D_SHADER_MACRO { Name: s!("HIST"), Definition: s!("1") },
            D3D_SHADER_MACRO { Name: PCSTR::null(), Definition: PCSTR::null() },
        ];
        let r = D3DCompile(
            HLSL.as_ptr() as *const _,
            HLSL.len(),
            s!("hdrsnap.hlsl"),
            if hist { Some(defines.as_ptr()) } else { None },
            None,
            entry,
            s!("cs_5_0"),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        );
        if let Err(e) = r {
            let msg = errors
                .map(|b: ID3DBlob| {
                    String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).to_string()
                })
                .unwrap_or_default();
            crate::log!("shader compile failed: {msg}");
            return Err(windows::core::Error::from_hresult(e.code()));
        }
        let code = code.unwrap();
        Ok(std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize()).to_vec())
    }
}

struct Plane {
    tex: ID3D11Texture2D,
    uav: ID3D11UnorderedAccessView,
    staging: Vec<ID3D11Texture2D>,
    w: u32,
    h: u32,
    bpp: usize,
}

const RING: usize = 3;
/// Frames a converted image spends in the readback ring before it reaches
/// ffmpeg (the video lags real time by this much).
pub const READBACK_LATENCY: u32 = (RING - 1) as u32;

impl Plane {
    fn new(dev: &ID3D11Device, w: u32, h: u32, fmt: DXGI_FORMAT, bpp: usize) -> Result<Self> {
        unsafe {
            let mut d = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: fmt,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            dev.CreateTexture2D(&d, None, Some(&mut tex))?;
            let tex = tex.unwrap();
            let mut uav = None;
            dev.CreateUnorderedAccessView(&tex, None, Some(&mut uav))?;
            d.Usage = D3D11_USAGE_STAGING;
            d.BindFlags = 0;
            d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = Vec::new();
            for _ in 0..RING {
                let mut t = None;
                dev.CreateTexture2D(&d, None, Some(&mut t))?;
                staging.push(t.unwrap());
            }
            Ok(Self { tex, uav: uav.unwrap(), staging, w, h, bpp })
        }
    }

    unsafe fn read(&self, ctx: &ID3D11DeviceContext, slot: usize, out: &mut Vec<u8>) -> Result<()> {
        unsafe {
            let st = &self.staging[slot];
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let row = self.w as usize * self.bpp;
            for y in 0..self.h as usize {
                let p = (m.pData as *const u8).add(y * m.RowPitch as usize);
                out.extend_from_slice(std::slice::from_raw_parts(p, row));
            }
            ctx.Unmap(st, 0);
            Ok(())
        }
    }
}

pub struct Converter {
    dev: ID3D11Device,
    ctx: ID3D11DeviceContext,
    cs_convert: ID3D11ComputeShader,
    cs_hist: Option<ID3D11ComputeShader>,
    cbuf: ID3D11Buffer,
    src: ID3D11Texture2D,
    src_srv: ID3D11ShaderResourceView,
    cursor: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, isize)>,
    dummy_srv: ID3D11ShaderResourceView,
    planes: Vec<Plane>, // NV12: [Y, UV]; RGBA: [RGBA]
    hist: ID3D11Buffer,
    hist_uav: ID3D11UnorderedAccessView,
    hist_staging: Vec<ID3D11Buffer>,
    pub mode: OutMode,
    pub out_w: u32,
    pub out_h: u32,
    frame: usize,
    hist_frame: usize,
}

impl Converter {
    /// `src_desc`: format/size of the duplicated desktop texture.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dev: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        src_fmt: DXGI_FORMAT,
        src_w: u32,
        src_h: u32,
        out_w: u32,
        out_h: u32,
        mode: OutMode,
    ) -> Result<Self> {
        unsafe {
            let cs = |entry, hist| -> Result<ID3D11ComputeShader> {
                let code = compile(entry, hist)?;
                let mut s = None;
                dev.CreateComputeShader(&code, None, Some(&mut s))?;
                Ok(s.unwrap())
            };
            let cs_convert = cs(s!("CSConvert"), false)?;
            // Histogram needs UAV atomics; if unavailable the roll-off just
            // stays off (recording still works).
            let cs_hist = cs(s!("CSHist"), true).ok();

            let bd = D3D11_BUFFER_DESC {
                ByteWidth: size_of::<Params>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let mut cbuf = None;
            dev.CreateBuffer(&bd, None, Some(&mut cbuf))?;

            let (src, src_srv) = Self::make_srv_texture(dev, src_w, src_h, src_fmt, None)?;
            let (_, dummy_srv) = Self::make_srv_texture(dev, 1, 1, DXGI_FORMAT_R8G8B8A8_UNORM, Some(&[0u8; 4]))?;

            let planes = match mode {
                OutMode::Nv12 => vec![
                    Plane::new(dev, out_w, out_h, DXGI_FORMAT_R8_UNORM, 1)?,
                    Plane::new(dev, out_w / 2, out_h / 2, DXGI_FORMAT_R8G8_UNORM, 2)?,
                ],
                OutMode::Rgba => vec![Plane::new(dev, out_w, out_h, DXGI_FORMAT_R8G8B8A8_UNORM, 4)?],
            };

            let hb = D3D11_BUFFER_DESC {
                ByteWidth: ((HIST_BINS + 1) * 4) as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
                ..Default::default()
            };
            let mut hist = None;
            dev.CreateBuffer(&hb, None, Some(&mut hist))?;
            let hist = hist.unwrap();
            let ud = D3D11_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_UINT,
                ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
                Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D11_BUFFER_UAV { FirstElement: 0, NumElements: (HIST_BINS + 1) as u32, Flags: 0 },
                },
            };
            let mut hist_uav = None;
            dev.CreateUnorderedAccessView(&hist, Some(&ud), Some(&mut hist_uav))?;
            let sb = D3D11_BUFFER_DESC {
                ByteWidth: ((HIST_BINS + 1) * 4) as u32,
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..Default::default()
            };
            let mut hist_staging = Vec::new();
            for _ in 0..RING {
                let mut b = None;
                dev.CreateBuffer(&sb, None, Some(&mut b))?;
                hist_staging.push(b.unwrap());
            }

            Ok(Self {
                dev: dev.clone(),
                ctx: ctx.clone(),
                cs_convert,
                cs_hist,
                cbuf: cbuf.unwrap(),
                src,
                src_srv,
                cursor: None,
                dummy_srv,
                planes,
                hist,
                hist_uav: hist_uav.unwrap(),
                hist_staging,
                mode,
                out_w,
                out_h,
                frame: 0,
                hist_frame: 0,
            })
        }
    }

    fn make_srv_texture(
        dev: &ID3D11Device,
        w: u32,
        h: u32,
        fmt: DXGI_FORMAT,
        init: Option<&[u8]>,
    ) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
        unsafe {
            let d = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: fmt,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let data = init.map(|b| D3D11_SUBRESOURCE_DATA { pSysMem: b.as_ptr() as *const _, SysMemPitch: w * 4, SysMemSlicePitch: 0 });
            let mut t = None;
            dev.CreateTexture2D(&d, data.as_ref().map(|d| d as *const _), Some(&mut t))?;
            let t = t.unwrap();
            let mut srv = None;
            dev.CreateShaderResourceView(&t, None, Some(&mut srv))?;
            Ok((t, srv.unwrap()))
        }
    }

    pub fn has_histogram(&self) -> bool {
        self.cs_hist.is_some()
    }

    /// Texture to copy each new desktop frame into (same format/size).
    pub fn source(&self) -> &ID3D11Texture2D {
        &self.src
    }

    /// Fill the source with CPU pixels (self test / tests).
    pub fn upload_source(&self, data: &[u8], row_pitch: u32) {
        unsafe { self.ctx.UpdateSubresource(&self.src, 0, None, data.as_ptr() as *const _, row_pitch, 0) }
    }

    /// Upload a cursor image (straight-alpha RGBA). Skipped if unchanged.
    pub fn set_cursor(&mut self, handle: isize, w: u32, h: u32, rgba: &[u8]) -> Result<()> {
        if self.cursor.as_ref().map(|c| c.2) == Some(handle) {
            return Ok(());
        }
        let (t, srv) = Self::make_srv_texture(&self.dev, w, h, DXGI_FORMAT_R8G8B8A8_UNORM, Some(rgba))?;
        self.cursor = Some((t, srv, handle));
        Ok(())
    }

    /// Run histogram + convert for the current source. Returns the converted
    /// frame from RING-1 calls ago (GPU readback latency), if any.
    pub fn process(&mut self, p: &Params, want_hist: bool, out: &mut Vec<u8>) -> Result<Option<[u32; HIST_BINS + 1]>> {
        unsafe {
            let ctx = &self.ctx;
            ctx.UpdateSubresource(&self.cbuf, 0, None, p as *const _ as *const _, 0, 0);
            ctx.CSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            let cur_srv = if p.f[0] != 0 { self.cursor.as_ref().map(|c| c.1.clone()) } else { None };
            ctx.CSSetShaderResources(0, Some(&[Some(self.src_srv.clone()), Some(cur_srv.unwrap_or_else(|| self.dummy_srv.clone()))]));

            let slot = self.frame % RING;
            let mut hist_result = None;
            if let (true, Some(cs_hist)) = (want_hist, &self.cs_hist) {
                ctx.ClearUnorderedAccessViewUint(&self.hist_uav, &[0; 4]);
                let uavs = [None, None, None, Some(self.hist_uav.clone())];
                ctx.CSSetUnorderedAccessViews(0, 4, Some(uavs.as_ptr()), None);
                ctx.CSSetShader(cs_hist, None);
                let step = p.d[2].max(1.0) as u32;
                let gx = (self.out_w.div_ceil(step)).div_ceil(8);
                let gy = (self.out_h.div_ceil(step)).div_ceil(8);
                ctx.Dispatch(gx, gy, 1);
                ctx.CopyResource(&self.hist_staging[self.hist_frame % RING], &self.hist);
                self.hist_frame += 1;
                if self.hist_frame >= RING {
                    let st = &self.hist_staging[self.hist_frame % RING];
                    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                    ctx.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
                    let mut h = [0u32; HIST_BINS + 1];
                    h.copy_from_slice(std::slice::from_raw_parts(m.pData as *const u32, HIST_BINS + 1));
                    ctx.Unmap(st, 0);
                    hist_result = Some(h);
                }
            }

            let uavs: [Option<ID3D11UnorderedAccessView>; 4] = match self.mode {
                OutMode::Nv12 => [Some(self.planes[0].uav.clone()), Some(self.planes[1].uav.clone()), None, None],
                OutMode::Rgba => [None, None, Some(self.planes[0].uav.clone()), None],
            };
            ctx.CSSetUnorderedAccessViews(0, 4, Some(uavs.as_ptr()), None);
            ctx.CSSetShader(&self.cs_convert, None);
            let (gx, gy) = match self.mode {
                OutMode::Nv12 => ((self.out_w / 2).div_ceil(8), (self.out_h / 2).div_ceil(8)),
                OutMode::Rgba => (self.out_w.div_ceil(8), self.out_h.div_ceil(8)),
            };
            ctx.Dispatch(gx, gy, 1);
            let none: [Option<ID3D11UnorderedAccessView>; 4] = [None, None, None, None];
            ctx.CSSetUnorderedAccessViews(0, 4, Some(none.as_ptr()), None);
            ctx.CSSetShaderResources(0, Some(&[None, None]));

            for pl in &self.planes {
                ctx.CopyResource(&pl.staging[slot], &pl.tex);
            }
            self.frame += 1;
            if self.frame >= RING {
                self.read_slot(self.frame % RING, out)?;
                return Ok(hist_result);
            }
            Ok(hist_result)
        }
    }

    /// True if `process` produced a frame into `out` this call.
    pub fn has_output(&self) -> bool {
        self.frame >= RING
    }

    fn read_slot(&self, slot: usize, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        for pl in &self.planes {
            unsafe { pl.read(&self.ctx, slot, out)? };
        }
        Ok(())
    }

    /// Frames still in flight after the last `process` (call at the end).
    pub fn drain(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut v = Vec::new();
        let pending = self.frame.min(RING - 1);
        for i in (1..=pending).rev() {
            let slot = (self.frame - i) % RING;
            let mut out = Vec::new();
            self.read_slot(slot, &mut out)?;
            v.push(out);
        }
        Ok(v)
    }

    pub fn frame_bytes(&self) -> usize {
        match self.mode {
            OutMode::Nv12 => (self.out_w * self.out_h * 3 / 2) as usize,
            OutMode::Rgba => (self.out_w * self.out_h * 4) as usize,
        }
    }
}

/// Create a D3D11 device (hardware on the given adapter, or WARP for tests).
pub fn create_device(adapter: Option<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let mut dev = None;
        let mut ctx = None;
        let drv = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_WARP };
        D3D11CreateDevice(
            adapter,
            drv,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            Some(&mut ctx),
        )?;
        Ok((dev.unwrap(), ctx.unwrap()))
    }
}

/// `HDRSnap.exe --selftest-gpu`: run the shader on WARP (software D3D) with a
/// known HDR test image and compare against the CPU tone mapper.
pub fn selftest() -> std::result::Result<String, String> {
    use hdrsnap_core::tonemap::{self, f32_to_f16, Rolloff};
    let (dev, ctx) = create_device(None).map_err(|e| format!("WARP device: {e}"))?;
    let (w, h) = (64u32, 32u32);
    let scale = 3.0f32;
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let t = x as f32 / (w - 1) as f32;
            let v = match y % 4 {
                0 => [t, t, t],
                1 => [t * 5.0, t * 3.0, t * 0.5],
                2 => [-0.05, t, 0.2],
                _ => [t * 8.0, t * 8.0, t * 8.0],
            };
            for c in v {
                px.push(f32_to_f16(c * scale));
            }
            px.push(f32_to_f16(1.0));
        }
    }
    let bytes: Vec<u8> = px.iter().flat_map(|v| v.to_le_bytes()).collect();
    let curve = Rolloff::new(0.75, 4.0).unwrap();
    let (k, pk, b) = curve.params();
    let desat = 0.5;
    let params = Params {
        a: [0, 0, w as i32, h as i32],
        b: [w as i32, h as i32, 1, 1],
        c: [1.0 / scale, k, pk, b],
        d: [desat, 1.0, 4.0, 0.0],
        e: [0; 4],
        f: [0, 1, 0, 0],
    };
    let mut report = String::new();
    for mode in [OutMode::Rgba, OutMode::Nv12] {
        let mut conv =
            Converter::new(&dev, &ctx, DXGI_FORMAT_R16G16B16A16_FLOAT, w, h, w, h, mode).map_err(|e| format!("converter: {e}"))?;
        conv.upload_source(&bytes, w * 8);
        let mut p = params;
        p.f[1] = if mode == OutMode::Rgba { 1 } else { 0 };
        let mut out = Vec::new();
        for _ in 0..RING {
            conv.process(&p, true, &mut out).map_err(|e| format!("process: {e}"))?;
        }
        if out.len() != conv.frame_bytes() {
            return Err(format!("{mode:?}: got {} bytes, expected {}", out.len(), conv.frame_bytes()));
        }
        let mut max_err = 0i32;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = (y * w as usize + x) * 4;
                let f = |c: usize| tonemap::f16_to_f32(px[i + c]) / scale;
                let lin = tonemap::map_linear(f(0), f(1), f(2), Some(&curve), desat);
                let cpu: Vec<f32> = lin.iter().map(|v| tonemap::srgb_oetf(*v)).collect();
                match mode {
                    OutMode::Rgba => {
                        for c in 0..3 {
                            let g = out[i + c] as i32;
                            let e = (g - (cpu[c] * 255.0).round() as i32).abs();
                            max_err = max_err.max(e);
                        }
                    }
                    OutMode::Nv12 => {
                        let yv = 16.0 + 219.0 * (0.2126 * cpu[0] + 0.7152 * cpu[1] + 0.0722 * cpu[2]);
                        let g = out[y * w as usize + x] as i32;
                        max_err = max_err.max((g - yv.round() as i32).abs());
                    }
                }
            }
        }
        report += &format!("{mode:?}: max error {max_err} code values\n");
        if mode == OutMode::Rgba {
            report += &format!(
                "Histogram (adaptive HDR roll-off in recordings): {}\n",
                if conv.has_histogram() { "available" } else { "NOT available, roll-off disabled" }
            );
        }
        if max_err > 2 {
            return Err(report);
        }
    }
    Ok(report)
}
