//! AVIF encoding with rav1e. A single frame is a still image (via ravif, with
//! alpha); more frames are an AV1 image sequence in a container written by
//! [`crate::mp4`], which plays in an `<img>` like a GIF. AV1 is slow to
//! encode, so this uses rav1e's fastest speed.

use anyhow::{Context as _, Result, anyhow, ensure};
use image::RgbaImage;
use rav1e::prelude::*;
use ravif::{Img, RGBA8};
use rayon::prelude::*;

use crate::mp4::{self, Container, write_box, write_colr, write_full_box};

/// ravif's quality (0-100) for stills.
const QUALITY: f32 = 80.0;
/// rav1e's quantizer (0-255) for sequences; its default.
const QUANTIZER: usize = 100;
/// 0 (slowest, smallest) to 10 (fastest).
const SPEED: u8 = 10;
/// The OBU type of a temporal delimiter, in its header byte.
const OBU_TEMPORAL_DELIMITER: u8 = 2 << 3;

/// `delays` are in milliseconds, one per frame. Animations ignore alpha, as
/// for GIF and MP4.
pub fn encode(frames: &[RgbaImage], delays: &[u32]) -> Result<Vec<u8>> {
    let first = frames.first().context("no frames")?;
    if frames.len() == 1 {
        return encode_still(first);
    }
    // 4:2:0 chroma needs even dimensions; drop an odd last row or column.
    let (width, height) = (first.width() & !1, first.height() & !1);
    ensure!(
        width > 0 && height > 0,
        "image too small for an AVIF sequence"
    );
    let (config, samples) = encode_sequence(frames, width, height)?;
    ensure!(
        samples.len() == frames.len(),
        "encoded {} pictures from {} frames",
        samples.len(),
        frames.len()
    );
    // The first frame is also the file's still image, for decoders that
    // don't play sequences.
    let still_length = samples[0].0.len() as u32;
    let meta = |offset: u32| still_meta(&config, width, height, offset, still_length);
    Ok(mp4::mux(
        &Container {
            brands: b"avis\0\0\0\0avifavismsf1iso8mif1miaf",
            handler: b"pict",
            entry: b"av01",
            config: (b"av1C", &config),
            looping: true,
            meta: Some(&meta),
        },
        &samples,
        width,
        height,
        &mp4::sample_delays(delays),
    ))
}

fn encode_still(image: &RgbaImage) -> Result<Vec<u8>> {
    let pixels: Vec<RGBA8> = image
        .as_raw()
        .chunks_exact(4)
        .map(|pixel| RGBA8::new(pixel[0], pixel[1], pixel[2], pixel[3]))
        .collect();
    let (width, height) = image.dimensions();
    let encoded = ravif::Encoder::new()
        .with_quality(QUALITY)
        .with_speed(SPEED)
        .encode_rgba(Img::new(pixels.as_slice(), width as usize, height as usize))
        .map_err(|error| anyhow!("AVIF encoding failed: {error}"))?;
    Ok(encoded.avif_file)
}

/// The `av1C` payload, and one temporal unit per frame with whether it's a
/// key frame.
#[allow(clippy::type_complexity)]
fn encode_sequence(
    frames: &[RgbaImage],
    width: u32,
    height: u32,
) -> Result<(Vec<u8>, Vec<(Vec<u8>, bool)>)> {
    let config = Config::new().with_encoder_config(EncoderConfig {
        width: width as usize,
        height: height as usize,
        time_base: Rational::new(1, mp4::TIMESCALE as u64),
        pixel_range: PixelRange::Limited,
        color_description: Some(ColorDescription {
            color_primaries: ColorPrimaries::BT709,
            transfer_characteristics: TransferCharacteristics::BT709,
            matrix_coefficients: MatrixCoefficients::BT709,
        }),
        quantizer: QUANTIZER,
        // Encodes in parallel (about twice as fast on 4+ cores), ~3% larger.
        tiles: 4,
        speed_settings: SpeedSettings::from_preset(SPEED),
        ..Default::default()
    });
    let mut context: Context<u8> = config
        .new_context()
        .map_err(|error| anyhow!("invalid AV1 config: {error}"))?;
    let pictures: Vec<_> = frames
        .par_iter()
        .map(|frame| mp4::to_yuv(frame, width, height))
        .collect();
    let mut samples = Vec::new();
    // Whether the encoder is done.
    let mut receive = |context: &mut Context<u8>| -> Result<bool> {
        loop {
            match context.receive_packet() {
                Ok(packet) => {
                    let key = packet.frame_type == FrameType::KEY;
                    samples.push((without_temporal_delimiter(packet.data), key));
                }
                Err(EncoderStatus::Encoded) => {}
                Err(EncoderStatus::NeedMoreData | EncoderStatus::EnoughData) => return Ok(false),
                Err(EncoderStatus::LimitReached) => return Ok(true),
                Err(error) => return Err(anyhow!("AV1 encoding failed: {error}")),
            }
        }
    };
    let chroma_width = width as usize / 2;
    for picture in &pictures {
        let mut frame = context.new_frame();
        frame.planes[0].copy_from_raw_u8(&picture.y, width as usize, 1);
        frame.planes[1].copy_from_raw_u8(&picture.u, chroma_width, 1);
        frame.planes[2].copy_from_raw_u8(&picture.v, chroma_width, 1);
        context
            .send_frame(frame)
            .map_err(|error| anyhow!("AV1 encoding failed: {error}"))?;
        receive(&mut context)?;
    }
    context.flush();
    while !receive(&mut context)? {}
    Ok((context.container_sequence_header(), samples))
}

