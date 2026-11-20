//! HDR (scRGB, FP16) -> SDR (sRGB, 8-bit) conversion.
//!
//! On an HDR monitor Windows composes the desktop in scRGB: linear light,
//! BT.709 primaries, where 1.0 = 80 nits. SDR content (the desktop, browsers,
//! most apps) is placed at the user's "SDR content brightness" level, so plain
//! SDR white ends up at e.g. 3.0 (240 nits) instead of 1.0. Tools that ignore
//! that either clip everything (overblown) or treat 80 nits as white
//! (washed out / wrong). The pipeline here:
//!
//! 1. Divide by the monitor's SDR white scale, so SDR white == 1.0 again.
//!    For pure SDR content this is an exact inverse of what DWM did, so the
//!    capture is identical to the same screen with HDR turned off.
//! 2. Look at the frame. If there is meaningful content brighter than SDR
//!    white (an HDR game or video), compress highlights with a smooth
//!    roll-off curve so they keep detail instead of clipping. If not, skip it
//!    entirely, so desktop screenshots are not dimmed at all.
//! 3. Bring out-of-gamut colours (negative scRGB values = wider than sRGB)
//!    back into sRGB by desaturating towards the pixel's own luminance,
//!    which keeps brightness and hue instead of clipping channels.
//! 4. sRGB encode to 8 bits.

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ToneMapMode {
    /// Roll off highlights only when the frame has real HDR content.
    Auto,
    /// Never roll off: anything above SDR white clips.
    Clip,
}

#[derive(Clone, Copy, Debug)]
pub struct HdrParams {
    /// SDR white in scRGB units (Windows SDRWhiteLevel / 1000). 1.0 = 80 nits.
    pub sdr_white_scale: f32,
    /// Display peak luminance in nits, if known (0 = unknown).
    pub display_peak_nits: f32,
    pub mode: ToneMapMode,
    /// Where the roll-off starts, relative to SDR white (0.5..0.98).
    /// Lower = more highlight detail, but midtones/whites get dimmer.
    pub knee: f32,
    /// How much the brightest highlights drift towards white (0..1).
    pub highlight_desaturation: f32,
}

impl Default for HdrParams {
    fn default() -> Self {
        Self { sdr_white_scale: 1.0, display_peak_nits: 0.0, mode: ToneMapMode::Auto, knee: 0.75, highlight_desaturation: 0.5 }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ToneMapReport {
    /// Fraction of pixels brighter than SDR white.
    pub hdr_pixel_fraction: f32,
    /// Brightest value used for the curve, relative to SDR white.
    pub source_peak: f32,
    /// Whether the highlight roll-off was applied.
    pub rolloff_applied: bool,
}

/// Pixels this much above SDR white count as "HDR". The small margin
/// absorbs FP16 rounding of exact SDR white.
pub const HDR_THRESHOLD: f32 = 1.02;
/// Need at least this fraction of HDR pixels before we touch the curve
/// (a stray bright cursor or icon should not dim the whole screenshot).
pub const MIN_HDR_FRACTION: f32 = 0.0005;
/// Peak = this percentile of all pixels (ignores a handful of extreme pixels).
const PEAK_PERCENTILE: f64 = 0.9995;

const HIST_BINS: usize = 512;
const HIST_MAX_LOG2: f32 = 8.0; // bins cover 1.0 .. 256x SDR white

// ---------------------------------------------------------------------------
// Lookup tables

fn f16_lut() -> &'static [f32] {
    static LUT: OnceLock<Vec<f32>> = OnceLock::new();
    LUT.get_or_init(|| (0..=u16::MAX).map(f16_to_f32).collect())
}

pub fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exp = ((h >> 10) & 0x1f) as u32;
    let mant = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // subnormal
            let mut e: i32 = -14;
            let mut m = mant;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x3ff;
            sign | (((e + 127) as u32) << 23) | (m << 13)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        sign | ((exp + 112) << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}

/// Round-to-nearest-even f32 -> f16 (used by tests and the EXR writer).
pub fn f32_to_f16(v: f32) -> u16 {
    let x = v.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let exp = ((x >> 23) & 0xff) as i32;
    let mant = x & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if mant != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = mant | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = 1u32 << (shift - 1);
        let rem = m & ((1 << shift) - 1);
        let mut r = m >> shift;
        if rem > half || (rem == half && (r & 1) == 1) {
            r += 1;
        }
        return sign | r as u16;
    }
    let mut r = ((e as u32) << 10) | (mant >> 13);
    let rem = mant & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && (r & 1) == 1) {
        r += 1;
    }
    sign | r as u16
}

