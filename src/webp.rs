//! WebP encoding. Each frame of an animation is encoded on its own, in
//! parallel, as just the area that changed since the previous frame, and the
//! frames are muxed here. Natively frames are lossy (libwebp); on
//! WebAssembly, where libwebp doesn't build, they're lossless (image-webp), so
//! files are larger.

use anyhow::{Context, Result, ensure};
use image::RgbaImage;
use rayon::prelude::*;

/// VP8X flags: the file has alpha and animation.
const ALPHA_AND_ANIMATION: u8 = 0x10 | 0x02;
/// ANMF flags: don't alpha-blend with the previous frame, don't dispose it.
const NO_BLEND: u8 = 0b10;
#[cfg(not(target_arch = "wasm32"))]
const QUALITY: f32 = 75.0;
/// Method 2 encodes ~2x faster than the default (4) for ~3% larger files.
#[cfg(not(target_arch = "wasm32"))]
const METHOD: i32 = 2;

/// `delays` are in milliseconds, one per frame.
pub fn encode(frames: &[RgbaImage], delays: &[u32]) -> Result<Vec<u8>> {
    let first = frames.first().context("no frames")?;
    if frames.len() == 1 {
        return encode_still(first);
    }
    let encoded: Vec<(Rect, Vec<u8>)> = frames
        .par_iter()
        .enumerate()
        .map(|(index, frame)| {
            if index == 0 {
                return Ok((Rect::full(frame), encode_still(frame)?));
            }
            let rect = Rect::changed(&frames[index - 1], frame);
            let area = image::imageops::crop_imm(frame, rect.x, rect.y, rect.width, rect.height);
            Ok((rect, encode_still(&area.to_image())?))
        })
        .collect::<Result<_>>()?;

    let mut body = b"WEBP".to_vec();
    let mut vp8x = vec![ALPHA_AND_ANIMATION, 0, 0, 0];
    vp8x.extend(u24(first.width() - 1));
    vp8x.extend(u24(first.height() - 1));
    write_chunk(&mut body, b"VP8X", &vp8x);
    // Transparent background, loop forever.
    write_chunk(&mut body, b"ANIM", &[0, 0, 0, 0, 0, 0]);
    for ((rect, file), delay) in encoded.iter().zip(delays) {
        let data = frame_data(file)?;
        let mut anmf = Vec::with_capacity(data.len() + 16);
        anmf.extend(u24(rect.x / 2));
        anmf.extend(u24(rect.y / 2));
        anmf.extend(u24(rect.width - 1));
        anmf.extend(u24(rect.height - 1));
        anmf.extend(u24(*delay));
        anmf.push(NO_BLEND);
        anmf.extend_from_slice(data);
        write_chunk(&mut body, b"ANMF", &anmf);
    }

    let mut out = b"RIFF".to_vec();
    out.extend(u32::try_from(body.len())?.to_le_bytes());
    out.extend(body);
    Ok(out)
}

