//! MP4 encoding: H.264 from rusty_h264, a pure-Rust encoder (so output is the
//! same natively and on WebAssembly), in an MP4 container written here.

use anyhow::{Context, Result, anyhow, ensure};
use image::RgbaImage;
use rayon::prelude::*;
use rusty_h264_common::YuvFrame;
use rusty_h264_encoder::{Encoder, EncoderConfig, Preset};

/// Constant quantizer (0-51, lower is better). 23 is x264's default CRF.
const QP: u8 = 23;
/// Delays are in milliseconds.
const TIMESCALE: u32 = 1000;
/// Browsers show GIF frames with a delay of 10 ms or less for 100 ms; do the
/// same, since a video sample can't last 0 ms.
const MINIMUM_DELAY: u32 = 10;
const DEFAULT_DELAY: u32 = 100;

/// BT.709 luma coefficients. The colour space is also signalled in a `colr`
/// box, since the H.264 stream has no VUI.
const KR: f32 = 0.2126;
const KB: f32 = 0.0722;
const KG: f32 = 1.0 - KR - KB;

/// H.264 NAL unit types.
const NAL_SLICE: u8 = 1;
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

/// `delays` are in milliseconds, one per frame. Alpha is ignored, as for GIF.
pub fn encode(frames: &[RgbaImage], delays: &[u32]) -> Result<Vec<u8>> {
    let first = frames.first().context("no frames")?;
    // 4:2:0 chroma needs even dimensions; drop an odd last row or column.
    let (width, height) = (first.width() & !1, first.height() & !1);
    ensure!(width > 0 && height > 0, "image too small for MP4");
    let delays: Vec<u32> = delays
        .iter()
        .map(|&delay| {
            if delay <= MINIMUM_DELAY {
                DEFAULT_DELAY
            } else {
                delay
            }
        })
        .collect();

    let mut config = EncoderConfig::new(width as usize, height as usize);
    // Twice as fast as Balanced for files about 5% larger.
    config.preset = Preset::Fast;
    config.qp = QP;
    config.level_idc = level(
        width,
        height,
        delays.iter().copied().min().unwrap_or(DEFAULT_DELAY),
    );
    config.framerate = TIMESCALE as f32 * delays.len() as f32 / delays.iter().sum::<u32>() as f32;
    let mut encoder = Encoder::new(config).map_err(|error| anyhow!("{error:?}"))?;
    let pictures: Vec<YuvFrame> = frames
        .par_iter()
        .map(|frame| to_yuv(frame, width, height))
        .collect();
    let mut stream = Vec::new();
    for picture in &pictures {
        stream.extend(encoder.encode(picture));
    }
    stream.extend(encoder.flush());

    let track = Track::from_annex_b(&stream)?;
    ensure!(
        track.samples.len() == frames.len(),
        "encoded {} pictures from {} frames",
        track.samples.len(),
        frames.len()
    );
    Ok(track.mux(width, height, &delays))
}

/// The lowest level whose frame size and macroblock rate admit the video.
fn level(width: u32, height: u32, minimum_delay: u32) -> u8 {
    // (level_idc, MaxMBPS, MaxFS) from Table A-1.
    const LEVELS: [(u8, u64, u64); 7] = [
        (30, 40_500, 1_620),
        (31, 108_000, 3_600),
        (32, 216_000, 5_120),
        (40, 245_760, 8_192),
        (42, 522_240, 8_704),
        (50, 589_824, 22_080),
        (51, 983_040, 36_864),
    ];
    let (columns, rows) = (width.div_ceil(16) as u64, height.div_ceil(16) as u64);
    let size = columns * rows;
    let rate = size * 1000 / minimum_delay.max(1) as u64;
    LEVELS
        .iter()
        // Neither dimension may exceed sqrt(8 * MaxFS) macroblocks.
        .find(|(_, mbps, fs)| size <= *fs && rate <= *mbps && columns.max(rows).pow(2) <= 8 * fs)
        .map_or(52, |(level, _, _)| *level)
}

