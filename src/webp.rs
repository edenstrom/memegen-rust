//! Pure-Rust WebP encoding for targets without libwebp (Cloudflare Workers).
//! Frames are lossless, so files are larger than libwebp's lossy output.

use anyhow::{Context, Result, ensure};
use image::RgbaImage;
use image_webp::{ColorType, WebPEncoder};
use rayon::prelude::*;

/// VP8X flags: the file has alpha and animation.
const ALPHA_AND_ANIMATION: u8 = 0x10 | 0x02;
/// ANMF flags: don't alpha-blend with the previous frame, don't dispose it.
const NO_BLEND: u8 = 0b10;

pub fn encode(frames: &[RgbaImage], duration: u32) -> Result<Vec<u8>> {
    let first = frames.first().context("no frames")?;
    if frames.len() == 1 {
        return encode_still(first);
    }
    let encoded: Vec<Vec<u8>> = frames.par_iter().map(encode_still).collect::<Result<_>>()?;

    let mut body = b"WEBP".to_vec();
    let mut vp8x = vec![ALPHA_AND_ANIMATION, 0, 0, 0];
    vp8x.extend(u24(first.width() - 1));
    vp8x.extend(u24(first.height() - 1));
    write_chunk(&mut body, b"VP8X", &vp8x);
    // Transparent background, loop forever.
    write_chunk(&mut body, b"ANIM", &[0, 0, 0, 0, 0, 0]);
    for (frame, file) in frames.iter().zip(&encoded) {
        let mut anmf = Vec::with_capacity(file.len() + 16);
        anmf.extend(u24(0)); // x offset / 2
        anmf.extend(u24(0)); // y offset / 2
        anmf.extend(u24(frame.width() - 1));
        anmf.extend(u24(frame.height() - 1));
        anmf.extend(u24(duration));
        anmf.push(NO_BLEND);
        anmf.extend_from_slice(vp8l_chunk(file)?);
        write_chunk(&mut body, b"ANMF", &anmf);
    }

    let mut out = b"RIFF".to_vec();
    out.extend(u32::try_from(body.len())?.to_le_bytes());
    out.extend(body);
    Ok(out)
}

fn encode_still(frame: &RgbaImage) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    WebPEncoder::new(&mut out).encode(
        frame.as_raw(),
        frame.width(),
        frame.height(),
        ColorType::Rgba8,
    )?;
    Ok(out)
}

/// The `VP8L` chunk (header, payload and padding) of a simple-format file.
fn vp8l_chunk(file: &[u8]) -> Result<&[u8]> {
    let chunk = file.get(12..).context("truncated WebP")?;
    ensure!(chunk.starts_with(b"VP8L"), "expected a lossless WebP");
    Ok(chunk)
}

fn write_chunk(out: &mut Vec<u8>, fourcc: &[u8; 4], payload: &[u8]) {
    out.extend(fourcc);
    out.extend((payload.len() as u32).to_le_bytes());
    out.extend(payload);
    if payload.len() % 2 == 1 {
        out.push(0);
    }
}

fn u24(value: u32) -> [u8; 3] {
    let [a, b, c, _] = value.to_le_bytes();
    [a, b, c]
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::codecs::webp::WebPDecoder;
    use image::{AnimationDecoder, Rgba};

    use super::*;

    #[test]
    fn encodes_animation() {
        let frames: Vec<RgbaImage> = (0..3u8)
            .map(|index| RgbaImage::from_pixel(5, 3, Rgba([index * 80, 0, 0, 255])))
            .collect();
        let bytes = encode(&frames, 120).unwrap();
        let decoder = WebPDecoder::new(Cursor::new(bytes)).unwrap();
        assert!(decoder.has_animation());
        let decoded = decoder.into_frames().collect_frames().unwrap();
        assert_eq!(decoded.len(), 3);
        for (frame, original) in decoded.iter().zip(&frames) {
            assert_eq!(frame.delay().numer_denom_ms(), (120, 1));
            assert_eq!(frame.buffer(), original);
        }
    }

    #[test]
    fn encodes_still() {
        let frame = RgbaImage::from_pixel(7, 4, Rgba([1, 2, 3, 255]));
        let bytes = encode(std::slice::from_ref(&frame), 100).unwrap();
        let image = image::load_from_memory(&bytes).unwrap().into_rgba8();
        assert_eq!(image, frame);
    }
}
