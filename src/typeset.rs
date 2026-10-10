//! Text measurement and drawing that mimics Pillow's `ImageDraw.text` /
//! `textbbox` semantics closely enough to reuse upstream's layout math
//! (`app/utils/images.py`).

use std::sync::Arc;

use ab_glyph::{Font as _, GlyphId, OutlinedGlyph, PxScale, PxScaleFactor, ScaleFont, point};
use image::{Rgba, RgbaImage};
use imageproc::geometric_transformations::{Interpolation, Projection, warp_into};

use crate::emoji;
use crate::fonts::Font;
use crate::settings;

/// Pilmoji draws emoji at 80% of the font size.
const EMOJI_SCALE: f32 = 0.8;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Rect {
    const ZERO: Rect = Rect {
        x0: 0.0,
        y0: 0.0,
        x1: 0.0,
        y1: 0.0,
    };

    fn union(self, other: Rect) -> Rect {
        Rect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    fn expand(self, amount: f32) -> Rect {
        Rect {
            x0: self.x0 - amount,
            y0: self.y0 - amount,
            x1: self.x1 + amount,
            y1: self.y1 + amount,
        }
    }

    fn offset(self, dx: f32, dy: f32) -> Rect {
        Rect {
            x0: self.x0 + dx,
            y0: self.y0 + dy,
            x1: self.x1 + dx,
            y1: self.y1 + dy,
        }
    }
}

enum Item<'t> {
    Glyph { id: GlyphId, x: f32 },
    Emoji { grapheme: &'t str, x: f32 },
}

struct LineLayout<'t> {
    items: Vec<Item<'t>>,
    advance: f32,
    ink: Option<Rect>,
}

/// A font at a specific pixel size (Pillow's `ImageFont.truetype(path, size)`).
pub struct SizedFont<'f> {
    pub font: &'f Font,
    pub size: u32,
    scale: PxScale,
    scale_factor: PxScaleFactor,
    ascent: f32,
}

impl<'f> SizedFont<'f> {
    pub fn new(font: &'f Font, size: u32) -> Self {
        let scale = font
            .data
            .pt_to_px_scale(size as f32)
            .unwrap_or(PxScale::from(size as f32));
        let scaled = font.data.as_scaled(scale);
        Self {
            font,
            size,
            scale,
            scale_factor: scaled.scale_factor(),
            ascent: scaled.ascent().ceil(),
        }
    }

    /// Upstream `get_stroke_width`, widened from `(size / 12).clamp(1, 3)`.
    pub fn stroke_width(&self) -> u32 {
        (self.size / 10).clamp(1, 6)
    }

    fn emoji_size(&self) -> f32 {
        (self.size as f32 * EMOJI_SCALE).round()
    }