const SRGB_LUT_SIZE: usize = 65536;

fn srgb_encode_lut() -> &'static [u8] {
    static LUT: OnceLock<Vec<u8>> = OnceLock::new();
    LUT.get_or_init(|| {
        (0..SRGB_LUT_SIZE)
            .map(|i| {
                let v = i as f32 / (SRGB_LUT_SIZE - 1) as f32;
                (srgb_oetf(v) * 255.0 + 0.5).floor().clamp(0.0, 255.0) as u8
            })
            .collect()
    })
}

pub fn srgb_oetf(v: f32) -> f32 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

pub fn srgb_eotf(v: f32) -> f32 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

#[inline]
fn encode(lut: &[u8], v: f32) -> u8 {
    // v is already clamped to [0, 1]
    lut[(v * (SRGB_LUT_SIZE - 1) as f32 + 0.5) as usize]
}

// ---------------------------------------------------------------------------
// Curve

/// Highlight roll-off in linear light, applied to max(R,G,B).
/// Identity below `knee`; above it a rational shoulder that leaves the knee
/// with slope 1 (no visible kink) and reaches exactly 1.0 at `peak`.
#[derive(Clone, Copy, Debug)]
pub struct Rolloff {
    knee: f32,
    peak: f32,
    b: f32,
}

impl Rolloff {
    pub fn new(knee: f32, peak: f32) -> Option<Self> {
        let knee = knee.clamp(0.1, 0.99);
        if peak <= 1.0 + 1e-4 {
            return None;
        }
        let s = (peak - knee) / (1.0 - knee); // slope needed so t-space slope is 1
        let b = 1.0 / (s - 1.0);
        Some(Self { knee, peak, b })
    }

    /// (knee, peak, b) for the GPU shader, which evaluates the same curve.
    pub fn params(&self) -> (f32, f32, f32) {
        (self.knee, self.peak, self.b)
    }

    #[inline]
    pub fn apply(&self, x: f32) -> f32 {
        if x <= self.knee {
            x
        } else if x >= self.peak {
            1.0
        } else {
            let t = (x - self.knee) / (self.peak - self.knee);
            self.knee + (1.0 - self.knee) * (t * (1.0 + self.b) / (t + self.b))
        }
    }
}

// ---------------------------------------------------------------------------
// Pixel ops

#[inline]
fn luminance(r: f32, g: f32, b: f32) -> f32 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Pull negative (out-of-sRGB-gamut) channels back in by desaturating toward
/// the pixel's luminance. Keeps luminance and hue.
#[inline]
fn gamut_fix(r: &mut f32, g: &mut f32, b: &mut f32) {
    let mn = r.min(*g).min(*b);
    if mn < 0.0 {
        let y = luminance(*r, *g, *b);
        if y <= 0.0 {
            *r = 0.0;
            *g = 0.0;
            *b = 0.0;
            return;
        }
        let t = y / (y - mn);
        *r = y + (*r - y) * t;
        *g = y + (*g - y) * t;
        *b = y + (*b - y) * t;
    }
}

/// One pixel: scRGB already divided by SDR white -> linear sRGB in [0, 1].
/// Gamut fix, optional highlight roll-off, clamp. (The GPU shader in
/// win/gpu.rs implements exactly the same steps.)
#[inline]
pub fn map_linear(mut r: f32, mut g: f32, mut b: f32, curve: Option<&Rolloff>, desat: f32) -> [f32; 3] {
    if !(r.is_finite() && g.is_finite() && b.is_finite()) {
        return [0.0; 3];
    }
    gamut_fix(&mut r, &mut g, &mut b);
    if let Some(c) = curve {
        let m = r.max(g).max(b);
        if m > c.knee {
            let m2 = c.apply(m);
            let k = m2 / m;
            r *= k;
            g *= k;
            b *= k;
            if m > 1.0 && desat > 0.0 {
                let w = desat * ((m - 1.0) / (c.peak - 1.0)).clamp(0.0, 1.0);
                r += (m2 - r) * w;
                g += (m2 - g) * w;
                b += (m2 - b) * w;
            }
        }
    }
    [r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0)]
}

// ---------------------------------------------------------------------------
// Public entry point