/// Limited-range BT.709 4:2:0, each chroma sample from the average of 2x2
/// pixels.
fn to_yuv(frame: &RgbaImage, width: u32, height: u32) -> YuvFrame {
    let (width, height, stride) = (width as usize, height as usize, frame.width() as usize * 4);
    let pixels = frame.as_raw();
    let rgb = |x: usize, y: usize| {
        let offset = y * stride + x * 4;
        let pixel = &pixels[offset..offset + 3];
        [pixel[0] as f32, pixel[1] as f32, pixel[2] as f32]
    };
    let luma = |[r, g, b]: [f32; 3]| KR * r + KG * g + KB * b;
    let clamp = |value: f32| value.round().clamp(0.0, 255.0) as u8;

    let mut y = Vec::with_capacity(width * height);
    for row in 0..height {
        for column in 0..width {
            y.push(clamp(16.0 + luma(rgb(column, row)) * 219.0 / 255.0));
        }
    }
    let (chroma_width, chroma_height) = (width / 2, height / 2);
    let mut u = Vec::with_capacity(chroma_width * chroma_height);
    let mut v = Vec::with_capacity(chroma_width * chroma_height);
    for row in 0..chroma_height {
        for column in 0..chroma_width {
            let mut sum = [0.0; 3];
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let pixel = rgb(column * 2 + dx, row * 2 + dy);
                for (total, value) in sum.iter_mut().zip(pixel) {
                    *total += value / 4.0;
                }
            }
            let [r, _, b] = sum;
            let luma = luma(sum);
            u.push(clamp(
                128.0 + (b - luma) / (2.0 * (1.0 - KB)) * 224.0 / 255.0,
            ));
            v.push(clamp(
                128.0 + (r - luma) / (2.0 * (1.0 - KR)) * 224.0 / 255.0,
            ));
        }
    }
    YuvFrame {
        width,
        height,
        y,
        u,
        v,
    }
}

/// An H.264 track: parameter sets and length-prefixed samples.
struct Track {
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// One per picture, with whether it's an IDR picture.
    samples: Vec<(Vec<u8>, bool)>,
}

impl Track {
    /// Split an Annex B stream into pictures. Parameter sets move to the
    /// sample description; other non-picture units go with the next picture.
    fn from_annex_b(stream: &[u8]) -> Result<Self> {
        let (mut sps, mut pps) = (None, None);
        let mut samples: Vec<(Vec<u8>, bool)> = Vec::new();
        let mut pending = Vec::new();
        for unit in nal_units(stream) {
            let kind = unit[0] & 0x1f;
            match kind {
                NAL_SPS => {
                    sps.get_or_insert_with(|| unit.to_vec());
                }
                NAL_PPS => {
                    pps.get_or_insert_with(|| unit.to_vec());
                }
                NAL_AUD => {}
                NAL_SLICE | NAL_IDR => {
                    // `first_mb_in_slice` is the first field, as ue(v): a set
                    // first bit means 0, so a new picture.
                    let first_slice = unit.get(1).is_some_and(|byte| byte & 0x80 != 0);
                    if first_slice || samples.is_empty() {
                        samples.push((std::mem::take(&mut pending), false));
                    }
                    let (sample, idr) = samples.last_mut().expect("pushed above");
                    *idr |= kind == NAL_IDR;
                    write_length_prefixed(sample, unit);
                }
                _ => write_length_prefixed(&mut pending, unit),
            }
        }
        Ok(Self {
            sps: sps.context("no SPS in the H.264 stream")?,
            pps: pps.context("no PPS in the H.264 stream")?,
            samples,
        })
    }

    /// A faststart MP4 (`moov` before `mdat`) with a single chunk.
    fn mux(&self, width: u32, height: u32, delays: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        write_box(&mut out, b"ftyp", |out| {
            out.extend(b"isom");
            out.extend(0x200u32.to_be_bytes());
            out.extend(b"isomiso2avc1mp41");
        });
        // The chunk offset doesn't change the size of `moov`.
        let mut moov = self.moov(width, height, delays, 0);
        let offset = (out.len() + moov.len() + 8) as u32;
        moov = self.moov(width, height, delays, offset);
        out.extend(moov);
        write_box(&mut out, b"mdat", |out| {
            for (sample, _) in &self.samples {
                out.extend(sample);
            }
        });
        out
    }

