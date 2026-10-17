//! Raw camera buffer -> RGB. Pure Rust, no C codecs.

use crate::imageops::RgbImage;
use nokhwa::utils::FrameFormat;

pub const ALL_FORMATS: &[FrameFormat] = &[
    FrameFormat::MJPEG,
    FrameFormat::YUYV,
    FrameFormat::NV12,
    FrameFormat::GRAY,
    FrameFormat::RAWRGB,
    FrameFormat::RAWBGR,
];

pub fn decode(fmt: FrameFormat, w: usize, h: usize, buf: &[u8]) -> Result<RgbImage, String> {
    if w == 0 || h == 0 {
        return Err("zero sized frame".into());
    }
    let px = w * h;
    match fmt {
        FrameFormat::MJPEG => decode_jpeg(buf),
        // Some backends mislabel NV12 as YUYV (and vice versa); trust the byte count.
        FrameFormat::YUYV | FrameFormat::NV12 => {
            if buf.len() >= px * 2 && buf.len() % h == 0 && fmt == FrameFormat::YUYV {
                Ok(yuyv(w, h, buf.len() / h, buf))
            } else if buf.len() >= px * 3 / 2 && buf.len() < px * 2 {
                Ok(nv12(w, h, buf))
            } else if buf.len() >= px * 2 && buf.len() % h == 0 {
                Ok(yuyv(w, h, buf.len() / h, buf))
            } else if buf.len() >= px * 3 / 2 {
                Ok(nv12(w, h, buf))
            } else {
                Err(format!("frame too small: {} bytes for {w}x{h}", buf.len()))
            }
        }
        FrameFormat::GRAY => {
            if buf.len() < px {
                return Err("short GRAY frame".into());
            }
            let mut out = RgbImage::new(w, h);
            for (o, &g) in out.data.chunks_exact_mut(3).zip(buf.iter()) {
                o.copy_from_slice(&[g, g, g]);
            }
            Ok(out)
        }
        FrameFormat::RAWRGB | FrameFormat::RAWBGR => {
            if buf.len() < px * 3 {
                return Err("short RGB frame".into());
            }
            let mut data = buf[..px * 3].to_vec();
            if fmt == FrameFormat::RAWBGR {
                for p in data.chunks_exact_mut(3) {
                    p.swap(0, 2);
                }
            }
            Ok(RgbImage { w, h, data })
        }
    }
}

#[inline]
fn yuv_to_rgb(y: i32, u: i32, v: i32) -> [u8; 3] {
    // BT.601 limited range.
    let c = (y - 16).max(0) * 298;
    let d = u - 128;
    let e = v - 128;
    let r = (c + 409 * e + 128) >> 8;
    let g = (c - 100 * d - 208 * e + 128) >> 8;
    let b = (c + 516 * d + 128) >> 8;
    [r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, b.clamp(0, 255) as u8]
}

fn yuyv(w: usize, h: usize, stride: usize, buf: &[u8]) -> RgbImage {
    let mut out = RgbImage::new(w, h);
    for y in 0..h {
        let row = &buf[y * stride..];
        for x2 in 0..w / 2 {
            let i = x2 * 4;
            let (y0, u, y1, v) = (row[i] as i32, row[i + 1] as i32, row[i + 2] as i32, row[i + 3] as i32);
            let o = (y * w + x2 * 2) * 3;
            out.data[o..o + 3].copy_from_slice(&yuv_to_rgb(y0, u, v));
            out.data[o + 3..o + 6].copy_from_slice(&yuv_to_rgb(y1, u, v));
        }
    }
    out
}

fn nv12(w: usize, h: usize, buf: &[u8]) -> RgbImage {
    let mut out = RgbImage::new(w, h);
    let uv = &buf[w * h..];
    for y in 0..h {
        for x in 0..w {
            let yy = buf[y * w + x] as i32;
            let ci = (y / 2) * w + (x & !1);
            let (u, v) = (uv[ci] as i32, uv[ci + 1] as i32);
            let o = (y * w + x) * 3;
            out.data[o..o + 3].copy_from_slice(&yuv_to_rgb(yy, u, v));
        }
    }
    out
}