    /// Lay out a single line with the origin at the left of the ascender line.
    fn layout_line<'t>(&self, text: &'t str) -> LineLayout<'t> {
        let scaled = self.font.data.as_scaled(self.scale);
        let mut items = Vec::new();
        let mut ink: Option<Rect> = None;
        let mut x = 0.0f32;
        let mut previous: Option<GlyphId> = None;

        for grapheme in emoji::segments(text) {
            if emoji::is_emoji(grapheme) {
                let size = self.emoji_size();
                let top = self.ascent - size * 0.9;
                let rect = Rect {
                    x0: x,
                    y0: top,
                    x1: x + size,
                    y1: top + size,
                };
                ink = Some(ink.map_or(rect, |r| r.union(rect)));
                items.push(Item::Emoji { grapheme, x });
                x += size;
                previous = None;
                continue;
            }
            for c in grapheme.chars() {
                if c.is_control() {
                    continue;
                }
                let id = scaled.glyph_id(c);
                if let Some(previous) = previous {
                    x += scaled.kern(previous, id);
                }
                if let Some(outline) = self.font.outline(id) {
                    let b = outline.px_bounds(self.scale_factor, point(x, self.ascent));
                    let rect = Rect {
                        x0: b.min.x,
                        y0: b.min.y,
                        x1: b.max.x,
                        y1: b.max.y,
                    };
                    ink = Some(ink.map_or(rect, |r| r.union(rect)));
                }
                items.push(Item::Glyph { id, x });
                x += scaled.h_advance(id);
                previous = Some(id);
            }
        }

        LineLayout {
            items,
            advance: x,
            ink,
        }
    }

    /// Glyphs with an outline, plus emoji: the marks `DrawOptions::visible`
    /// counts, so spaces don't take a step of their own.
    pub fn marks(&self, text: &str) -> usize {
        text.split('\n')
            .map(|line| {
                self.layout_line(line)
                    .items
                    .iter()
                    .filter(|item| match item {
                        Item::Glyph { id, .. } => self.font.outline(*id).is_some(),
                        Item::Emoji { .. } => true,
                    })
                    .count()
            })
            .sum()
    }

    /// Pillow `FreeTypeFont.getlength`.
    fn length(&self, text: &str) -> f32 {
        self.layout_line(text).advance
    }

    /// Pillow `FreeTypeFont.getbbox` (single line, anchor `la`).
    pub fn bbox(&self, text: &str) -> Rect {
        let joined;
        let text = if text.contains('\n') {
            joined = text.replace('\n', "");
            joined.as_str()
        } else {
            text
        };
        let layout = self.layout_line(text);
        match layout.ink {
            Some(ink) => Rect {
                x1: ink.x1.max(layout.advance),
                ..ink
            },
            None => Rect {
                x1: layout.advance,
                ..Rect::ZERO
            },
        }
    }

    fn line_spacing(&self, spacing: f32, stroke: f32) -> f32 {
        self.bbox("A").y1 + 2.0 * stroke + spacing
    }

    /// Pillow `ImageDraw.textbbox((0, 0), text, font, spacing, align, stroke_width)`.
    pub fn text_bbox(&self, text: &str, spacing: f32, stroke: f32, align: &str) -> Rect {
        if !text.contains('\n') {
            return self.bbox(text).expand(stroke);
        }
        let lines: Vec<&str> = text.split('\n').collect();
        let widths: Vec<f32> = lines.iter().map(|line| self.length(line)).collect();
        let max_width = widths.iter().copied().fold(0.0, f32::max);
        let line_spacing = self.line_spacing(spacing, stroke);

        let mut bbox: Option<Rect> = None;
        let mut top = 0.0;
        for (line, width) in lines.iter().zip(&widths) {
            let left = align_offset(align, max_width - width);
            let rect = self.bbox(line).expand(stroke).offset(left, top);
            bbox = Some(bbox.map_or(rect, |b| b.union(rect)));
            top += line_spacing;
        }
        bbox.unwrap_or(Rect::ZERO)
    }

    /// Upstream `get_text_size`.
    pub fn text_size(&self, text: &str) -> (f32, f32) {
        let bbox = self.text_bbox(text, 4.0, 0.0, "left");
        let stroke = self.stroke_width() as f32;
        (bbox.x1 + stroke, bbox.y1 + stroke)
    }

    /// Upstream `get_text_size_minus_font_offset`.
    fn text_size_minus_offset(&self, text: &str) -> (f32, f32) {
        let (width, height) = self.text_size(text);
        let bbox = self.bbox(text);
        (width - bbox.x0, height - bbox.y0)
    }
}

fn align_offset(align: &str, difference: f32) -> f32 {
    match align {
        "center" => difference / 2.0,
        "right" => difference,
        _ => 0.0,
    }
}

/// Upstream `get_font`: the largest size (down to the minimum) that fits.
pub fn fit_font<'f>(
    font: &'f Font,
    text: &str,
    max_size: (u32, u32),
    max_font_size: u32,
) -> SizedFont<'f> {
    let (width, height) = (max_size.0 as f32, max_size.1 as f32);
    let max_width = width - width / 35.0;
    let max_height = height - height / 10.0;
    let fits = |size: u32| {
        let (w, h) = SizedFont::new(font, size).text_size_minus_offset(text);
        w <= max_width && h <= max_height
    };

    // Binary search for the largest fitting size; text extents grow
    // monotonically with size, so this matches upstream's linear scan.
    let minimum = settings::MINIMUM_FONT_SIZE;
    let (mut low, mut high) = (minimum, max_font_size.max(minimum));
    if !fits(low) {
        return SizedFont::new(font, minimum);
    }
    while low < high {
        let mid = (low + high).div_ceil(2);
        if fits(mid) { low = mid } else { high = mid - 1 }
    }
    SizedFont::new(font, low)
}

