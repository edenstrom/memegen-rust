//! PNG encoding for static memes, split into horizontal strips.
//!
//! A template's background is encoded once; each render then re-encodes only
//! the strips its text touches and copies the rest. Every row uses the Paeth
//! filter, and each strip is a non-final deflate block ending in a sync flush,
//! so strips are independent byte strings that can be encoded in parallel.
//!
//! Compression mirrors `fdeflate` (what `image` uses for fast PNGs): one fixed
//! Huffman table tuned for filtered image data, with runs of zero bytes as the
//! only back-references. The tables and block header below come from fdeflate
//! 0.3 (MIT OR Apache-2.0).

use std::ops::Range;

use image::RgbaImage;
use rayon::prelude::*;

/// Rows per strip: small enough that text rarely dirties much more than it
/// covers, large enough that per-strip overhead (~70 bytes) stays negligible.
const STRIP_ROWS: u32 = 16;
/// Strips are cheap to encode; batching them avoids waking more threads
/// than the work is worth.
const STRIPS_PER_TASK: usize = 4;

const SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
const ZLIB_HEADER: [u8; 2] = [0x78, 0x01];
/// A final, empty block with fixed Huffman codes.
const FINAL_BLOCK: [u8; 2] = [0x03, 0x00];
const PAETH: u8 = 4;

/// fdeflate's dynamic block header (without the zlib header), with BFINAL
/// cleared. The last byte holds 5 bits.
const BLOCK_HEADER: [u8; 52] = [
    236, 192, 3, 160, 36, 89, 150, 198, 241, 255, 119, 238, 141, 200, 204, 167, 114, 75, 99, 174,
    109, 219, 182, 109, 219, 182, 109, 219, 182, 109, 105, 140, 158, 150, 74, 175, 158, 50, 51, 34,
    238, 249, 118, 183, 106, 122, 166, 135, 59, 107, 213, 15,
];

const HUFFMAN_LENGTHS: [u8; 286] = [
    2, 3, 4, 5, 5, 6, 6, 7, 7, 7, 8, 8, 8, 8, 8, 9, 9, 9, 9, 9, 9, 9, 10, 10, 10, 10, 10, 10, 10,
    10, 10, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12,
    12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12,
    12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12,
    12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12,
    12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12,
    12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12,
    12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12,
    12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 11, 11, 11, 11, 11, 11, 11,
    11, 11, 11, 10, 11, 10, 10, 10, 10, 10, 10, 10, 10, 10, 9, 9, 9, 9, 9, 8, 9, 8, 8, 8, 8, 8, 7,
    7, 7, 6, 6, 6, 5, 4, 3, 12, 12, 12, 9, 9, 11, 10, 11, 11, 10, 11, 11, 11, 11, 11, 11, 12, 11,
    12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 9,
];

/// Canonical Huffman codes for `HUFFMAN_LENGTHS`, bit-reversed for deflate.
const HUFFMAN_CODES: [u16; 286] = {
    let mut codes = [0u16; 286];
    let mut code = 0u32;
    let mut length = 1;
    while length <= 16 {
        let mut symbol = 0;
        while symbol < 286 {
            if HUFFMAN_LENGTHS[symbol] == length {
                codes[symbol] = (code as u16).reverse_bits() >> (16 - length);
                code += 1;
            }
            symbol += 1;
        }
        code <<= 1;
        length += 1;
    }
    assert!(code == 2 << 16, "HUFFMAN_LENGTHS is invalid");
    codes
};

/// Deflate length symbols 257..=285: `(base length, extra bits)`.
const LENGTH_CODES: [(u32, u8); 29] = [
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 1),
    (13, 1),
    (15, 1),
    (17, 1),
    (19, 2),
    (23, 2),
    (27, 2),
    (31, 2),
    (35, 3),
    (43, 3),
    (51, 3),
    (59, 3),
    (67, 4),
    (83, 4),
    (99, 4),
    (115, 4),
    (131, 5),
    (163, 5),
    (195, 5),
    (227, 5),
    (258, 0),
];

/// A strip's IDAT chunk and the Adler-32 of its uncompressed bytes.
struct Strip {
    chunk: Vec<u8>,
    adler: u32,
    length: usize,
}