    fn moov(&self, width: u32, height: u32, delays: &[u32], offset: u32) -> Vec<u8> {
        let duration: u32 = delays.iter().sum();
        let mut out = Vec::new();
        write_box(&mut out, b"moov", |out| {
            write_full_box(out, b"mvhd", 0, |out| {
                out.extend([0; 8]); // creation and modification time
                out.extend(TIMESCALE.to_be_bytes());
                out.extend(duration.to_be_bytes());
                out.extend(0x0001_0000u32.to_be_bytes()); // rate 1.0
                out.extend(0x0100u16.to_be_bytes()); // volume 1.0
                out.extend([0; 10]);
                out.extend(MATRIX);
                out.extend([0; 24]);
                out.extend(2u32.to_be_bytes()); // next track ID
            });
            write_box(out, b"trak", |out| {
                // Enabled, in the movie.
                write_full_box(out, b"tkhd", 3, |out| {
                    out.extend([0; 8]);
                    out.extend(1u32.to_be_bytes()); // track ID
                    out.extend([0; 4]);
                    out.extend(duration.to_be_bytes());
                    out.extend([0; 16]); // reserved, layer, group, volume
                    out.extend(MATRIX);
                    out.extend((width << 16).to_be_bytes());
                    out.extend((height << 16).to_be_bytes());
                });
                write_box(out, b"mdia", |out| {
                    write_full_box(out, b"mdhd", 0, |out| {
                        out.extend([0; 8]);
                        out.extend(TIMESCALE.to_be_bytes());
                        out.extend(duration.to_be_bytes());
                        out.extend(0x55c4u16.to_be_bytes()); // language "und"
                        out.extend([0; 2]);
                    });
                    write_full_box(out, b"hdlr", 0, |out| {
                        out.extend([0; 4]);
                        out.extend(b"vide");
                        out.extend([0; 12]);
                        out.extend(b"VideoHandler\0");
                    });
                    write_box(out, b"minf", |out| {
                        write_full_box(out, b"vmhd", 1, |out| out.extend([0; 8]));
                        write_box(out, b"dinf", |out| {
                            write_full_box(out, b"dref", 0, |out| {
                                out.extend(1u32.to_be_bytes());
                                // The data is in this file.
                                write_full_box(out, b"url ", 1, |_| {});
                            });
                        });
                        write_box(out, b"stbl", |out| {
                            self.stbl(out, width, height, delays, offset)
                        });
                    });
                });
            });
        });
        out
    }

    fn stbl(&self, out: &mut Vec<u8>, width: u32, height: u32, delays: &[u32], offset: u32) {
        write_full_box(out, b"stsd", 0, |out| {
            out.extend(1u32.to_be_bytes());
            write_box(out, b"avc1", |out| {
                out.extend([0; 6]);
                out.extend(1u16.to_be_bytes()); // data reference index
                out.extend([0; 16]);
                out.extend((width as u16).to_be_bytes());
                out.extend((height as u16).to_be_bytes());
                out.extend(0x0048_0000u32.to_be_bytes()); // 72 dpi
                out.extend(0x0048_0000u32.to_be_bytes());
                out.extend([0; 4]);
                out.extend(1u16.to_be_bytes()); // frames per sample
                out.extend([0; 32]); // compressor name
                out.extend(0x18u16.to_be_bytes()); // depth
                out.extend((-1i16).to_be_bytes());
                write_box(out, b"avcC", |out| {
                    // Version, then profile, compatibility and level.
                    out.push(1);
                    out.extend(&self.sps[1..4]);
                    out.push(0xfc | 3); // 4-byte NAL unit lengths
                    out.push(0xe0 | 1);
                    out.extend((self.sps.len() as u16).to_be_bytes());
                    out.extend(&self.sps);
                    out.push(1);
                    out.extend((self.pps.len() as u16).to_be_bytes());
                    out.extend(&self.pps);
                    // High profiles add the chroma format and bit depths.
                    if ![66, 77, 88].contains(&self.sps[1]) {
                        out.extend([0xfc | 1, 0xf8, 0xf8, 0]);
                    }
                });
                write_box(out, b"colr", |out| {
                    out.extend(b"nclx");
                    // BT.709 primaries, transfer and matrix; limited range.
                    out.extend([0, 1, 0, 1, 0, 1, 0]);
                });
            });
        });
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for &delay in delays {
            match runs.last_mut() {
                Some((count, last)) if *last == delay => *count += 1,
                _ => runs.push((1, delay)),
            }
        }
        write_full_box(out, b"stts", 0, |out| {
            out.extend((runs.len() as u32).to_be_bytes());
            for (count, delay) in &runs {
                out.extend(count.to_be_bytes());
                out.extend(delay.to_be_bytes());
            }
        });
        let sync: Vec<u32> = (1..)
            .zip(&self.samples)
            .filter(|(_, (_, idr))| *idr)
            .map(|(number, _)| number)
            .collect();
        write_full_box(out, b"stss", 0, |out| {
            out.extend((sync.len() as u32).to_be_bytes());
            for number in &sync {
                out.extend(number.to_be_bytes());
            }
        });
        let count = self.samples.len() as u32;
        write_full_box(out, b"stsc", 0, |out| {
            out.extend(1u32.to_be_bytes());
            out.extend(1u32.to_be_bytes()); // first chunk
            out.extend(count.to_be_bytes());
            out.extend(1u32.to_be_bytes()); // sample description
        });
        write_full_box(out, b"stsz", 0, |out| {
            out.extend(0u32.to_be_bytes());
            out.extend(count.to_be_bytes());
            for (sample, _) in &self.samples {
                out.extend((sample.len() as u32).to_be_bytes());
            }
        });
        write_full_box(out, b"stco", 0, |out| {
            out.extend(1u32.to_be_bytes());
            out.extend(offset.to_be_bytes());
        });
    }
}