fn decode_jpeg(buf: &[u8]) -> Result<RgbImage, String> {
    let try_decode = |b: &[u8]| -> Result<RgbImage, String> {
        let img = image::load_from_memory_with_format(b, image::ImageFormat::Jpeg)
            .map_err(|e| e.to_string())?
            .to_rgb8();
        let (w, h) = img.dimensions();
        Ok(RgbImage { w: w as usize, h: h as usize, data: img.into_raw() })
    };
    match try_decode(buf) {
        Ok(i) => Ok(i),
        Err(e) => {
            // Many UVC cameras send MJPEG without Huffman tables (DHT); add the standard ones.
            if let Some(fixed) = insert_default_dht(buf) {
                try_decode(&fixed)
            } else {
                Err(format!("MJPEG decode: {e}"))
            }
        }
    }
}

/// Returns a copy with the standard JPEG Annex K Huffman tables inserted before SOS,
/// or None if the stream already has a DHT or is not a JPEG.
pub fn insert_default_dht(buf: &[u8]) -> Option<Vec<u8>> {
    if buf.len() < 4 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 4 <= buf.len() {
        if buf[i] != 0xFF {
            return None;
        }
        let marker = buf[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if marker == 0xC4 {
            return None;
        }
        if marker == 0xDA {
            let mut out = Vec::with_capacity(buf.len() + 450);
            out.extend_from_slice(&buf[..i]);
            out.extend_from_slice(&default_dht_segment());
            out.extend_from_slice(&buf[i..]);
            return Some(out);
        }
        let len = ((buf[i + 2] as usize) << 8) | buf[i + 3] as usize;
        i += 2 + len;
    }
    None
}

const DC_LUM_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_CHR_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUM_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUM_VALS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
const AC_CHR_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHR_VALS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

fn default_dht_segment() -> Vec<u8> {
    let tables: [(u8, &[u8; 16], &[u8]); 4] = [
        (0x00, &DC_LUM_BITS, &DC_VALS),
        (0x10, &AC_LUM_BITS, &AC_LUM_VALS),
        (0x01, &DC_CHR_BITS, &DC_VALS),
        (0x11, &AC_CHR_BITS, &AC_CHR_VALS),
    ];
    let body_len: usize = tables.iter().map(|(_, _, v)| 1 + 16 + v.len()).sum();
    let len = body_len + 2;
    let mut seg = vec![0xFF, 0xC4, (len >> 8) as u8, (len & 0xFF) as u8];
    for (class_id, bits, vals) in tables {
        seg.push(class_id);
        seg.extend_from_slice(bits);
        seg.extend_from_slice(vals);
    }
    seg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_sizes_match() {
        let s = |b: &[u8; 16]| b.iter().map(|&x| x as usize).sum::<usize>();
        assert_eq!(s(&DC_LUM_BITS), DC_VALS.len());
        assert_eq!(s(&DC_CHR_BITS), DC_VALS.len());
        assert_eq!(s(&AC_LUM_BITS), AC_LUM_VALS.len());
        assert_eq!(s(&AC_CHR_BITS), AC_CHR_VALS.len());
    }

    /// Strip the DHT from an encoded JPEG, then confirm our default tables restore it.
    #[test]
    fn mjpeg_without_dht_decodes() {
        use image::ImageEncoder;
        let (w, h) = (64usize, 48usize);
        let mut src = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                src[i] = (x * 4) as u8;
                src[i + 1] = (y * 5) as u8;
                src[i + 2] = 128;
            }
        }
        let mut jpg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 90)
            .write_image(&src, w as u32, h as u32, image::ExtendedColorType::Rgb8)
            .unwrap();
        let reference = decode_jpeg(&jpg).unwrap();
        // remove every DHT segment
        let mut stripped = jpg[..2].to_vec();
        let mut i = 2;
        loop {
            let marker = jpg[i + 1];
            if marker == 0xDA {
                stripped.extend_from_slice(&jpg[i..]);
                break;
            }
            let len = ((jpg[i + 2] as usize) << 8) | jpg[i + 3] as usize;
            if marker != 0xC4 {
                stripped.extend_from_slice(&jpg[i..i + 2 + len]);
            }
            i += 2 + len;
        }
        assert!(insert_default_dht(&stripped).is_some());
        let fixed = decode_jpeg(&stripped).unwrap();
        assert_eq!((fixed.w, fixed.h), (w, h));
        assert_eq!(fixed.data, reference.data);
    }

    #[test]
    fn yuyv_grey() {
        let (w, h) = (4, 2);
        let buf: Vec<u8> = std::iter::repeat([126u8, 128, 126, 128]).take(w * h / 2).flatten().collect();
        let img = decode(FrameFormat::YUYV, w, h, &buf).unwrap();
        assert!(img.data.iter().all(|&v| (v as i32 - 128).abs() <= 2));
    }
}