/// A background image encoded as strips.
pub struct Strips {
    width: u32,
    height: u32,
    strips: Vec<Strip>,
}

impl Strips {
    pub fn new(image: &RgbaImage) -> Self {
        let (width, height) = image.dimensions();
        let strips = strip_ranges(height)
            .collect::<Vec<_>>()
            .into_par_iter()
            .with_min_len(STRIPS_PER_TASK)
            .map(|rows| encode_strip(image.as_raw(), width, 0, rows))
            .collect();
        Self {
            width,
            height,
            strips,
        }
    }

    /// Bytes held, for cache weighing.
    pub fn size(&self) -> usize {
        self.strips.iter().map(|strip| strip.chunk.len()).sum()
    }

    /// Encode an image that matches the original outside the `dirty` rows.
    /// `compose(rows)` returns the new image's pixels for just those rows.
    pub fn encode(
        &self,
        dirty: &[Range<u32>],
        compose: impl Fn(Range<u32>) -> RgbaImage + Sync,
    ) -> Vec<u8> {
        // Filtering reads the previous row, so that must be clean too.
        let touched: Vec<(usize, Range<u32>)> = strip_ranges(self.height)
            .enumerate()
            .filter(|(_, rows)| {
                let start = rows.start.saturating_sub(1);
                dirty
                    .iter()
                    .any(|range| range.start < rows.end && start < range.end)
            })
            .collect();
        let mut fresh: Vec<Option<Strip>> = (0..self.strips.len()).map(|_| None).collect();
        let encoded: Vec<Strip> = touched
            .par_iter()
            .with_min_len(STRIPS_PER_TASK)
            .map(|(_, rows)| {
                let start = rows.start.saturating_sub(1);
                let image = compose(start..rows.end);
                encode_strip(image.as_raw(), self.width, start, rows.clone())
            })
            .collect();
        for ((index, _), strip) in touched.iter().zip(encoded) {
            fresh[*index] = Some(strip);
        }
        let strips = fresh
            .iter()
            .zip(&self.strips)
            .map(|(fresh, cached)| fresh.as_ref().unwrap_or(cached));
        write_png(self.width, self.height, strips)
    }
}

fn strip_ranges(height: u32) -> impl Iterator<Item = Range<u32>> {
    (0..height)
        .step_by(STRIP_ROWS as usize)
        .map(move |start| start..(start + STRIP_ROWS).min(height))
}

/// Encode `rows` of an RGBA image whose buffer `pixels` starts at row `first`.
fn encode_strip(pixels: &[u8], width: u32, first: u32, rows: Range<u32>) -> Strip {
    let stride = width as usize * 3;
    let row_bytes = width as usize * 4;
    let rgb = |y: u32, out: &mut [u8]| {
        let offset = (y - first) as usize * row_bytes;
        let row = &pixels[offset..offset + row_bytes];
        for (dst, src) in out.chunks_exact_mut(3).zip(row.chunks_exact(4)) {
            dst.copy_from_slice(&src[..3]);
        }
    };
    let mut previous = vec![0u8; stride];
    let mut current = vec![0u8; stride];
    if rows.start > 0 {
        rgb(rows.start - 1, &mut previous);
    }
    let mut filtered = Vec::with_capacity((stride + 1) * rows.len());
    for y in rows {
        rgb(y, &mut current);
        filtered.push(PAETH);
        paeth_filter(&current, &previous, &mut filtered);
        std::mem::swap(&mut previous, &mut current);
    }

    let mut adler = simd_adler32::Adler32::new();
    adler.write(&filtered);
    let mut writer = BitWriter::with_capacity(8 + filtered.len() / 2);
    writer.bytes.extend_from_slice(b"\0\0\0\0IDAT");
    writer.bytes.extend_from_slice(&BLOCK_HEADER[..51]);
    writer.write_bits(BLOCK_HEADER[51] as u64, 5);
    compress(&mut writer, &filtered);
    writer.sync_flush();
    let mut chunk = writer.bytes;
    finish_chunk(&mut chunk);
    Strip {
        chunk,
        adler: adler.finish(),
        length: filtered.len(),
    }
}