/// Upstream `get_text_offset`.
pub fn text_offset(text: &str, font: &SizedFont, max_size: (u32, u32), align: &str) -> (f32, f32) {
    let (text_width, text_height) = font.text_size(text);
    let stroke = font.stroke_width() as f32;
    let bbox = font.bbox(text);
    let mut x_offset = bbox.x0 - stroke;
    let mut y_offset = bbox.y0 - stroke;

    let lines: Vec<&str> = text.split('\n').collect();
    let rows = lines.len();
    let last = lines.last().copied().unwrap_or("");
    let y_adjust = if rows >= 3 {
        1.1
    } else {
        1.0 + (3 - rows) as f32 * 0.25
    };

    if align != "left" {
        x_offset -= (max_size.0 as f32 - text_width) / 2.0;
    }

    // Upstream's `y_adjust` makes up for the empty ascender space above
    // Titillium's capitals. Impact's capitals fill the ascent, so the same
    // nudge sets text low; center the stroked ink instead. Render draws with
    // `spacing = -y_offset / (rows * 2)`, so solve for the origin `y` where
    // ink top + ink bottom = box height.
    if font.font.is_impact() {
        let rows = rows as f32;
        let ink_top = font.bbox(lines[0]).y0 - stroke;
        let ink_bottom = font.bbox(last).y1 + stroke;
        let line_spacing = font.line_spacing(0.0, stroke);
        let y = (max_size.1 as f32 - ink_top - ink_bottom - (rows - 1.0) * line_spacing)
            / (2.0 + (rows - 1.0) / (2.0 * rows));
        return (x_offset, -y);
    }

    y_offset -= (max_size.1 as f32 - text_height / y_adjust) / 2.0;

    if last.chars().any(|c| "gjpqy".contains(c)) {
        y_offset += (text_height / 20.0).floor();
    }
    (x_offset, y_offset)
}

/// Upstream `wrap`: choose between 1, 2, or 3 lines of text.
pub fn wrap(font: &Font, line: &str, max_size: (u32, u32), max_font_size: u32) -> String {
    let lines_1 = line.to_string();
    let lines_2 = split_2(line);
    let lines_3 = split_3(line);

    let font_1 = fit_font(font, &lines_1, max_size, max_font_size);
    let font_2 = fit_font(font, &lines_2, max_size, max_font_size);
    let font_3 = fit_font(font, &lines_3, max_size, max_font_size);

    if font_1.size == font_2.size && font_2.size <= settings::MINIMUM_FONT_SIZE {
        return lines_2;
    }
    if font_1.size >= font_2.size {
        return lines_1;
    }
    let threshold = max_size.0 as f32 * 0.60;
    if !unsafe_wrap(&lines_3) && font_3.text_size(&lines_3).0 >= threshold {
        return lines_3;
    }
    if font_2.text_size(&lines_2).0 >= threshold {
        return lines_2;
    }
    lines_1
}

/// Lines consisting solely of emoji should not be split.
fn unsafe_wrap(text: &str) -> bool {
    text.split('\n').any(|line| {
        let stripped = line.trim();
        !stripped.is_empty()
            && emoji::segments(stripped).any(emoji::is_emoji)
            && emoji::segments(stripped).all(|g| emoji::is_emoji(g) || g.trim().is_empty())
    })
}

fn split_2(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let length = chars.len() as i64;
    let midpoint = length / 2 - 1;
    for offset in 0..length / 4 {
        for index in [midpoint - offset, midpoint + offset] {
            if index >= 0 && chars[index as usize] == ' ' {
                let (left, right): (String, String) = (
                    chars[..index as usize].iter().collect(),
                    chars[index as usize..].iter().collect(),
                );
                return format!("{}\n{}", left.trim(), right.trim());
            }
        }
    }
    line.to_string()
}