/// The area of an animation frame to encode. Offsets in a WebP animation
/// must be even.
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Rect {
    fn full(frame: &RgbaImage) -> Self {
        Self {
            x: 0,
            y: 0,
            width: frame.width(),
            height: frame.height(),
        }
    }

    /// The pixels of `current` that differ from `previous`, or a single pixel
    /// when none do.
    fn changed(previous: &RgbaImage, current: &RgbaImage) -> Self {
        let row = current.width() as usize * 4;
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
        let rows = previous
            .as_raw()
            .chunks_exact(row)
            .zip(current.as_raw().chunks_exact(row));
        for (y, (a, b)) in (0..).zip(rows) {
            if a == b {
                continue;
            }
            let pixels = || a.chunks_exact(4).zip(b.chunks_exact(4));
            let first = pixels().position(|(a, b)| a != b).unwrap_or(0) as u32;
            let last = pixels().rposition(|(a, b)| a != b).unwrap_or(0) as u32;
            x0 = x0.min(first);
            x1 = x1.max(last);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        if x0 == u32::MAX {
            (x0, y0, x1, y1) = (0, 0, 0, 0);
        }
        let (x, y) = (x0 & !1, y0 & !1);
        Self {
            x,
            y,
            width: x1 + 1 - x,
            height: y1 + 1 - y,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn encode_still(frame: &RgbaImage) -> Result<Vec<u8>> {
    let mut config =
        ::webp::WebPConfig::new().map_err(|_| anyhow::anyhow!("invalid WebP config"))?;
    config.quality = QUALITY;
    config.method = METHOD;
    let encoded = ::webp::Encoder::from_rgba(frame.as_raw(), frame.width(), frame.height())
        .encode_advanced(&config)
        .map_err(|error| anyhow::anyhow!("WebP encoding failed: {error:?}"))?;
    Ok(encoded.to_vec())
}

#[cfg(target_arch = "wasm32")]
fn encode_still(frame: &RgbaImage) -> Result<Vec<u8>> {
    use image_webp::{ColorType, WebPEncoder};
    let mut out = Vec::new();
    WebPEncoder::new(&mut out).encode(
        frame.as_raw(),
        frame.width(),
        frame.height(),
        ColorType::Rgba8,
    )?;
    Ok(out)
}

/// The image data of a single-frame file, for an `ANMF` chunk: an optional
/// `ALPH` chunk followed by a `VP8 ` or `VP8L` chunk.
fn frame_data(file: &[u8]) -> Result<&[u8]> {
    ensure!(
        file.get(..4) == Some(b"RIFF") && file.get(8..12) == Some(b"WEBP"),
        "not a WebP file"
    );
    let mut data = &file[12..];
    if data.starts_with(b"VP8X") {
        // Fourcc, size and a 10-byte payload.
        data = data.get(18..).context("truncated WebP")?;
    }
    ensure!(
        [b"ALPH", b"VP8 ", b"VP8L"]
            .iter()
            .any(|fourcc| data.starts_with(*fourcc)),
        "unexpected WebP chunk"
    );
    Ok(data)
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

    /// Frames are lossy natively, so compare channels with some slack.
    fn assert_close(decoded: &RgbaImage, original: &RgbaImage) {
        assert_eq!(decoded.dimensions(), original.dimensions());
        for (a, b) in decoded.pixels().zip(original.pixels()) {
            // The color of a fully transparent pixel isn't kept.
            if a[3] == 0 && b[3] == 0 {
                continue;
            }
            for (x, y) in a.0.iter().zip(b.0) {
                assert!(x.abs_diff(y) <= 24, "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn encodes_animation() {
        let mut frames: Vec<RgbaImage> = (0..3u8)
            .map(|index| RgbaImage::from_pixel(16, 16, Rgba([index * 80, 0, 0, 255])))
            .collect();
        // A frame with transparency is muxed from a VP8X file with an ALPH chunk.
        for y in 0..8 {
            for x in 0..16 {
                frames[1].put_pixel(x, y, Rgba([0, 0, 0, 0]));
            }
        }
        // Only an area at odd offsets changes, then nothing does.
        let mut partial = frames[2].clone();
        for y in 5..9 {
            for x in 7..12 {
                partial.put_pixel(x, y, Rgba([0, 200, 0, 255]));
            }
        }
        frames.extend([partial.clone(), partial]);
        let delays = [120, 120, 120, 120, 3000];
        let bytes = encode(&frames, &delays).unwrap();
        let decoder = WebPDecoder::new(Cursor::new(bytes)).unwrap();
        assert!(decoder.has_animation());
        let decoded = decoder.into_frames().collect_frames().unwrap();
        assert_eq!(decoded.len(), 5);
        for (frame, delay) in decoded.iter().zip(delays) {
            assert_eq!(frame.delay().numer_denom_ms(), (delay, 1));
        }
        for (frame, original) in decoded.iter().zip(&frames[..3]) {
            assert_close(frame.buffer(), original);
        }
        // Lossy chroma bleeds across the block's edges, so check inside and
        // well outside it.
        for frame in &decoded[3..] {
            for (x, y) in [(9, 7), (2, 2), (14, 14), (14, 2)] {
                let (a, b) = (frame.buffer().get_pixel(x, y), frames[3].get_pixel(x, y));
                assert!(
                    a.0.iter().zip(b.0).all(|(a, b)| a.abs_diff(b) <= 24),
                    "{a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn encodes_still() {
        let frame = RgbaImage::from_pixel(7, 4, Rgba([1, 2, 3, 255]));
        let bytes = encode(std::slice::from_ref(&frame), &[100]).unwrap();
        let image = image::load_from_memory(&bytes).unwrap().into_rgba8();
        assert_close(&image, &frame);
    }
}