/// Tone map an scRGB FP16 RGBA image to packed 8-bit RGB.
///
/// * `src` - R16G16B16A16_FLOAT pixels, `src_pitch` u16 elements per row
/// * `dst` - RGB8, `dst_pitch` bytes per row
pub fn tonemap_scrgb(
    src: &[u16],
    src_pitch: usize,
    width: usize,
    height: usize,
    params: &HdrParams,
    dst: &mut [u8],
    dst_pitch: usize,
) -> ToneMapReport {
    let f16 = f16_lut();
    let enc = srgb_encode_lut();
    let inv_scale = 1.0 / params.sdr_white_scale.max(0.01);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(32);
    let rows_per = height.div_ceil(threads).max(1);

    // --- pass 1: histogram of brightness above SDR white -------------------
    let mut hist = vec![0u64; HIST_BINS];
    let mut hdr_count = 0u64;
    if params.mode == ToneMapMode::Auto {
        let results: Vec<(Vec<u64>, u64)> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..height)
                .step_by(rows_per)
                .map(|y0| {
                    let y1 = (y0 + rows_per).min(height);
                    s.spawn(move || {
                        let mut h = vec![0u64; HIST_BINS];
                        let mut c = 0u64;
                        for y in y0..y1 {
                            let row = &src[y * src_pitch..y * src_pitch + width * 4];
                            for px in row.chunks_exact(4) {
                                let m = f16[px[0] as usize].max(f16[px[1] as usize]).max(f16[px[2] as usize]) * inv_scale;
                                if m > HDR_THRESHOLD && m.is_finite() {
                                    c += 1;
                                    let bin = (m.log2() / HIST_MAX_LOG2 * HIST_BINS as f32) as usize;
                                    h[bin.min(HIST_BINS - 1)] += 1;
                                }
                            }
                        }
                        (h, c)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (h, c) in results {
            hdr_count += c;
            for (a, b) in hist.iter_mut().zip(h) {
                *a += b;
            }
        }
    }

    let total = (width * height).max(1) as u64;
    let hdr_fraction = hdr_count as f32 / total as f32;
    let mut report = ToneMapReport { hdr_pixel_fraction: hdr_fraction, source_peak: 1.0, rolloff_applied: false };

    let mut curve = None;
    if params.mode == ToneMapMode::Auto && hdr_fraction >= MIN_HDR_FRACTION {
        // Walk down from the top until we've passed (1 - percentile) of all pixels.
        let skip = ((1.0 - PEAK_PERCENTILE) * total as f64) as u64;
        let mut acc = 0u64;
        let mut peak_bin = 0usize;
        for i in (0..HIST_BINS).rev() {
            acc += hist[i];
            if acc > skip {
                peak_bin = i;
                break;
            }
        }
        let mut peak = 2f32.powf((peak_bin + 1) as f32 / HIST_BINS as f32 * HIST_MAX_LOG2);
        // The panel can't show more than its peak anyway.
        let disp_peak = params.display_peak_nits / (params.sdr_white_scale * 80.0);
        if disp_peak > 1.05 {
            peak = peak.min(disp_peak);
        }
        report.source_peak = peak;
        if peak > 1.05 {
            curve = Rolloff::new(params.knee, peak);
        }
    }
    report.rolloff_applied = curve.is_some();
    let desat = params.highlight_desaturation.clamp(0.0, 1.0);

    // --- pass 2: convert ----------------------------------------------------
    std::thread::scope(|s| {
        for (band_idx, dst_band) in dst.chunks_mut(rows_per * dst_pitch).enumerate() {
            let y0 = band_idx * rows_per;
            if y0 >= height {
                break;
            }
            let y1 = (y0 + rows_per).min(height);
            let curve = curve;
            s.spawn(move || {
                for y in y0..y1 {
                    let row = &src[y * src_pitch..y * src_pitch + width * 4];
                    let out = &mut dst_band[(y - y0) * dst_pitch..(y - y0) * dst_pitch + width * 3];
                    for (px, o) in row.chunks_exact(4).zip(out.chunks_exact_mut(3)) {
                        let [r, g, b] = map_linear(
                            f16[px[0] as usize] * inv_scale,
                            f16[px[1] as usize] * inv_scale,
                            f16[px[2] as usize] * inv_scale,
                            curve.as_ref(),
                            desat,
                        );
                        o[0] = encode(enc, r);
                        o[1] = encode(enc, g);
                        o[2] = encode(enc, b);
                    }
                }
            });
        }
    });

    report
}

/// Copy BGRA8 (SDR capture) into packed RGB8. No colour processing at all.
pub fn bgra_to_rgb(src: &[u8], src_pitch: usize, width: usize, height: usize, dst: &mut [u8], dst_pitch: usize) {
    for y in 0..height {
        let row = &src[y * src_pitch..y * src_pitch + width * 4];
        let out = &mut dst[y * dst_pitch..y * dst_pitch + width * 3];
        for (px, o) in row.chunks_exact(4).zip(out.chunks_exact_mut(3)) {
            o[0] = px[2];
            o[1] = px[1];
            o[2] = px[0];
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sdr_frame(codes: &[[u8; 3]], scale: f32) -> Vec<u16> {
        let mut v = Vec::new();
        for c in codes {
            for &ch in c {
                v.push(f32_to_f16(srgb_eotf(ch as f32 / 255.0) * scale));
            }
            v.push(f32_to_f16(1.0));
        }
        v
    }

    #[test]
    fn f16_roundtrip() {
        for h in 0..=0x7bffu16 {
            assert_eq!(f32_to_f16(f16_to_f32(h)), h, "h={h:#x}");
        }
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
    }

    /// The key property: SDR content shown on an HDR monitor must come back
    /// as the exact same 8-bit values, for every SDR brightness setting.
    #[test]
    fn sdr_content_roundtrips_exactly() {
        let mut codes = Vec::new();
        for v in 0..=255u8 {
            codes.push([v, v, v]);
            codes.push([v, 0, 255 - v]);
            codes.push([255, v, v / 2]);
        }
        let n = codes.len();
        // Windows SDR brightness slider runs 80..480 nits (scale 1.0..6.0)
        for level in (1000..=6000).step_by(250) {
            let scale = level as f32 / 1000.0;
            let src = make_sdr_frame(&codes, scale);
            let mut dst = vec![0u8; n * 3];
            let p = HdrParams { sdr_white_scale: scale, display_peak_nits: 1000.0, ..Default::default() };
            let rep = tonemap_scrgb(&src, n * 4, n, 1, &p, &mut dst, n * 3);
            assert!(!rep.rolloff_applied, "pure SDR frame must not be tone mapped");
            for (i, c) in codes.iter().enumerate() {
                assert_eq!(&dst[i * 3..i * 3 + 3], c, "scale {scale} code {c:?}");
            }
        }
    }

    #[test]
    fn hdr_highlights_roll_off_instead_of_clipping() {
        // 1000 px: half SDR mid grey, half a gradient of HDR highlights up to 4x SDR white
        let scale = 3.0; // 240 nit SDR white
        let w = 1000;
        let mut src = Vec::new();
        for i in 0..w {
            let v = if i < 500 { 0.2 } else { 1.0 + 3.0 * (i - 500) as f32 / 499.0 };
            for _ in 0..3 {
                src.push(f32_to_f16(v * scale));
            }
            src.push(f32_to_f16(1.0));
        }
        let mut dst = vec![0u8; w * 3];
        let p = HdrParams { sdr_white_scale: scale, display_peak_nits: 1000.0, ..Default::default() };
        let rep = tonemap_scrgb(&src, w * 4, w, 1, &p, &mut dst, w * 3);
        assert!(rep.rolloff_applied);
        assert!(rep.source_peak > 3.5 && rep.source_peak <= 4.2, "peak {}", rep.source_peak);
        // monotonic, and highlights are not all crushed to 255
        let vals: Vec<u8> = (500..w).map(|i| dst[i * 3]).collect();
        assert!(vals.windows(2).all(|w| w[1] >= w[0]));
        let distinct: std::collections::BTreeSet<u8> = vals.iter().copied().collect();
        assert!(distinct.len() > 10, "highlights should keep gradation, got {}", distinct.len());
        // midtones below the knee untouched
        assert_eq!(dst[0], (srgb_oetf(0.2) * 255.0).round() as u8);
    }

    #[test]
    fn clip_mode_and_negative_values() {
        let scale = 2.0;
        // out-of-gamut saturated green (negative red/blue in scRGB)
        let src = vec![f32_to_f16(-0.1 * scale), f32_to_f16(0.8 * scale), f32_to_f16(-0.05 * scale), 0x3c00];
        let mut dst = vec![0u8; 3];
        let p = HdrParams { sdr_white_scale: scale, mode: ToneMapMode::Clip, ..Default::default() };
        tonemap_scrgb(&src, 4, 1, 1, &p, &mut dst, 3);
        assert_eq!(dst[0], 0);
        assert!(dst[1] > 200);
    }

    #[test]
    fn rolloff_curve_properties() {
        let c = Rolloff::new(0.75, 4.0).unwrap();
        assert_eq!(c.apply(0.5), 0.5);
        assert!((c.apply(4.0) - 1.0).abs() < 1e-6);
        let mut prev = 0.0;
        for i in 0..=1000 {
            let x = 0.75 + 3.25 * i as f32 / 1000.0;
            let y = c.apply(x);
            assert!(y >= prev - 1e-6 && y <= 1.0 + 1e-6);
            prev = y;
        }
        // continuous slope at the knee
        let d = (c.apply(0.7501) - c.apply(0.75)) / 0.0001;
        assert!((d - 1.0).abs() < 0.05, "slope {d}");
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    /// cargo test --release --lib bench_5k -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_5k() {
        let (w, h) = (5120usize, 1440usize);
        let mut src = vec![0u16; w * h * 4];
        for (i, px) in src.chunks_exact_mut(4).enumerate() {
            let v = (i % 5000) as f32 / 1000.0; // 0..5x, lots of HDR pixels
            px[0] = f32_to_f16(v * 3.0);
            px[1] = f32_to_f16(v * 2.0);
            px[2] = f32_to_f16(v);
            px[3] = 0x3c00;
        }
        let mut dst = vec![0u8; w * h * 3];
        let p = HdrParams { sdr_white_scale: 3.0, display_peak_nits: 1000.0, ..Default::default() };
        tonemap_scrgb(&src, w * 4, w, h, &p, &mut dst, w * 3); // warm LUTs
        let t = std::time::Instant::now();
        let r = tonemap_scrgb(&src, w * 4, w, h, &p, &mut dst, w * 3);
        println!("5120x1440 tonemap: {:?} on {} threads, {:?}", t.elapsed(), std::thread::available_parallelism().unwrap(), r);
    }
}

#[cfg(test)]
mod visual {
    use super::*;
    /// Writes $TMP/hdrsnap_visual_{auto,clip}.png from a synthetic HDR frame.
    #[test]
    #[ignore]
    fn visual_check() {
        let (w, h) = (900usize, 300usize);
        let scale = 3.0;
        let mut src = vec![0u16; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = if x < 300 {
                    // SDR "UI": white bar, grey ramp, saturated swatches
                    let t = x as f32 / 300.0;
                    if y < 100 {
                        (1.0, 1.0, 1.0)
                    } else if y < 200 {
                        let v = srgb_eotf(t);
                        (v, v, v)
                    } else {
                        (srgb_eotf(t), srgb_eotf(0.3), srgb_eotf(1.0 - t))
                    }
                } else {
                    // HDR sky: gradient up to 6x SDR white with a bright sun disk
                    let t = (x - 300) as f32 / 600.0;
                    let dx = x as f32 - 750.0;
                    let dy = y as f32 - 150.0;
                    let sun = (-(dx * dx + dy * dy) / 800.0).exp() * 8.0;
                    let base = 0.2 + 3.0 * t * (1.0 - y as f32 / h as f32);
                    (base * 1.1 + sun, base * 0.9 + sun * 0.9, base * 0.6 + sun * 0.7)
                };
                let i = (y * w + x) * 4;
                src[i] = f32_to_f16(r * scale);
                src[i + 1] = f32_to_f16(g * scale);
                src[i + 2] = f32_to_f16(b * scale);
                src[i + 3] = 0x3c00;
            }
        }
        for (name, mode) in [("auto", ToneMapMode::Auto), ("clip", ToneMapMode::Clip)] {
            let mut img = crate::imageio::RgbImage::new(w, h);
            let p = HdrParams { sdr_white_scale: scale, display_peak_nits: 1000.0, mode, ..Default::default() };
            let r = tonemap_scrgb(&src, w * 4, w, h, &p, &mut img.data, w * 3);
            println!("{name}: {r:?}");
            crate::imageio::save_png(&img, &std::env::temp_dir().join(format!("hdrsnap_visual_{name}.png")), 6).unwrap();
        }
    }
}