fn split_3(line: &str) -> String {
    let max_len = line.chars().count() as f32 / 3.0;
    let mut lines = [String::new(), String::new(), String::new()];
    let mut index = 0;
    for word in line.split(' ') {
        let current = lines[index].chars().count() as f32;
        let next = current + word.chars().count() as f32 * 0.7;
        if next > max_len && index < 2 {
            index += 1;
        }
        lines[index].push_str(word);
        lines[index].push(' ');
    }
    lines.join("\n").trim().to_string()
}

pub fn parse_color(value: &str) -> Option<[u8; 4]> {
    csscolorparser::parse(value)
        .ok()
        .map(|color| color.to_rgba8())
}

/// Pillow's `paste(color, mask)` blend: every channel, alpha included.
#[inline]
fn blend(dst: &mut [u8], src: [u8; 4], mask: f32) {
    if mask <= 0.0 {
        return;
    }
    let mask = mask.min(1.0);
    for (d, s) in dst.iter_mut().zip(src) {
        *d = (s as f32 * mask + *d as f32 * (1.0 - mask)).round() as u8;
    }
}

pub struct DrawOptions<'a> {
    pub fill: [u8; 4],
    pub stroke_width: u32,
    pub stroke_fill: [u8; 4],
    pub spacing: f32,
    pub align: &'a str,
    /// Draw only the first this many marks (see [`SizedFont::marks`]), laid
    /// out where they'd be in the full text.
    pub visible: Option<usize>,
}

/// Pillow `ImageDraw.text(xy, text, ...)` with Pilmoji-style emoji images,
/// looked up by grapheme and pixel size.
pub fn draw_text(
    canvas: &mut RgbaImage,
    xy: (f32, f32),
    text: &str,
    font: &SizedFont,
    options: &DrawOptions,
    emoji_image: &dyn Fn(&str, u32) -> Option<Arc<RgbaImage>>,
) {
    let (width, height) = canvas.dimensions();
    if width == 0 || height == 0 || text.trim().is_empty() {
        return;
    }
    let stroke = options.stroke_width as f32;
    let margin = options.stroke_width as i64;
    let mask_width = width as i64 + 2 * margin;
    let mask_height = height as i64 + 2 * margin;
    let mut coverage = vec![0f32; (mask_width * mask_height) as usize];
    let mut emoji_draws: Vec<(i64, i64, &str)> = Vec::new();
    // Bounds of the drawn coverage in mask coordinates: (x0, y0, x1, y1).
    let mut ink: Option<(i64, i64, i64, i64)> = None;

    let lines: Vec<&str> = text.split('\n').collect();
    let layouts: Vec<LineLayout> = lines.iter().map(|line| font.layout_line(line)).collect();
    let max_width = layouts.iter().map(|l| l.advance).fold(0.0, f32::max);
    let line_spacing = font.line_spacing(options.spacing, stroke);
    let emoji_size = font.emoji_size();

    let mut top = xy.1;
    let mut marks = 0;
    'lines: for layout in &layouts {
        let left = xy.0
            + if lines.len() > 1 {
                align_offset(options.align, max_width - layout.advance)
            } else {
                0.0
            };
        for item in &layout.items {
            match *item {
                Item::Glyph { id, x } => {
                    let glyph = id.with_scale_and_position(
                        font.scale,
                        point(left + x + margin as f32, top + font.ascent + margin as f32),
                    );
                    let Some(outline) = font.font.outline(id) else {
                        continue;
                    };
                    if options.visible.is_some_and(|visible| marks >= visible) {
                        break 'lines;
                    }
                    marks += 1;
                    let outlined = OutlinedGlyph::new(glyph, outline.clone(), font.scale_factor);
                    let bounds = outlined.px_bounds();
                    let (x0, y0) = (bounds.min.x as i64, bounds.min.y as i64);
                    let (x1, y1) = (bounds.max.x as i64, bounds.max.y as i64);
                    let (x0, y0) = (x0.max(0), y0.max(0));
                    let (x1, y1) = (x1.min(mask_width), y1.min(mask_height));
                    if x0 < x1 && y0 < y1 {
                        ink = Some(ink.map_or((x0, y0, x1, y1), |(a, b, c, d)| {
                            (a.min(x0), b.min(y0), c.max(x1), d.max(y1))
                        }));
                    }
                    outlined.draw(|gx, gy, value| {
                        let px = bounds.min.x as i64 + gx as i64;
                        let py = bounds.min.y as i64 + gy as i64;
                        if px >= 0 && py >= 0 && px < mask_width && py < mask_height {
                            let cell = &mut coverage[(py * mask_width + px) as usize];
                            *cell = cell.max(value);
                        }
                    });
                }
                Item::Emoji { grapheme, x } => {
                    if options.visible.is_some_and(|visible| marks >= visible) {
                        break 'lines;
                    }
                    marks += 1;
                    let y = top + font.ascent - emoji_size * 0.9;
                    emoji_draws.push(((left + x).round() as i64, y.round() as i64, grapheme));
                }
            }
        }
        top += line_spacing;
    }

    if let Some(ink) = ink {
        let stroke_mask = (options.stroke_width > 0)
            .then(|| dilate(&coverage, mask_width, mask_height, stroke, ink));
        // The stroke reaches at most `reach` pixels past the coverage.
        let reach = if stroke_mask.is_some() {
            (stroke + 0.5).ceil() as i64
        } else {
            0
        };
        let (x0, x1) = (
            (ink.0 - reach - margin).max(0),
            (ink.2 + reach - margin).min(width as i64),
        );
        let (y0, y1) = (
            (ink.1 - reach - margin).max(0),
            (ink.3 + reach - margin).min(height as i64),
        );
        let row_bytes = width as usize * 4;
        for y in y0..y1 {
            let row = &mut canvas.as_mut()[y as usize * row_bytes..(y as usize + 1) * row_bytes];
            let start = ((y + margin) * mask_width + margin) as usize;
            for x in x0 as usize..x1.max(x0) as usize {
                let pixel = &mut row[x * 4..x * 4 + 4];
                if let Some(mask) = &stroke_mask {
                    blend(pixel, options.stroke_fill, mask[start + x]);
                }
                blend(pixel, options.fill, coverage[start + x]);
            }
        }
    }

    for (x, y, grapheme) in emoji_draws {
        if let Some(image) = emoji_image(grapheme, emoji_size as u32) {
            paste_with_alpha(canvas, &image, (x, y));
        }
    }
}

