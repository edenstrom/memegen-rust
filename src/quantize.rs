//! Median-cut palette quantization (the approach Pillow uses for GIFs) over a
//! 15-bit color histogram, with a lookup table for fast pixel mapping.

const BITS: u32 = 5;
const LEVELS: usize = 1 << BITS;
const BINS: usize = LEVELS * LEVELS * LEVELS;

#[inline]
fn bin(r: u8, g: u8, b: u8) -> usize {
    let shift = 8 - BITS;
    ((r >> shift) as usize) << (2 * BITS) | ((g >> shift) as usize) << BITS | (b >> shift) as usize
}

#[inline]
fn unbin(index: usize) -> [usize; 3] {
    [
        index >> (2 * BITS),
        (index >> BITS) & (LEVELS - 1),
        index & (LEVELS - 1),
    ]
}

pub struct Palette {
    /// RGB triples, at most `colors` entries.
    pub colors: Vec<[u8; 3]>,
    lookup: Vec<u8>,
}

impl Palette {
    /// Build a palette of up to `max_colors` from RGB(A) pixel data.
    pub fn build<'a>(pixels: impl Iterator<Item = &'a [u8]>, max_colors: usize) -> Self {
        let mut histogram = vec![0u64; BINS];
        let mut sums = vec![[0u64; 3]; BINS];
        for pixel in pixels {
            let index = bin(pixel[0], pixel[1], pixel[2]);
            histogram[index] += 1;
            for channel in 0..3 {
                sums[index][channel] += pixel[channel] as u64;
            }
        }

        let used: Vec<usize> = (0..BINS).filter(|&index| histogram[index] > 0).collect();
        let mut boxes = vec![used];
        while boxes.len() < max_colors {
            // Split the most populous box that spans more than one bin.
            let candidate = boxes
                .iter()
                .enumerate()
                .filter(|(_, b)| b.len() > 1)
                .max_by_key(|(_, b)| b.iter().map(|&i| histogram[i]).sum::<u64>())
                .map(|(index, _)| index);
            let Some(index) = candidate else { break };
            let mut target = boxes.swap_remove(index);

            let mut low = [usize::MAX; 3];
            let mut high = [0usize; 3];
            for &i in &target {
                let c = unbin(i);
                for axis in 0..3 {
                    low[axis] = low[axis].min(c[axis]);
                    high[axis] = high[axis].max(c[axis]);
                }
            }
            let axis = (0..3).max_by_key(|&a| high[a] - low[a]).unwrap_or(0);
            target.sort_unstable_by_key(|&i| unbin(i)[axis]);

            let total: u64 = target.iter().map(|&i| histogram[i]).sum();
            let mut running = 0;
            let mut split = 1;
            for (position, &i) in target.iter().enumerate() {
                running += histogram[i];
                if running * 2 >= total {
                    split = (position + 1).clamp(1, target.len() - 1);
                    break;
                }
            }
            let upper = target.split_off(split);
            boxes.push(target);
            boxes.push(upper);
        }

        let colors: Vec<[u8; 3]> = boxes
            .iter()
            .filter(|b| !b.is_empty())
            .map(|b| {
                let count: u64 = b.iter().map(|&i| histogram[i]).sum::<u64>().max(1);
                let mut rgb = [0u8; 3];
                for (channel, value) in rgb.iter_mut().enumerate() {
                    *value = (b.iter().map(|&i| sums[i][channel]).sum::<u64>() / count) as u8;
                }
                rgb
            })
            .collect();

        let colors = if colors.is_empty() {
            vec![[0, 0, 0]]
        } else {
            colors
        };
        let lookup = (0..BINS)
            .map(|index| {
                let center = unbin(index).map(|c| ((c << (8 - BITS)) + (1 << (7 - BITS))) as i32);
                colors
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, color)| {
                        (0..3)
                            .map(|a| (color[a] as i32 - center[a]).pow(2))
                            .sum::<i32>()
                    })
                    .map(|(position, _)| position as u8)
                    .unwrap_or(0)
            })
            .collect();
        Self { colors, lookup }
    }

    #[inline]
    pub fn index_of(&self, r: u8, g: u8, b: u8) -> u8 {
        self.lookup[bin(r, g, b)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_distinct_colors() {
        let pixels: Vec<[u8; 4]> = vec![[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]];
        let palette = Palette::build(pixels.iter().map(|p| &p[..]), 8);
        assert_eq!(palette.colors.len(), 3);
        for p in &pixels {
            let color = palette.colors[palette.index_of(p[0], p[1], p[2]) as usize];
            assert_eq!(&color[..], &p[..3]);
        }
    }
}
