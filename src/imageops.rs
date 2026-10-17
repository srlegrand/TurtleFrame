//! Plain RGB image type and the small image operations TurtleFrame needs.

use std::path::Path;
use eframe::egui;

/// Longest side of in-memory previews (live view, onion skins, playback).
pub const PREVIEW_MAX: usize = 1280;
/// Longest side of timeline thumbnails.
pub const THUMB_MAX: usize = 160;

#[derive(Clone)]
pub struct RgbImage {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

impl RgbImage {
    pub fn new(w: usize, h: usize) -> Self {
        Self { w, h, data: vec![0; w * h * 3] }
    }

    pub fn to_color_image(&self) -> egui::ColorImage {
        egui::ColorImage::from_rgb([self.w, self.h], &self.data)
    }
}

/// Integer box downscale so the longest side is <= max_dim. Cheap and alias free.
pub fn box_downscale(img: &RgbImage, max_dim: usize) -> RgbImage {
    let f = img.w.max(img.h).div_ceil(max_dim).max(1);
    if f == 1 {
        return img.clone();
    }
    let ow = (img.w / f).max(1);
    let oh = (img.h / f).max(1);
    let mut out = RgbImage::new(ow, oh);
    let n = (f * f) as u32;
    for oy in 0..oh {
        for ox in 0..ow {
            let mut s = [0u32; 3];
            for dy in 0..f {
                let row = (oy * f + dy) * img.w;
                for dx in 0..f {
                    let i = (row + ox * f + dx) * 3;
                    s[0] += img.data[i] as u32;
                    s[1] += img.data[i + 1] as u32;
                    s[2] += img.data[i + 2] as u32;
                }
            }
            let o = (oy * ow + ox) * 3;
            out.data[o] = (s[0] / n) as u8;
            out.data[o + 1] = (s[1] / n) as u8;
            out.data[o + 2] = (s[2] / n) as u8;
        }
    }
    out
}

/// Per-pixel mean of several same-sized frames (noise reduction for captures).
pub fn average(frames: &[RgbImage]) -> Option<RgbImage> {
    let first = frames.first()?;
    if frames.len() == 1 {
        return Some(first.clone());
    }
    let same: Vec<&RgbImage> = frames
        .iter()
        .filter(|f| f.w == first.w && f.h == first.h)
        .collect();
    let n = same.len() as u32;
    let mut acc = vec![0u32; first.data.len()];
    for f in &same {
        for (a, &v) in acc.iter_mut().zip(f.data.iter()) {
            *a += v as u32;
        }
    }
    let data = acc.iter().map(|&a| ((a + n / 2) / n) as u8).collect();
    Some(RgbImage { w: first.w, h: first.h, data })
}

pub fn luminance(img: &RgbImage) -> Vec<u8> {
    img.data
        .chunks_exact(3)
        .map(|p| ((p[0] as u32 * 77 + p[1] as u32 * 150 + p[2] as u32 * 29) >> 8) as u8)
        .collect()
}

/// Box blur of a single channel via an integral image.
fn box_blur(src: &[u8], w: usize, h: usize, r: usize) -> Vec<f32> {
    let mut integ = vec![0u64; (w + 1) * (h + 1)];
    for y in 0..h {
        let mut row = 0u64;
        for x in 0..w {
            row += src[y * w + x] as u64;
            integ[(y + 1) * (w + 1) + x + 1] = integ[y * (w + 1) + x + 1] + row;
        }
    }
    let mut out = vec![0f32; w * h];
    for y in 0..h {
        let y0 = y.saturating_sub(r);
        let y1 = (y + r + 1).min(h);
        for x in 0..w {
            let x0 = x.saturating_sub(r);
            let x1 = (x + r + 1).min(w);
            let sum = integ[y1 * (w + 1) + x1] + integ[y0 * (w + 1) + x0]
                - integ[y0 * (w + 1) + x1]
                - integ[y1 * (w + 1) + x0];
            out[y * w + x] = sum as f32 / ((x1 - x0) * (y1 - y0)) as f32;
        }
    }
    out
}

/// Alpha mask of dark pencil lines on light paper.
/// A pixel counts as line when darker than `threshold` x its local surroundings, so
/// thin strokes show while large dark areas (table, shadows) stay clear.
pub fn mask_lines(img: &RgbImage, threshold: f32) -> Vec<u8> {
    let lum = luminance(img);
    let r = (img.w.max(img.h) / 60).max(4);
    let bg = box_blur(&lum, img.w, img.h, r);
    lum.iter()
        .zip(bg.iter())
        .map(|(&l, &b)| {
            let b = b.max(24.0);
            let cut = b * threshold;
            let soft = (b * 0.15).max(4.0);
            (((cut - l as f32) / soft).clamp(0.0, 1.0) * 255.0) as u8
        })
        .collect()
}

/// Alpha mask from a Sobel edge detect. `sensitivity` 0..1.
pub fn mask_edges(img: &RgbImage, sensitivity: f32) -> Vec<u8> {
    let (w, h) = (img.w, img.h);
    let lum = luminance(img);
    let mut out = vec![0u8; w * h];
    if w < 3 || h < 3 {
        return out;
    }
    let t0 = (1.0 - sensitivity.clamp(0.0, 1.0)) * 300.0 + 20.0;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let p = |dx: isize, dy: isize| -> f32 {
                lum[((y as isize + dy) as usize) * w + (x as isize + dx) as usize] as f32
            };
            let gx = p(1, -1) + 2.0 * p(1, 0) + p(1, 1) - p(-1, -1) - 2.0 * p(-1, 0) - p(-1, 1);
            let gy = p(-1, 1) + 2.0 * p(0, 1) + p(1, 1) - p(-1, -1) - 2.0 * p(0, -1) - p(1, -1);
            let m = (gx * gx + gy * gy).sqrt();
            out[y * w + x] = (((m - t0) / t0).clamp(0.0, 1.0) * 255.0) as u8;
        }
    }
    out
}