/// Anti-aliased morphological dilation approximating FreeType's stroker.
/// Only pixels inside `ink` (x0, y0, x1, y1) may be covered.
fn dilate(
    coverage: &[f32],
    width: i64,
    height: i64,
    radius: f32,
    (x0, y0, x1, y1): (i64, i64, i64, i64),
) -> Vec<f32> {
    let reach = (radius + 0.5).ceil() as i64;
    let mut kernel = Vec::new();
    for dy in -reach..=reach {
        for dx in -reach..=reach {
            let distance = ((dx * dx + dy * dy) as f32).sqrt();
            let weight = (radius + 0.5 - distance).clamp(0.0, 1.0);
            if weight > 0.0 {
                kernel.push((dx, dy, (dy * width + dx) as isize, weight));
            }
        }
    }
    // A cell `d` away from a pixel is at least one step closer to the
    // neighbour on that side, so when that neighbour is at least as covered it
    // reaches the cell at least as strongly. Only spread in the other
    // directions: kernels[sides] skips offsets toward each side in `sides`
    // (left, right, up, down bits).
    let kernels: Vec<Vec<_>> = (0..16)
        .map(|sides: u8| {
            kernel
                .iter()
                .copied()
                .filter(|&(dx, dy, ..)| {
                    !((sides & 1 != 0 && dx < 0)
                        || (sides & 2 != 0 && dx > 0)
                        || (sides & 4 != 0 && dy < 0)
                        || (sides & 8 != 0 && dy > 0))
                })
                .collect()
        })
        .collect();

    let stride = width as usize;
    let mut out = vec![0f32; coverage.len()];
    for y in y0..y1 {
        for x in x0..x1 {
            let index = (y * width + x) as usize;
            let value = coverage[index];
            if value <= 0.0 {
                continue;
            }
            let covers = |inside: bool, neighbour: usize| inside && coverage[neighbour] >= value;
            let sides = covers(x > 0, index.wrapping_sub(1)) as usize
                | (covers(x < width - 1, index + 1) as usize) << 1
                | (covers(y > 0, index.wrapping_sub(stride)) as usize) << 2
                | (covers(y < height - 1, index + stride) as usize) << 3;
            let kernel = &kernels[sides];
            if x >= reach && y >= reach && x < width - reach && y < height - reach {
                for &(_, _, offset, weight) in kernel {
                    let cell = &mut out[index.wrapping_add_signed(offset)];
                    *cell = cell.max(value * weight);
                }
                continue;
            }
            for &(dx, dy, _, weight) in kernel {
                let (nx, ny) = (x + dx, y + dy);
                if nx >= 0 && ny >= 0 && nx < width && ny < height {
                    let cell = &mut out[(ny * width + nx) as usize];
                    *cell = cell.max(value * weight);
                }
            }
        }
    }
    out
}

