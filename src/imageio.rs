//! Image encoders: PNG, JPG (8-bit sRGB) and a minimal OpenEXR writer used
//! for the optional raw-HDR debug dump.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

/// Packed 8-bit RGB image.
#[derive(Clone)]
pub struct RgbImage {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl RgbImage {
    pub fn new(width: usize, height: usize) -> Self {
        Self { width, height, data: vec![0; width * height * 3] }
    }

    pub fn crop(&self, x: usize, y: usize, w: usize, h: usize) -> RgbImage {
        let mut out = RgbImage::new(w, h);
        for row in 0..h {
            let s = ((y + row) * self.width + x) * 3;
            out.data[row * w * 3..(row + 1) * w * 3].copy_from_slice(&self.data[s..s + w * 3]);
        }
        out
    }
}

pub fn save_png(img: &RgbImage, path: &Path, level: u8) -> std::io::Result<()> {
    let file = BufWriter::with_capacity(1 << 20, File::create(path)?);
    let mut enc = png::Encoder::new(file, img.width as u32, img.height as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    let level = level.min(9);
    if level == 0 {
        enc.set_deflate_compression(png::DeflateCompression::NoCompression);
        enc.set_filter(png::Filter::NoFilter);
    } else {
        enc.set_deflate_compression(png::DeflateCompression::Level(level));
        enc.set_filter(png::Filter::Adaptive);
    }
    let mut w = enc.write_header().map_err(std::io::Error::other)?;
    w.write_image_data(&img.data).map_err(std::io::Error::other)?;
    w.finish().map_err(std::io::Error::other)?;
    Ok(())
}

pub fn save_jpg(img: &RgbImage, path: &Path, quality: u8) -> std::io::Result<()> {
    let quality = quality.clamp(1, 100);
    let mut enc = jpeg_encoder::Encoder::new_file(path, quality).map_err(std::io::Error::other)?;
    // 4:4:4 keeps text and UI edges sharp; drop to 4:2:0 only at low quality.
    if quality >= 70 {
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_4_4);
    }
    enc.encode(&img.data, img.width as u16, img.height as u16, jpeg_encoder::ColorType::Rgb)
        .map_err(std::io::Error::other)
}

/// Write an uncompressed half-float RGBA OpenEXR file.
/// `pixels` is R16G16B16A16_FLOAT, `pitch` u16 elements per row.
pub fn save_exr_half(pixels: &[u16], pitch: usize, width: usize, height: usize, path: &Path) -> std::io::Result<()> {
    let mut f = BufWriter::with_capacity(1 << 20, File::create(path)?);
    let mut hdr: Vec<u8> = Vec::new();
    hdr.extend_from_slice(&0x0131_2f76u32.to_le_bytes()); // magic
    hdr.extend_from_slice(&2u32.to_le_bytes()); // version 2, scanline

    fn attr(h: &mut Vec<u8>, name: &str, ty: &str, data: &[u8]) {
        h.extend_from_slice(name.as_bytes());
        h.push(0);
        h.extend_from_slice(ty.as_bytes());
        h.push(0);
        h.extend_from_slice(&(data.len() as u32).to_le_bytes());
        h.extend_from_slice(data);
    }

    // channels, alphabetical: A B G R ; pixel type 1 = HALF
    let mut ch = Vec::new();
    for c in ["A", "B", "G", "R"] {
        ch.extend_from_slice(c.as_bytes());
        ch.push(0);
        ch.extend_from_slice(&1i32.to_le_bytes()); // HALF
        ch.extend_from_slice(&[0, 0, 0, 0]); // pLinear + reserved
        ch.extend_from_slice(&1i32.to_le_bytes()); // xSampling
        ch.extend_from_slice(&1i32.to_le_bytes()); // ySampling
    }
    ch.push(0);
    attr(&mut hdr, "channels", "chlist", &ch);
    attr(&mut hdr, "compression", "compression", &[0]);
    let mut b = Vec::new();
    for v in [0i32, 0, width as i32 - 1, height as i32 - 1] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    attr(&mut hdr, "dataWindow", "box2i", &b);
    attr(&mut hdr, "displayWindow", "box2i", &b);
    attr(&mut hdr, "lineOrder", "lineOrder", &[0]);
    attr(&mut hdr, "pixelAspectRatio", "float", &1f32.to_le_bytes());
    let mut c = Vec::new();
    c.extend_from_slice(&0f32.to_le_bytes());
    c.extend_from_slice(&0f32.to_le_bytes());
    attr(&mut hdr, "screenWindowCenter", "v2f", &c);
    attr(&mut hdr, "screenWindowWidth", "float", &1f32.to_le_bytes());
    hdr.push(0); // end of header

    let line_bytes = width * 4 * 2;
    let chunk = 8 + line_bytes;
    let table_start = hdr.len() as u64;
    let data_start = table_start + 8 * height as u64;
    f.write_all(&hdr)?;
    for y in 0..height as u64 {
        f.write_all(&(data_start + y * chunk as u64).to_le_bytes())?;
    }
    let mut line = vec![0u8; line_bytes];
    for y in 0..height {
        let row = &pixels[y * pitch..y * pitch + width * 4];
        // planar per channel: A, B, G, R
        for (plane, src_ch) in [3usize, 2, 1, 0].iter().enumerate() {
            let base = plane * width * 2;
            for x in 0..width {
                let v = row[x * 4 + src_ch];
                line[base + x * 2..base + x * 2 + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
        f.write_all(&(y as i32).to_le_bytes())?;
        f.write_all(&(line_bytes as i32).to_le_bytes())?;
        f.write_all(&line)?;
    }
    f.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_valid_files() {
        let dir = std::env::temp_dir().join("hdrsnap_test");
        std::fs::create_dir_all(&dir).unwrap();
        let mut img = RgbImage::new(64, 32);
        for (i, v) in img.data.iter_mut().enumerate() {
            *v = (i % 251) as u8;
        }
        for lvl in [0, 1, 6, 9] {
            save_png(&img, &dir.join(format!("t{lvl}.png")), lvl).unwrap();
        }
        save_jpg(&img, &dir.join("t.jpg"), 90).unwrap();
        save_jpg(&img, &dir.join("t50.jpg"), 50).unwrap();
        // round-trip the PNG
        let dec = png::Decoder::new(std::io::BufReader::new(File::open(dir.join("t9.png")).unwrap()));
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        r.next_frame(&mut buf).unwrap();
        assert_eq!(&buf[..img.data.len()], &img.data[..]);

        let px: Vec<u16> = (0..16 * 8 * 4).map(|i| crate::tonemap::f32_to_f16(i as f32 / 100.0)).collect();
        save_exr_half(&px, 16 * 4, 16, 8, &dir.join("t.exr")).unwrap();
    }
}