/// Identity transformation matrix.
const MATRIX: [u8; 36] = {
    let mut matrix = [0; 36];
    matrix[1] = 1; // 1.0 in 16.16
    matrix[17] = 1;
    matrix[32] = 0x40; // 1.0 in 2.30
    matrix
};

/// The NAL units of an Annex B stream, without start codes. Zeros before a
/// start code (from a 4-byte start code or padding) aren't part of a unit.
fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start = None;
    let mut index = 0;
    while index + 3 <= stream.len() {
        if stream[index..index + 3] != [0, 0, 1] {
            index += 1;
            continue;
        }
        if let Some(start) = start {
            units.push(trim_zeros(&stream[start..index]));
        }
        index += 3;
        start = Some(index);
    }
    if let Some(start) = start {
        units.push(trim_zeros(&stream[start..]));
    }
    units.retain(|unit| !unit.is_empty());
    units
}

fn trim_zeros(unit: &[u8]) -> &[u8] {
    let end = unit
        .iter()
        .rposition(|&byte| byte != 0)
        .map_or(0, |last| last + 1);
    &unit[..end]
}

fn write_length_prefixed(out: &mut Vec<u8>, unit: &[u8]) {
    out.extend((unit.len() as u32).to_be_bytes());
    out.extend(unit);
}

fn write_box(out: &mut Vec<u8>, kind: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    out.extend([0; 4]);
    out.extend(kind);
    body(out);
    let size = (out.len() - start) as u32;
    out[start..start + 4].copy_from_slice(&size.to_be_bytes());
}

/// A box with a version (always 0 here) and 24 bits of flags.
fn write_full_box(out: &mut Vec<u8>, kind: &[u8; 4], flags: u32, body: impl FnOnce(&mut Vec<u8>)) {
    write_box(out, kind, |out| {
        out.extend(flags.to_be_bytes());
        body(out);
    });
}

#[cfg(test)]
mod tests {
    use image::Rgba;
    use rusty_h264_decoder::Decoder;

    use super::*;