/// Pillow `Image.paste(im, point, mask=im)`: blend using the source alpha.
pub fn paste_with_alpha(dst: &mut RgbaImage, src: &RgbaImage, (left, top): (i64, i64)) {
    let (dst_width, dst_height) = (dst.width() as i64, dst.height() as i64);
    let (x0, x1) = (left.max(0), (left + src.width() as i64).min(dst_width));
    let (y0, y1) = (top.max(0), (top + src.height() as i64).min(dst_height));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let (dst_stride, src_stride) = (dst_width as usize * 4, src.width() as usize * 4);
    let span = (x1 - x0) as usize * 4;
    let (dst_x, src_x) = (x0 as usize * 4, (x0 - left) as usize * 4);
    let dst = dst.as_mut();
    for y in y0..y1 {
        let d = y as usize * dst_stride + dst_x;
        let s = (y - top) as usize * src_stride + src_x;
        let pixels = dst[d..d + span]
            .chunks_exact_mut(4)
            .zip(src.as_raw()[s..s + span].chunks_exact(4));
        for (target, pixel) in pixels {
            match pixel[3] {
                0 => {}
                255 => target.copy_from_slice(pixel),
                alpha => blend(target, pixel.try_into().unwrap(), alpha as f32 / 255.0),
            }
        }
    }
}

/// Crop to the non-transparent pixels, returning their offset.
pub fn trim(image: &RgbaImage) -> Option<((u32, u32), RgbaImage)> {
    let (width, height) = image.dimensions();
    let row_bytes = width as usize * 4;
    let row = |y: u32| &image.as_raw()[y as usize * row_bytes..(y as usize + 1) * row_bytes];
    let opaque = |pixel: &[u8]| pixel[3] != 0;
    let inked = |y: &u32| row(*y).chunks_exact(4).any(opaque);
    let y0 = (0..height).find(inked)?;
    let y1 = (y0..height).rev().find(inked)? + 1;
    let (mut x0, mut x1) = (width, 0);
    for y in y0..y1 {
        // Only scan the columns outside those already known to be inked.
        let pixels = row(y);
        if let Some(x) = pixels[..x0 as usize * 4].chunks_exact(4).position(opaque) {
            x0 = x as u32;
        }
        let right = x1.max(x0) as usize * 4;
        if let Some(x) = pixels[right..].chunks_exact(4).rposition(opaque) {
            x1 = (right / 4 + x + 1) as u32;
        }
    }
    if (x0, y0, x1, y1) == (0, 0, width, height) {
        return Some(((0, 0), image.clone()));
    }
    let cropped = image::imageops::crop_imm(image, x0, y0, x1 - x0, y1 - y0).to_image();
    Some(((x0, y0), cropped))
}

