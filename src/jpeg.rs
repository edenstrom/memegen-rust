//! JPEG encoding for static memes, split into rows of 8x8 blocks.
//!
//! Like [`crate::png`]: a template's background is encoded once, and each
//! render re-encodes only the block rows its text touches. A restart marker
//! after every block row resets the DC predictors, so each row's entropy-coded
//! data is independent and rows from separate encodes can be spliced together.
//! Chroma is not subsampled (as with `image`'s encoder), so rows are 8 pixels.

use std::ops::Range;

use anyhow::{Context, Result, ensure};
use image::RgbaImage;
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use rayon::prelude::*;

const BLOCK_ROWS: u32 = 8;
/// Block rows per encode; each encode has fixed setup costs.
const ROWS_PER_TASK: usize = 8;

const EOI: [u8; 2] = [0xff, 0xd9];

/// A background image encoded as independently decodable block rows.
pub struct Rows {
    quality: u8,
    width: u32,
    height: u32,
    /// Everything up to and including the start-of-scan segment.
    header: Vec<u8>,
    /// Entropy-coded data of each block row, without restart markers.
    rows: Vec<Vec<u8>>,
}

impl Rows {
    pub fn new(image: &RgbaImage, quality: u8) -> Result<Self> {
        let (width, height) = image.dimensions();
        let block_rows: Vec<u32> = (0..height.div_ceil(BLOCK_ROWS)).collect();
        let chunks = block_rows
            .par_chunks(ROWS_PER_TASK)
            .map(|chunk| {
                let rows =
                    chunk[0] * BLOCK_ROWS..((chunk[chunk.len() - 1] + 1) * BLOCK_ROWS).min(height);
                encode_rows(&crop(image, rows), quality)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut chunks = chunks.into_iter();
        let (mut header, mut rows) = chunks.next().context("empty image")?;
        set_height(&mut header, height)?;
        rows.extend(chunks.flat_map(|(_, rows)| rows));
        Ok(Self {
            quality,
            width,
            height,
            header,
            rows,
        })
    }

    /// Bytes held, for cache weighing.
    pub fn size(&self) -> usize {
        self.header.len() + self.rows.iter().map(Vec::len).sum::<usize>()
    }

    /// Encode an image that matches the original outside the `dirty` rows.
    /// `compose(rows)` returns the new image's pixels for just those rows.
    pub fn encode(
        &self,
        dirty: &[Range<u32>],
        compose: impl Fn(Range<u32>) -> RgbaImage + Sync,
    ) -> Result<Vec<u8>> {
        let height = self.height;
        let touched: Vec<u32> = (0..self.rows.len() as u32)
            .filter(|&row| {
                let (start, end) = (row * BLOCK_ROWS, (row + 1) * BLOCK_ROWS);
                dirty
                    .iter()
                    .any(|range| range.start < end && start < range.end)
            })
            .collect();
        // Batch runs of adjacent rows into single encodes.
        let mut tasks: Vec<Vec<u32>> = Vec::new();
        for row in touched {
            match tasks.last_mut() {
                Some(task) if task.len() < ROWS_PER_TASK && task.last() == Some(&(row - 1)) => {
                    task.push(row)
                }
                _ => tasks.push(vec![row]),
            }
        }
        let encoded = tasks
            .par_iter()
            .map(|task| {
                let rows =
                    task[0] * BLOCK_ROWS..((task[task.len() - 1] + 1) * BLOCK_ROWS).min(height);
                let image = compose(rows);
                ensure!(
                    image.width() == self.width,
                    "composed rows have the wrong width"
                );
                Ok(encode_rows(&image, self.quality)?.1)
            })
            .collect::<Result<Vec<_>>>()?;

        let mut fresh: Vec<Option<&[u8]>> = vec![None; self.rows.len()];
        for (task, rows) in tasks.iter().zip(&encoded) {
            for (&row, data) in task.iter().zip(rows) {
                fresh[row as usize] = Some(data);
            }
        }
        let size = self.size()
            + 2 * self.rows.len()
            + encoded.iter().flatten().map(Vec::len).sum::<usize>();
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(&self.header);
        for (index, (fresh, cached)) in fresh.iter().zip(&self.rows).enumerate() {
            if index > 0 {
                out.extend_from_slice(&[0xff, 0xd0 + (index - 1) as u8 % 8]);
            }
            out.extend_from_slice(fresh.unwrap_or(cached));
        }
        out.extend_from_slice(&EOI);
        Ok(out)
    }
}

fn crop(image: &RgbaImage, rows: Range<u32>) -> RgbaImage {
    let row_bytes = image.width() as usize * 4;
    let pixels =
        image.as_raw()[rows.start as usize * row_bytes..rows.end as usize * row_bytes].to_vec();
    RgbaImage::from_raw(image.width(), rows.len() as u32, pixels)
        .expect("buffer matches dimensions")
}

/// Encode with a restart marker after every block row; returns the header
/// and each block row's entropy-coded data.
fn encode_rows(image: &RgbaImage, quality: u8) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let (width, height) = (
        u16::try_from(image.width())?,
        u16::try_from(image.height())?,
    );
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, quality);
    encoder.set_sampling_factor(SamplingFactor::F_1_1);
    encoder.set_restart_interval(width.div_ceil(BLOCK_ROWS as u16));
    encoder.encode(image.as_raw(), width, height, ColorType::Rgba)?;

