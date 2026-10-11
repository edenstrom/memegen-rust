//! AVIF encoding (static images only), with rav1e via ravif. AV1 is slow to
//! encode, so this trades some compression for a fast encoder speed.

use anyhow::{Result, anyhow};
use image::RgbaImage;
use ravif::{Encoder, Img, RGBA8};

const QUALITY: f32 = 80.0;
/// 1 (slowest, smallest) to 10 (fastest).
const SPEED: u8 = 10;

pub fn encode(image: &RgbaImage) -> Result<Vec<u8>> {
    let pixels: Vec<RGBA8> = image
        .as_raw()
        .chunks_exact(4)
        .map(|pixel| RGBA8::new(pixel[0], pixel[1], pixel[2], pixel[3]))
        .collect();
    let (width, height) = image.dimensions();
    let encoded = Encoder::new()
        .with_quality(QUALITY)
        .with_speed(SPEED)
        .encode_rgba(Img::new(pixels.as_slice(), width as usize, height as usize))
        .map_err(|error| anyhow!("AVIF encoding failed: {error}"))?;
    Ok(encoded.avif_file)
}