fn paeth_filter(current: &[u8], previous: &[u8], out: &mut Vec<u8>) {
    const BPP: usize = 3;
    out.extend(
        current[..BPP]
            .iter()
            .zip(&previous[..BPP])
            .map(|(&x, &b)| x.wrapping_sub(b)),
    );
    out.extend(
        current[BPP..]
            .iter()
            .zip(&current[..current.len() - BPP])
            .zip(previous[BPP..].iter().zip(previous))
            .map(|((&x, &a), (&b, &c))| x.wrapping_sub(paeth(a, b, c))),
    );
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (ia, ib, ic) = (a as i16, b as i16, c as i16);
    let pa = (ib - ic).abs();
    let pb = (ia - ic).abs();
    let pc = (ia + ib - 2 * ic).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    buffer: u64,
    bits: u8,
}

impl BitWriter {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
            buffer: 0,
            bits: 0,
        }
    }

    fn write_bits(&mut self, bits: u64, count: u8) {
        debug_assert!(count <= 64);
        self.buffer |= bits << self.bits;
        self.bits += count;
        if self.bits >= 64 {
            self.bytes.extend_from_slice(&self.buffer.to_le_bytes());
            self.bits -= 64;
            self.buffer = bits.checked_shr((count - self.bits) as u32).unwrap_or(0);
        }
    }

    fn write_symbol(&mut self, symbol: usize) {
        self.write_bits(HUFFMAN_CODES[symbol] as u64, HUFFMAN_LENGTHS[symbol]);
    }

    /// End the block, then add an empty stored block to byte-align the output.
    fn sync_flush(&mut self) {
        self.write_symbol(256);
        self.write_bits(0, 3);
        let whole = self.bits.div_ceil(8) as usize;
        self.bytes
            .extend_from_slice(&self.buffer.to_le_bytes()[..whole]);
        self.bytes.extend_from_slice(&[0, 0, 0xff, 0xff]);
        self.buffer = 0;
        self.bits = 0;
    }
}

/// fdeflate's `Compressor::write_data`.
fn compress(writer: &mut BitWriter, data: &[u8]) {
    let mut run = 0;
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().unwrap());
        if word == 0 {
            run += 8;
            continue;
        } else if run > 0 {
            let extra = word.trailing_zeros() / 8;
            write_run(writer, run + extra);
            run = 0;
            if extra > 0 {
                run = word.leading_zeros() / 8;
                for &byte in &chunk[extra as usize..8 - run as usize] {
                    writer.write_symbol(byte as usize);
                }
                continue;
            }
        }

        let tail = word.leading_zeros() / 8;
        if tail > 0 {
            for &byte in &chunk[..8 - tail as usize] {
                writer.write_symbol(byte as usize);
            }
            run = tail;
            continue;
        }

        for half in chunk.chunks_exact(4) {
            let (mut bits, mut count) = (0u64, 0u8);
            for &byte in half {
                bits |= (HUFFMAN_CODES[byte as usize] as u64) << count;
                count += HUFFMAN_LENGTHS[byte as usize];
            }
            writer.write_bits(bits, count);
        }
    }
    if run > 0 {
        write_run(writer, run);
    }
    for &byte in chunks.remainder() {
        writer.write_symbol(byte as usize);
    }
}

/// A zero byte, then copies of it (distance 1, whose code is a single 0 bit).
fn write_run(writer: &mut BitWriter, mut run: u32) {
    writer.write_symbol(0);
    run -= 1;
    while run >= 258 {
        writer.write_bits(HUFFMAN_CODES[285] as u64, HUFFMAN_LENGTHS[285] + 1);
        run -= 258;
    }
    if run > 4 {
        let index = LENGTH_CODES.partition_point(|&(base, _)| base <= run) - 1;
        let (base, extra) = LENGTH_CODES[index];
        let symbol = 257 + index;
        writer.write_symbol(symbol);
        writer.write_bits((run - base) as u64, extra + 1);
    } else {
        writer.write_bits(0, run as u8 * HUFFMAN_LENGTHS[0]);
    }
}