    let (header, data) = split_header(&out).context("malformed JPEG header")?;
    let mut rows = Vec::with_capacity(height.div_ceil(BLOCK_ROWS as u16) as usize);
    // Restart markers split the rows; stuffed 0xff bytes are followed by 0.
    let (mut start, mut index) = (0, 0);
    while let Some(found) = data[index..].iter().position(|&byte| byte == 0xff) {
        let at = index + found;
        index = at + 1;
        match data.get(at + 1) {
            Some(0xd0..=0xd7) => {
                rows.push(data[start..at].to_vec());
                (start, index) = (at + 2, at + 2);
            }
            Some(0xd9) => {
                rows.push(data[start..at].to_vec());
                break;
            }
            _ => {}
        }
    }
    ensure!(
        rows.len() == height.div_ceil(BLOCK_ROWS as u16) as usize,
        "expected one restart interval per block row"
    );
    Ok((header.to_vec(), rows))
}

/// Split after the start-of-scan segment.
fn split_header(jpeg: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut offset = 2;
    loop {
        let (marker, length) = (*jpeg.get(offset + 1)?, segment_length(jpeg, offset)?);
        offset += 2 + length;
        if marker == 0xda {
            return Some(jpeg.split_at(offset));
        }
    }
}

fn segment_length(jpeg: &[u8], offset: usize) -> Option<usize> {
    if *jpeg.get(offset)? != 0xff {
        return None;
    }
    Some(u16::from_be_bytes([*jpeg.get(offset + 2)?, *jpeg.get(offset + 3)?]) as usize)
}

/// Offset of the start-of-frame marker.
fn sof_offset(header: &[u8]) -> Option<usize> {
    let mut offset = 2;
    while offset + 1 < header.len() {
        if matches!(header[offset + 1], 0xc0..=0xc2) {
            return Some(offset);
        }
        offset += 2 + segment_length(header, offset)?;
    }
    None
}

fn set_height(header: &mut [u8], height: u32) -> Result<()> {
    let offset = sof_offset(header).context("JPEG header has no frame")?;
    header[offset + 5..offset + 7].copy_from_slice(&u16::try_from(height)?.to_be_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use image::Rgba;

    use super::*;

    fn gradient(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            Rgba([(x * 7) as u8, (y * 3) as u8, ((x ^ y) & 0xf0) as u8, 255])
        })
    }

    fn whole(image: &RgbaImage) -> Vec<u8> {
        let (header, rows) = encode_rows(image, 90).unwrap();
        let rows: Vec<_> = rows.iter().map(Vec::as_slice).collect();
        let mut out = header;
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                out.extend_from_slice(&[0xff, 0xd0 + (index - 1) as u8 % 8]);
            }
            out.extend_from_slice(row);
        }
        out.extend_from_slice(&EOI);
        out
    }

    #[test]
    fn matches_a_single_encode() {
        for (width, height) in [(1, 1), (9, 7), (37, 50), (300, 129), (640, 333)] {
            let image = gradient(width, height);
            let rows = Rows::new(&image, 90).unwrap();
            let encoded = rows.encode(&[], |_| unreachable!()).unwrap();
            assert_eq!(encoded, whole(&image), "{width}x{height}");
            image::load_from_memory(&encoded).unwrap();
        }
    }

    #[test]
    fn reencodes_dirty_rows() {
        let base = gradient(64, 203);
        let mut changed = base.clone();
        for (x, y) in [(10, 40), (11, 47), (3, 48), (60, 120), (0, 202)] {
            changed.put_pixel(x, y, Rgba([255, 0, 0, 255]));
        }
        let rows = Rows::new(&base, 90).unwrap();
        let compose = |rows: Range<u32>| crop(&changed, rows);
        let dirty = [40..49, 120..121, 202..203];
        assert_eq!(rows.encode(&dirty, compose).unwrap(), whole(&changed));
    }
}