/// White texture whose alpha is the mask; tinted red/green when drawn.
pub fn alpha_to_color_image(w: usize, h: usize, alpha: &[u8]) -> egui::ColorImage {
    let pixels = alpha
        .iter()
        .map(|&a| egui::Color32::from_rgba_premultiplied(a, a, a, a))
        .collect();
    egui::ColorImage::new([w, h], pixels)
}

/// Whole-image ghost: darkness becomes alpha, so paper stays clear and everything
/// drawn or placed on it shows through, tinted.
pub fn mask_image(img: &RgbImage) -> Vec<u8> {
    luminance(img).into_iter().map(|l| 255 - l).collect()
}

pub fn load_rgb(path: &Path) -> Result<RgbImage, String> {
    let img = image::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    Ok(RgbImage { w: w as usize, h: h as usize, data: rgb.into_raw() })
}

pub fn image_dims(path: &Path) -> Result<(usize, usize), String> {
    image::image_dimensions(path)
        .map(|(w, h)| (w as usize, h as usize))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Lossless PNG, fast compression. Written to a temp name then renamed.
pub fn save_png(path: &Path, img: &RgbImage) -> Result<(), String> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::ImageEncoder;
    let tmp = path.with_extension("png.tmp");
    let file = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let w = std::io::BufWriter::new(file);
    let enc = PngEncoder::new_with_quality(w, CompressionType::Fast, FilterType::Adaptive);
    enc.write_image(&img.data, img.w as u32, img.h as u32, image::ExtendedColorType::Rgb8)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn save_jpeg(path: &Path, img: &RgbImage, quality: u8) -> Result<(), String> {
    use image::codecs::jpeg::JpegEncoder;
    use image::ImageEncoder;
    let tmp = path.with_extension("jpg.tmp");
    let file = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let w = std::io::BufWriter::new(file);
    JpegEncoder::new_with_quality(w, quality)
        .write_image(&img.data, img.w as u32, img.h as u32, image::ExtendedColorType::Rgb8)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Run `f(row_index, row_slice)` over all rows of `out` on every core.
pub fn par_rows<T: Send, F>(out: &mut [T], row_len: usize, f: F)
where
    F: Fn(usize, &mut [T]) + Sync,
{
    if row_len == 0 || out.is_empty() {
        return;
    }
    let rows = out.len() / row_len;
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
    let rows_per = rows.div_ceil(threads).max(1);
    std::thread::scope(|s| {
        for (ci, chunk) in out.chunks_mut(rows_per * row_len).enumerate() {
            let f = &f;
            s.spawn(move || {
                for (ri, row) in chunk.chunks_mut(row_len).enumerate() {
                    f(ci * rows_per + ri, row);
                }
            });
        }
    });
}