/// Pillow `Image.rotate(angle, BICUBIC, expand=True)` (counter-clockwise).
pub fn rotate_expand(image: RgbaImage, angle: f32) -> RgbaImage {
    let angle = angle.rem_euclid(360.0);
    if angle == 0.0 {
        return image;
    }
    let (width, height) = (image.width() as f32, image.height() as f32);
    let theta = -angle.to_radians();
    let (sin, cos) = theta.sin_cos();
    let corners = [(0.0, 0.0), (width, 0.0), (width, height), (0.0, height)];
    let (cx, cy) = (width / 2.0, height / 2.0);
    let rotated: Vec<(f32, f32)> = corners
        .iter()
        .map(|&(x, y)| {
            (
                (x - cx) * cos - (y - cy) * sin,
                (x - cx) * sin + (y - cy) * cos,
            )
        })
        .collect();
    let min_x = rotated.iter().map(|p| p.0).fold(f32::MAX, f32::min);
    let max_x = rotated.iter().map(|p| p.0).fold(f32::MIN, f32::max);
    let min_y = rotated.iter().map(|p| p.1).fold(f32::MAX, f32::min);
    let max_y = rotated.iter().map(|p| p.1).fold(f32::MIN, f32::max);
    let new_width = ((max_x.ceil() - min_x.floor()) as u32).max(1);
    let new_height = ((max_y.ceil() - min_y.floor()) as u32).max(1);

    let projection = Projection::translate(new_width as f32 / 2.0, new_height as f32 / 2.0)
        * Projection::rotate(theta)
        * Projection::translate(-cx, -cy);
    let mut out = RgbaImage::new(new_width, new_height);
    warp_into(
        &image,
        projection,
        Interpolation::Bicubic,
        imageproc::geometric_transformations::Border::Constant(Rgba([0, 0, 0, 0])),
        &mut out,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The straightforward dilation `dilate` must match exactly.
    fn dilate_reference(coverage: &[f32], width: i64, height: i64, radius: f32) -> Vec<f32> {
        let reach = (radius + 0.5).ceil() as i64;
        let mut out = vec![0f32; coverage.len()];
        for y in 0..height {
            for x in 0..width {
                let value = coverage[(y * width + x) as usize];
                for dy in -reach..=reach {
                    for dx in -reach..=reach {
                        let distance = ((dx * dx + dy * dy) as f32).sqrt();
                        let weight = (radius + 0.5 - distance).clamp(0.0, 1.0);
                        let (nx, ny) = (x + dx, y + dy);
                        if value > 0.0
                            && weight > 0.0
                            && nx >= 0
                            && ny >= 0
                            && nx < width
                            && ny < height
                        {
                            let cell = &mut out[(ny * width + nx) as usize];
                            *cell = cell.max(value * weight);
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn dilates_like_reference() {
        let mut seed = 12345u32;
        let mut random = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) as f32 / 65536.0
        };
        let (width, height) = (61, 37);
        for radius in [1.0, 2.0, 3.0] {
            for density in [0.1, 0.5, 0.9] {
                // Blobs of full coverage with soft edges, plus noise.
                let coverage: Vec<f32> = (0..width * height)
                    .map(|_| match random() {
                        r if r < density * 0.7 => 1.0,
                        r if r < density => random(),
                        _ => 0.0,
                    })
                    .collect();
                assert_eq!(
                    dilate(&coverage, width, height, radius, (0, 0, width, height)),
                    dilate_reference(&coverage, width, height, radius)
                );
            }
        }
    }

    #[test]
    fn trims_transparent_edges() {
        let mut image = RgbaImage::new(20, 10);
        assert!(trim(&image).is_none());
        for (x, y) in [(5, 2), (3, 4), (12, 7), (8, 6)] {
            image.put_pixel(x, y, Rgba([1, 2, 3, 4]));
        }
        let ((left, top), cropped) = trim(&image).unwrap();
        assert_eq!(
            (left, top, cropped.width(), cropped.height()),
            (3, 2, 10, 6)
        );
        assert_eq!(cropped.get_pixel(9, 5), &Rgba([1, 2, 3, 4]));
        image.put_pixel(0, 0, Rgba([0, 0, 0, 1]));
        image.put_pixel(19, 9, Rgba([0, 0, 0, 1]));
        assert_eq!(trim(&image).unwrap(), ((0, 0), image));
    }

    #[test]
    fn splits_like_upstream() {
        assert_eq!(split_2("not sure if trolling"), "not sure\nif trolling");
        assert_eq!(split_2("abc"), "abc");
        assert_eq!(
            split_3("one two three four five six"),
            "one two \nthree four \nfive six"
        );
    }
}