    /// The payload of the first `kind` box in `data`.
    fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> &'a [u8] {
        let mut rest = data;
        while rest.len() >= 8 {
            let size = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
            if &rest[4..8] == kind {
                return &rest[8..size];
            }
            rest = &rest[size..];
        }
        panic!("no {} box", String::from_utf8_lossy(kind));
    }

    fn path<'a>(data: &'a [u8], kinds: &[&[u8; 4]]) -> &'a [u8] {
        kinds.iter().fold(data, |data, kind| child(data, kind))
    }

    fn u32_at(data: &[u8], offset: usize) -> u32 {
        u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
    }

    /// A table of u32 values after a full box's version, flags and count.
    fn table(data: &[u8], skip: usize) -> Vec<u32> {
        let count = u32_at(data, 4 + skip) as usize;
        let size = (data.len() - 8 - skip) / count.max(1);
        (0..count)
            .flat_map(|index| {
                let offset = 8 + skip + index * size;
                (0..size / 4).map(move |field| offset + field * 4)
            })
            .map(|offset| u32_at(data, offset))
            .collect()
    }

    #[test]
    fn encodes_video() {
        // Odd dimensions lose the last row and column.
        let colors = [[200, 30, 30], [30, 200, 30], [30, 30, 200]];
        let frames: Vec<RgbaImage> = colors
            .iter()
            .map(|&[r, g, b]| RgbaImage::from_pixel(35, 19, Rgba([r, g, b, 255])))
            .collect();
        let bytes = encode(&frames, &[120, 0, 3000]).unwrap();

        assert_eq!(&bytes[4..8], b"ftyp");
        let stbl = path(&bytes, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"]);
        let tkhd = path(&bytes, &[b"moov", b"trak", b"tkhd"]);
        assert_eq!((u32_at(tkhd, 76), u32_at(tkhd, 80)), (34 << 16, 18 << 16));
        // A delay of 0 plays for 100 ms, as in browsers.
        assert_eq!(table(child(stbl, b"stts"), 0), [1, 120, 1, 100, 1, 3000]);
        assert_eq!(table(child(stbl, b"stss"), 0), [1]);
        let sizes = table(child(stbl, b"stsz"), 4);
        assert_eq!(sizes.len(), 3);
        let offset = table(child(stbl, b"stco"), 0)[0] as usize;
        let mdat = child(&bytes, b"mdat");
        assert_eq!(&bytes[offset - 4..offset], b"mdat");
        assert_eq!(mdat.len(), sizes.iter().sum::<u32>() as usize);

        // Rebuild an Annex B stream from the sample description and samples.
        // avc1 has 78 bytes of fields before its child boxes.
        let avcc = path(&child(stbl, b"stsd")[8..], &[b"avc1"]);
        let avcc = child(&avcc[78..], b"avcC");
        let sps_length = u16::from_be_bytes([avcc[6], avcc[7]]) as usize;
        let sps = &avcc[8..8 + sps_length];
        // A count of 1, then the length and the PPS.
        let rest = &avcc[8 + sps_length..];
        let pps = &rest[3..3 + u16::from_be_bytes([rest[1], rest[2]]) as usize];
        let mut stream = Vec::new();
        for unit in [sps, pps] {
            stream.extend([0, 0, 0, 1]);
            stream.extend(unit);
        }
        let mut rest = mdat;
        while !rest.is_empty() {
            let length = u32_at(rest, 0) as usize;
            stream.extend([0, 0, 0, 1]);
            stream.extend(&rest[4..4 + length]);
            rest = &rest[4 + length..];
        }
        let decoded = Decoder::new().decode_stream(&stream).unwrap();
        assert_eq!(decoded.len(), 3);
        for (picture, original) in decoded.iter().zip(&frames) {
            assert_eq!((picture.width, picture.height), (34, 18));
            let expected = to_yuv(original, 34, 18);
            for (a, b) in [
                (&picture.y, &expected.y),
                (&picture.u, &expected.u),
                (&picture.v, &expected.v),
            ] {
                for (x, y) in a.iter().zip(b) {
                    assert!(x.abs_diff(*y) <= 6, "{x} vs {y}");
                }
            }
        }
    }

    #[test]
    fn converts_to_bt709() {
        let frame = RgbaImage::from_fn(2, 2, |x, _| {
            if x == 0 {
                Rgba([255, 255, 255, 255])
            } else {
                Rgba([0, 0, 0, 255])
            }
        });
        let picture = to_yuv(&frame, 2, 2);
        assert_eq!(picture.y, [235, 16, 235, 16]);
        assert_eq!((picture.u[0], picture.v[0]), (128, 128));
        let red = to_yuv(&RgbaImage::from_pixel(2, 2, Rgba([255, 0, 0, 255])), 2, 2);
        assert_eq!((red.y[0], red.u[0], red.v[0]), (63, 102, 240));
    }

    #[test]
    fn picks_a_level() {
        assert_eq!(level(600, 434, 100), 30);
        assert_eq!(level(800, 600, 100), 31);
        assert_eq!(level(600, 600, 20), 31);
        assert_eq!(level(600, 2400, 100), 40);
    }
}