/// rav1e starts each temporal unit with a temporal delimiter, which
/// ISOBMFF samples leave out.
fn without_temporal_delimiter(mut data: Vec<u8>) -> Vec<u8> {
    if data.starts_with(&[OBU_TEMPORAL_DELIMITER | 0b10, 0]) {
        data.drain(..2);
    }
    data
}

/// A `meta` box whose primary item is the first sample, at `offset`.
fn still_meta(config: &[u8], width: u32, height: u32, offset: u32, length: u32) -> Vec<u8> {
    let mut out = Vec::new();
    write_full_box(&mut out, b"meta", 0, |out| {
        write_full_box(out, b"hdlr", 0, |out| {
            out.extend([0; 4]);
            out.extend(b"pict");
            out.extend([0; 12]);
            out.push(0); // empty name
        });
        write_full_box(out, b"pitm", 0, |out| out.extend(1u16.to_be_bytes()));
        write_full_box(out, b"iloc", 0, |out| {
            out.push(0x44); // 4-byte offsets and lengths
            out.push(0); // no base offset
            out.extend(1u16.to_be_bytes()); // items
            out.extend(1u16.to_be_bytes()); // item ID
            out.extend(0u16.to_be_bytes()); // data reference: this file
            out.extend(1u16.to_be_bytes()); // extents
            out.extend(offset.to_be_bytes());
            out.extend(length.to_be_bytes());
        });
        write_full_box(out, b"iinf", 0, |out| {
            out.extend(1u16.to_be_bytes());
            // Version 2.
            write_full_box(out, b"infe", 2 << 24, |out| {
                out.extend(1u16.to_be_bytes()); // item ID
                out.extend(0u16.to_be_bytes()); // protection index
                out.extend(b"av01");
                out.push(0); // empty name
            });
        });
        write_box(out, b"iprp", |out| {
            write_box(out, b"ipco", |out| {
                write_full_box(out, b"ispe", 0, |out| {
                    out.extend(width.to_be_bytes());
                    out.extend(height.to_be_bytes());
                });
                write_box(out, b"av1C", |out| out.extend(config));
                write_colr(out);
                write_full_box(out, b"pixi", 0, |out| out.extend([3, 8, 8, 8]));
            });
            write_full_box(out, b"ipma", 0, |out| {
                out.extend(1u32.to_be_bytes()); // entries
                out.extend(1u16.to_be_bytes()); // item ID
                // Property indices (from 1); the high bit marks av1C essential.
                out.extend([4, 1, 0x80 | 2, 3, 4]);
            });
        });
    });
    out
}

#[cfg(test)]
mod tests {
    use image::Rgba;

    use super::*;
    use crate::mp4::tests::{child, path, table, u32_at};

    #[test]
    fn encodes_a_still() {
        let image = RgbaImage::from_pixel(5, 3, Rgba([200, 30, 30, 128]));
        let bytes = encode(&[image], &[0]).unwrap();
        assert_eq!(&bytes[4..12], b"ftypavif");
    }

    #[test]
    fn encodes_a_looping_sequence() {
        // Odd dimensions lose the last row and column.
        let colors = [[200, 30, 30], [30, 200, 30], [30, 30, 200]];
        let frames: Vec<RgbaImage> = colors
            .iter()
            .map(|&[r, g, b]| RgbaImage::from_pixel(35, 19, Rgba([r, g, b, 255])))
            .collect();
        let bytes = encode(&frames, &[120, 0, 3000]).unwrap();

        assert_eq!(&bytes[4..12], b"ftypavis");
        let trak = path(&bytes, &[b"moov", b"trak"]);
        // Repeats forever, so the track's duration is indefinite.
        assert_eq!(u32_at(child(trak, b"tkhd"), 20), u32::MAX);
        let elst = path(trak, &[b"edts", b"elst"]);
        assert_eq!(u32_at(elst, 0), 1);
        assert_eq!(table(elst, 0), [3220, 0, 0x0001_0000]);
        let stbl = path(trak, &[b"mdia", b"minf", b"stbl"]);
        assert_eq!(table(child(stbl, b"stts"), 0), [1, 120, 1, 100, 1, 3000]);
        assert_eq!(table(child(stbl, b"stss"), 0), [1]);
        let sizes = table(child(stbl, b"stsz"), 4);
        assert_eq!(sizes.len(), 3);
        let offset = table(child(stbl, b"stco"), 0)[0];
        assert_eq!(&bytes[offset as usize - 4..offset as usize], b"mdat");
        // Samples have no temporal delimiter; the key frame starts with a
        // sequence header (OBU type 1).
        assert_eq!(bytes[offset as usize] >> 3, 1);

        // The primary item is the first sample.
        let meta = &child(&bytes, b"meta")[4..];
        assert_eq!(&child(meta, b"pitm")[4..], [0, 1]);
        let iloc = child(meta, b"iloc");
        assert_eq!(u32_at(iloc, 14), offset);
        assert_eq!(u32_at(iloc, 18), sizes[0]);
        let ispe = path(meta, &[b"iprp", b"ipco", b"ispe"]);
        assert_eq!((u32_at(ispe, 4), u32_at(ispe, 8)), (34, 18));
    }
}