/// Fill in the length and CRC of a chunk built as `[0; 4] + type + data`.
fn finish_chunk(chunk: &mut Vec<u8>) {
    let length = u32::try_from(chunk.len() - 8).expect("chunk too large");
    chunk[..4].copy_from_slice(&length.to_be_bytes());
    let crc = crc32fast::hash(&chunk[4..]);
    chunk.extend_from_slice(&crc.to_be_bytes());
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    let mut chunk = Vec::with_capacity(12 + data.len());
    chunk.extend_from_slice(&[0; 4]);
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    finish_chunk(&mut chunk);
    out.extend_from_slice(&chunk);
}

fn write_png<'a>(
    width: u32,
    height: u32,
    strips: impl Iterator<Item = &'a Strip> + Clone,
) -> Vec<u8> {
    let size: usize = strips.clone().map(|strip| strip.chunk.len()).sum();
    let mut out = Vec::with_capacity(size + 128);
    out.extend_from_slice(&SIGNATURE);
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    // 8-bit RGB, deflate, adaptive filtering, no interlacing.
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    write_chunk(&mut out, b"IHDR", &header);
    write_chunk(&mut out, b"IDAT", &ZLIB_HEADER);
    let mut adler = 1;
    for strip in strips {
        out.extend_from_slice(&strip.chunk);
        adler = adler32_combine(adler, strip.adler, strip.length);
    }
    let mut trailer = FINAL_BLOCK.to_vec();
    trailer.extend_from_slice(&adler.to_be_bytes());
    write_chunk(&mut out, b"IDAT", &trailer);
    write_chunk(&mut out, b"IEND", &[]);
    out
}

/// zlib's `adler32_combine`: the checksum of `a ++ b` from those of `a` and `b`.
fn adler32_combine(first: u32, second: u32, second_length: usize) -> u32 {
    const BASE: u64 = 65521;
    let remainder = second_length as u64 % BASE;
    let (first, second) = (first as u64, second as u64);
    let mut sum1 = first & 0xffff;
    let mut sum2 = (remainder * sum1) % BASE;
    sum1 += (second & 0xffff) + BASE - 1;
    sum2 += (first >> 16) + (second >> 16) + BASE - remainder;
    ((sum1 % BASE) | ((sum2 % BASE) << 16)) as u32
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{ImageReader, Rgba};

    use super::*;

    fn decode(bytes: &[u8]) -> RgbaImage {
        ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .into_rgba8()
    }

    fn gradient(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            Rgba([(x * 7) as u8, (y * 3) as u8, ((x ^ y) & 0xf0) as u8, 255])
        })
    }

    #[test]
    fn round_trips() {
        for (width, height) in [(1, 1), (5, 3), (37, 50), (300, 129)] {
            let image = gradient(width, height);
            let strips = Strips::new(&image);
            assert_eq!(decode(&strips.encode(&[], |_| unreachable!())), image);
        }
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn reencodes_dirty_rows() {
        let base = gradient(64, 100);
        let mut changed = base.clone();
        for y in 40..45 {
            for x in 10..20 {
                changed.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            }
        }
        let strips = Strips::new(&base);
        let compose = |rows: Range<u32>| {
            image::imageops::crop_imm(&changed, 0, rows.start, 64, rows.len() as u32).to_image()
        };
        assert_eq!(decode(&strips.encode(&[40..45], compose)), changed);
        // Row 48 starts a strip, so the strip before it is not re-encoded.
        let mut edge = base.clone();
        edge.put_pixel(0, 48, Rgba([1, 2, 3, 255]));
        let compose = |rows: Range<u32>| {
            image::imageops::crop_imm(&edge, 0, rows.start, 64, rows.len() as u32).to_image()
        };
        assert_eq!(decode(&strips.encode(&[48..49], compose)), edge);
    }

    #[test]
    fn combines_checksums() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let (a, b) = data.split_at(70_001);
        assert_eq!(
            adler32_combine(
                simd_adler32::adler32(&a),
                simd_adler32::adler32(&b),
                b.len()
            ),
            simd_adler32::adler32(&data.as_slice())
        );
    }
}
