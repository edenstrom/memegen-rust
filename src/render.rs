//! Meme rendering pipeline with in-memory caches.

use std::io::Cursor;
use std::ops::Range;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use image::codecs::gif::GifDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use rayon::prelude::*;

use crate::assets::Source;
use crate::cache::Cache;
use crate::config::Config;
use crate::emoji::EmojiImages;
use crate::fonts::Fonts;
use crate::quantize::Palette;
use crate::template::{Catalog, Template};
use crate::textbox::TextBox;
use crate::typeset::{self, DrawOptions, SizedFont};
use crate::{jpeg, png, settings, slug};

const MAXIMUM_FRAMES: usize = 20;
const MINIMUM_FRAMES: usize = 5;
const JPEG_QUALITY: u8 = 95;

/// Animated text: delay per mark on a still background, the pause between
/// text boxes, and how long the finished text holds before the animation
/// loops.
const TYPING_DELAY_MS: u32 = 60;
const TYPING_GAP_MS: u32 = 600;
const TYPING_HOLD_MS: u32 = 3000;

/// Eased animated text (MP4): the frame step over an animated background
/// (30 fps, so the typing keeps its curve between background frames).
const EASING_STEP_MS: u32 = 33;

/// Frame budgets for animated text: frames while typing (longer text types
/// several marks per frame), and frames in total including the hold on an
/// animated background. Each frame is a full-size image in memory.
#[cfg(not(target_arch = "wasm32"))]
const TYPING_FRAMES: (usize, usize) = (60, 120);
#[cfg(target_arch = "wasm32")]
const TYPING_FRAMES: (usize, usize) = (24, 40);

/// Frame budget for animated WebP (0 means upstream's default sampling).
/// Upstream keeps 4x more frames than for GIF; the lossless encoder used on
/// WebAssembly sticks to the GIF budget to bound memory and file size.
#[cfg(not(target_arch = "wasm32"))]
const WEBP_MAXIMUM_FRAMES: usize = MAXIMUM_FRAMES * 4;
#[cfg(target_arch = "wasm32")]
const WEBP_MAXIMUM_FRAMES: usize = 0;

/// Cache budgets in MB for decoded backgrounds, decoded animations, encoded
/// static backgrounds and encoded output. A Workers isolate has 128 MB of memory
/// in total.
#[cfg(not(target_arch = "wasm32"))]
const CACHE_MB: (u64, u64, u64, u64) = (256, 512, 128, 256);
#[cfg(target_arch = "wasm32")]
const CACHE_MB: (u64, u64, u64, u64) = (16, 32, 4, 8);

#[derive(Debug, Clone)]
pub struct Rendered {
    pub bytes: Bytes,
    pub content_type: &'static str,
    pub extension: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("Template not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error(
        "Custom text too long (a line exceeds {} bytes)",
        settings::MAX_SLUG_PART_BYTES
    )]
    TooLong,
    #[error("Render failed: {0}")]
    Internal(String),
}

impl RenderError {
    pub fn status(&self) -> u16 {
        match self {
            Self::NotFound(_) => 404,
            Self::Invalid(_) => 422,
            Self::TooLong => 414,
            Self::Internal(_) => 500,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemeRequest {
    pub template_id: String,
    pub lines: Vec<String>,
    pub font: String,
    pub extension: String,
    /// Type the text out one mark at a time (GIF, WebP and MP4 only).
    pub animate_text: bool,
}

fn maximum_frames(extension: &str) -> usize {
    if extension == "webp" {
        WEBP_MAXIMUM_FRAMES
    } else {
        0
    }
}

/// Whether `extension` is an animated format.
pub fn is_animated(extension: &str) -> bool {
    matches!(extension, "gif" | "webp" | "mp4")
}

pub fn content_type(extension: &str) -> &'static str {
    match extension {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        _ => "image/png",
    }
}

/// A static background encoded once, so renders only encode rows with text.
enum Encoded {
    Png(png::Strips),
    Jpeg(jpeg::Rows),
}

impl Encoded {
    fn size(&self) -> usize {
        match self {
            Self::Png(strips) => strips.size(),
            Self::Jpeg(rows) => rows.size(),
        }
    }
}

struct Animation {
    /// `(source frame index, resized frame)` for the sampled frames.
    frames: Vec<(usize, RgbaImage)>,
    total: usize,
    duration: u32,
}

impl Animation {
    /// Upstream's delay for the sampled frames, longer when frames were
    /// dropped so the animation keeps its speed.
    fn frame_duration(&self) -> u32 {
        let sampled = self.frames.len();
        if sampled <= MINIMUM_FRAMES {
            return self.duration;
        }
        let ratio = sampled as f64 / self.total.max(MAXIMUM_FRAMES) as f64;
        (250.0f64).min((self.duration as f64 / ratio).floor()) as u32
    }
}

pub struct Renderer {
    config: Config,
    catalog: Arc<Catalog>,
    fonts: Arc<Fonts>,
    emoji: EmojiImages,
    backgrounds: Cache<String, Arc<RgbaImage>>,
    animations: Cache<(String, usize), Arc<Animation>>,
    encoded: Cache<(String, &'static str), Arc<Encoded>>,
    outputs: Cache<String, Arc<Rendered>>,
}

/// A validated request and its output cache key.
struct Prepared {
    template: Arc<Template>,
    font: String,
    extension: String,
    animate_text: bool,
    key: String,
}

/// How animated text is typed out, a mark at a time, one text box after
/// another.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Reveal {
    /// Every mark after the same delay (GIF and WebP).
    Typing,
    /// The marks speed up and then slow down again, easing in and out (MP4,
    /// where the extra frames compress well).
    Easing,
}

impl Reveal {
    /// When mark `index` (from 1) of `marks` appears, as a fraction of the
    /// box's typing time. Easing is the inverse of an ease-in-out sine.
    fn appears(self, index: usize, marks: usize) -> f64 {
        let progress = index as f64 / marks as f64;
        match self {
            Reveal::Typing => progress,
            Reveal::Easing => (1.0 - 2.0 * progress).acos() / std::f64::consts::PI,
        }
    }
}

/// A text box's text, wrapped and sized, ready to draw in full or in part.
struct TextLayout<'a> {
    text: &'a TextBox,
    point: (i64, i64),
    size: (u32, u32),
    line: String,
    font: SizedFont<'a>,
    offset: (f32, f32),
}

impl Renderer {
    pub fn new(config: Config, catalog: Arc<Catalog>, fonts: Arc<Fonts>) -> Self {
        const MB: u64 = 1024 * 1024;
        let (backgrounds, animations, encoded, outputs) = CACHE_MB;
        Self {
            config,
            catalog,
            fonts,
            emoji: EmojiImages::default(),
            backgrounds: Cache::new(backgrounds * MB, |_, image| image.as_raw().len() as u32),
            animations: Cache::new(animations * MB, |_, animation| {
                animation
                    .frames
                    .iter()
                    .map(|(_, frame)| frame.as_raw().len() as u32)
                    .fold(0u32, u32::saturating_add)
            }),
            encoded: Cache::new(encoded * MB, |_, encoded| encoded.size() as u32),
            outputs: Cache::new(outputs * MB, |_, rendered| rendered.bytes.len() as u32),
        }
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn fonts(&self) -> &Fonts {
        &self.fonts
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn prepare(&self, request: &MemeRequest) -> Result<Prepared, RenderError> {
        let extension = request.extension.to_lowercase();
        if !settings::ALLOWED_EXTENSIONS.contains(&extension.as_str()) {
            return Err(RenderError::Invalid(format!(
                "Invalid extension: {extension} (expected one of {})",
                settings::ALLOWED_EXTENSIONS.join(", ")
            )));
        }
        let template = self
            .catalog
            .get(&request.template_id)
            .filter(|template| template.static_image.is_some())
            .ok_or_else(|| RenderError::NotFound(request.template_id.clone()))?
            .clone();

        let font = match request.font.as_str() {
            "" | settings::PLACEHOLDER => String::new(),
            name => self
                .fonts
                .get(name)
                .map(|font| font.id.to_string())
                .ok_or_else(|| RenderError::Invalid(format!("Invalid font: {name}")))?,
        };

        if request.animate_text && !is_animated(&extension) {
            return Err(RenderError::Invalid(format!(
                "Animated text needs a gif, webp or mp4 extension, not {extension}"
            )));
        }

        let slug = slug::encode(&request.lines);
        if slug
            .split('/')
            .any(|part| part.len() > settings::MAX_SLUG_PART_BYTES)
        {
            return Err(RenderError::TooLong);
        }
        let mut key = format!("{}|{slug}|{font}|{extension}", template.id);
        if request.animate_text {
            key.push_str("|animate_text");
        }
        Ok(Prepared {
            template,
            font,
            extension,
            animate_text: request.animate_text,
            key,
        })
    }

    /// Asset paths a render of `request` would read that aren't cached yet,
    /// for sources that must load files before rendering.
    pub fn missing(&self, request: &MemeRequest) -> Vec<String> {
        let Ok(prepared) = self.prepare(request) else {
            return vec![];
        };
        if !self.config.debug && self.outputs.contains(&prepared.key) {
            return vec![];
        }
        let template = &prepared.template;
        let extension = prepared.extension.as_str();
        let mut paths: Vec<String> = match extension {
            "gif" | "webp" | "mp4" => {
                let key = (template.id.clone(), maximum_frames(extension));
                (!self.animations.contains(&key))
                    .then(|| template.animated_image.clone())
                    .flatten()
            }
            _ => (!self.backgrounds.contains(&template.id))
                .then(|| template.static_image.clone())
                .flatten(),
        }
        .into_iter()
        .chain(
            request
                .lines
                .iter()
                .flat_map(|line| self.emoji.missing(line)),
        )
        .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// The cached output for `request`, if there is one.
    pub fn cached(&self, request: &MemeRequest) -> Option<Arc<Rendered>> {
        if self.config.debug {
            return None;
        }
        self.outputs.get(&self.prepare(request).ok()?.key)
    }

    /// Validate a request, then render it (or fetch it from cache).
    pub fn render(
        &self,
        request: &MemeRequest,
        source: &dyn Source,
    ) -> Result<Arc<Rendered>, RenderError> {
        let Prepared {
            template,
            font,
            extension,
            animate_text,
            key,
        } = self.prepare(request)?;
        let render = || {
            self.render_logged(
                &template,
                &request.lines,
                &font,
                &extension,
                animate_text,
                source,
            )
            .map(Arc::new)
        };
        if self.config.debug {
            return render().map_err(|error| RenderError::Internal(format!("{error:#}")));
        }
        self.outputs
            .try_get_with(key, render)
            .map_err(|error: Arc<anyhow::Error>| RenderError::Internal(format!("{error:#}")))
    }

    fn render_logged(
        &self,
        template: &Template,
        lines: &[String],
        font: &str,
        extension: &str,
        animate_text: bool,
        source: &dyn Source,
    ) -> Result<Rendered> {
        // `Instant::now` panics on wasm32-unknown-unknown.
        #[cfg(not(target_arch = "wasm32"))]
        let started = std::time::Instant::now();
        let bytes = self.render_bytes(template, lines, font, extension, animate_text, source)?;
        #[cfg(not(target_arch = "wasm32"))]
        tracing::info!(
            "Rendered {}/{} ({extension}, {} bytes) in {:.1?}",
            template.id,
            slug::encode(lines),
            bytes.len(),
            started.elapsed()
        );
        Ok(Rendered {
            bytes: Bytes::from(bytes),
            content_type: content_type(extension),
            extension: extension.to_string(),
        })
    }

    /// Render and encode without consulting or filling the output caches.
    pub fn render_bytes(
        &self,
        template: &Template,
        lines: &[String],
        font: &str,
        extension: &str,
        animate_text: bool,
        source: &dyn Source,
    ) -> Result<Vec<u8>> {
        match extension {
            "gif" | "webp" | "mp4" => {
                let maximum_frames = maximum_frames(extension);
                let (frames, delays) = if animate_text {
                    let reveal = if extension == "mp4" {
                        Reveal::Easing
                    } else {
                        Reveal::Typing
                    };
                    self.render_typing(template, lines, font, maximum_frames, reveal, source)?
                } else {
                    let (frames, duration) =
                        self.render_animation(template, lines, font, maximum_frames, source)?;
                    let delays = vec![duration; frames.len()];
                    (frames, delays)
                };
                match extension {
                    "gif" => encode_gif(frames, &delays),
                    "webp" => crate::webp::encode(&frames, &delays),
                    _ => crate::mp4::encode(&frames, &delays),
                }
            }
            _ => self.render_static(template, lines, font, extension, source),
        }
    }

    /// Drop decoded backgrounds and rendered outputs (for cold benchmarks).
    pub fn clear_caches(&self) {
        self.backgrounds.invalidate_all();
        self.animations.invalidate_all();
        self.encoded.invalidate_all();
        self.outputs.invalidate_all();
    }

    /// Upstream `render_image` (static output). Only the rows covered by text
    /// are encoded; the rest is copied from the template's cached encoding.
    fn render_static(
        &self,
        template: &Template,
        lines: &[String],
        font: &str,
        extension: &str,
        source: &dyn Source,
    ) -> Result<Vec<u8>> {
        let background = self.background(template, source)?;
        let format = if extension == "png" { "png" } else { "jpg" };
        let encoded = self
            .encoded
            .try_get_with((template.id.clone(), format), || {
                anyhow::Ok(Arc::new(match format {
                    "png" => Encoded::Png(png::Strips::new(&background)),
                    _ => Encoded::Jpeg(jpeg::Rows::new(&background, JPEG_QUALITY)?),
                }))
            })
            .map_err(|error: Arc<anyhow::Error>| anyhow!("{error:#}"))?;
        let (width, height) = background.dimensions();
        let layers = self.text_layers(template, lines, font, (width, height), source);
        let dirty: Vec<_> = layers
            .iter()
            .map(|((_, top), layer)| {
                let clamp = |y: i64| y.clamp(0, height as i64) as u32;
                clamp(*top)..clamp(top + layer.height() as i64)
            })
            .collect();
        let compose = |rows: Range<u32>| {
            let row_bytes = width as usize * 4;
            let pixels = background.as_raw()
                [rows.start as usize * row_bytes..rows.end as usize * row_bytes]
                .to_vec();
            let mut image = RgbaImage::from_raw(width, rows.len() as u32, pixels)
                .expect("buffer matches dimensions");
            for ((left, top), layer) in &layers {
                typeset::paste_with_alpha(&mut image, layer, (*left, top - rows.start as i64));
            }
            image
        };
        match &*encoded {
            Encoded::Png(strips) => Ok(strips.encode(&dirty, compose)),
            Encoded::Jpeg(rows) => rows.encode(&dirty, compose),
        }
    }

    /// Drawn text for each of the template's text boxes that has any.
    fn text_layers(
        &self,
        template: &Template,
        lines: &[String],
        font: &str,
        size: (u32, u32),
        source: &dyn Source,
    ) -> Vec<((i64, i64), RgbaImage)> {
        template
            .text
            .par_iter()
            .enumerate()
            .filter_map(|(index, text)| {
                self.text_layer(text, lines.get(index), lines, font, size, source)
            })
            .collect()
    }

    /// Upstream `render_animation`, without animated-text or frame-count options.
    fn render_animation(
        &self,
        template: &Template,
        lines: &[String],
        font: &str,
        maximum_frames: usize,
        source: &dyn Source,
    ) -> Result<(Vec<RgbaImage>, u32)> {
        let animation = self.animation(template, maximum_frames, source)?;
        let Some((_, first)) = animation.frames.first() else {
            return Err(anyhow!("no frames decoded for {}", template.id));
        };
        let size = first.dimensions();
        let layers: Vec<_> = template
            .text
            .par_iter()
            .enumerate()
            .map(|(index, text)| self.text_layer(text, lines.get(index), lines, font, size, source))
            .collect();

        let total = animation.total;
        let frames: Vec<RgbaImage> = animation
            .frames
            .par_iter()
            .map(|(index, frame)| {
                let percent = if total == 1 {
                    1.0
                } else {
                    *index as f32 / total as f32
                };
                let mut image = frame.clone();
                for (text, layer) in template.text.iter().zip(&layers) {
                    let visible = percent == 1.0
                        || (text.start <= percent && percent < text.stop)
                        || text.stop == 0.0;
                    if let (true, Some((point, layer))) = (visible, layer) {
                        typeset::paste_with_alpha(&mut image, layer, *point);
                    }
                }
                image
            })
            .collect();

        Ok((frames, animation.frame_duration()))
    }

    /// Animated text: the text boxes are typed out in order, a mark at a
    /// time (see [`Reveal`]), with a [`TYPING_GAP_MS`] pause between boxes, over the template's frames (looped as needed); then the
    /// finished text holds for [`TYPING_HOLD_MS`]. Text boxes' `start`/`stop`
    /// are ignored. Returns the frames and each frame's delay in milliseconds.
    fn render_typing(
        &self,
        template: &Template,
        lines: &[String],
        font: &str,
        maximum_frames: usize,
        reveal: Reveal,
        source: &dyn Source,
    ) -> Result<(Vec<RgbaImage>, Vec<u32>)> {
        let animation = self.animation(template, maximum_frames, source)?;
        let backgrounds = &animation.frames;
        let Some((_, first)) = backgrounds.first() else {
            return Err(anyhow!("no frames decoded for {}", template.id));
        };
        let size = first.dimensions();
        let boxes: Vec<(TextLayout, usize)> = template
            .text
            .iter()
            .enumerate()
            .filter_map(|(index, text)| self.text_layout(text, lines.get(index), lines, font, size))
            .map(|layout| {
                let marks = layout.font.marks(&layout.line);
                (layout, marks)
            })
            .filter(|(_, marks)| *marks > 0)
            .collect();
        if boxes.is_empty() {
            let (frames, duration) =
                self.render_animation(template, lines, font, maximum_frames, source)?;
            let delays = vec![duration; frames.len()];
            return Ok((frames, delays));
        }
        let full: Vec<_> = boxes
            .par_iter()
            .map(|(layout, _)| self.draw_layout(layout, None, source))
            .collect();

        // When each box's marks appear, in milliseconds. Each box takes
        // [`TYPING_DELAY_MS`] per mark on average.
        let mut appears: Vec<Vec<u32>> = Vec::with_capacity(boxes.len());
        let mut end = 0;
        for (_, marks) in &boxes {
            let start = if appears.is_empty() {
                0
            } else {
                end + TYPING_GAP_MS
            };
            let length = (*marks as u32 * TYPING_DELAY_MS) as f64;
            let times: Vec<u32> = (1..=*marks)
                .map(|index| start + (length * reveal.appears(index, *marks)).round() as u32)
                .collect();
            end = times[times.len() - 1];
            appears.push(times);
        }

        // When each frame starts; the text is drawn as it is then.
        let (typing_budget, frame_budget) = TYPING_FRAMES;
        let period = animation.frame_duration().max(1);
        let mut times: Vec<u32> = Vec::new();
        if backgrounds.len() == 1 {
            // A still background needs a frame only when a mark appears (or a
            // few marks, to stay in budget): the pauses and the hold lengthen
            // the frame before them.
            let total: usize = boxes.iter().map(|(_, marks)| marks).sum();
            for box_times in &appears {
                let marks = box_times.len();
                let frames = marks.min((marks * typing_budget).div_ceil(total));
                times.extend((1..=frames).map(|frame| box_times[marks * frame / frames - 1]));
            }
        } else {
            // An animated background keeps moving through the typing, the
            // pauses and the hold, at its own pace; MP4 also eases the typing
            // between its frames.
            let base = match reveal {
                Reveal::Typing => period,
                Reveal::Easing => period.min(EASING_STEP_MS),
            };
            let step = base.max(end.div_ceil(typing_budget as u32)).max(1);
            times.extend((0..=end.div_ceil(step)).map(|index| (index * step).min(end)));
            let hold = (end / period + 1..)
                .map(|index| index * period)
                .take_while(|&time| time < end + TYPING_HOLD_MS)
                .take(frame_budget.saturating_sub(times.len()));
            times.extend(hold);
        }
        let delays: Vec<u32> = times
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .chain(times.last().map(|&last| end + TYPING_HOLD_MS - last))
            .collect();

        let frames: Vec<RgbaImage> = times
            .par_iter()
            .map(|&time| {
                let background = (time / period) as usize % backgrounds.len();
                let mut image = backgrounds[background].1.clone();
                for (((layout, marks), full), box_times) in boxes.iter().zip(&full).zip(&appears) {
                    let visible = box_times.partition_point(|&appear| appear <= time);
                    if visible == *marks {
                        if let Some((point, layer)) = full {
                            typeset::paste_with_alpha(&mut image, layer, *point);
                        }
                    } else if visible > 0
                        && let Some((point, layer)) =
                            self.draw_layout(layout, Some(visible), source)
                    {
                        typeset::paste_with_alpha(&mut image, &layer, point);
                    }
                }
                image
            })
            .collect();
        Ok((frames, delays))
    }

    /// Upstream `get_image_element`, drawn and rotated into a layer.
    fn text_layer(
        &self,
        text: &TextBox,
        line: Option<&String>,
        lines: &[String],
        font_name: &str,
        image_size: (u32, u32),
        source: &dyn Source,
    ) -> Option<((i64, i64), RgbaImage)> {
        let layout = self.text_layout(text, line, lines, font_name, image_size)?;
        self.draw_layout(&layout, None, source)
    }

    /// Upstream `get_image_element`'s wrapping and font fitting.
    fn text_layout<'a>(
        &'a self,
        text: &'a TextBox,
        line: Option<&String>,
        lines: &[String],
        font_name: &str,
        image_size: (u32, u32),
    ) -> Option<TextLayout<'a>> {
        let line = line?;
        let font = self
            .fonts
            .get(if font_name.is_empty() {
                &text.font
            } else {
                font_name
            })
            .or_else(|| self.fonts.get(""))?;
        let point = text.get_anchor(image_size);
        let max_size = text.get_size(image_size);
        if max_size.0 == 0 || max_size.1 == 0 {
            return None;
        }
        let max_font_size =
            (image_size.1 as f32 / if text.angle != 0.0 { 4.0 } else { 9.0 }) as u32;

        let wrapped = typeset::wrap(font, line, max_size, max_font_size);
        let line = text.stylize(&wrapped, lines);
        if line.trim().is_empty() {
            return None;
        }
        let sized: SizedFont = typeset::fit_font(font, &line, max_size, max_font_size);
        let offset = typeset::text_offset(&line, &sized, max_size, &text.align);
        Some(TextLayout {
            text,
            point,
            size: max_size,
            line,
            font: sized,
            offset,
        })
    }

    /// Draw the first `visible` marks of `layout` (all if `None`) and rotate
    /// them into a layer.
    fn draw_layout(
        &self,
        layout: &TextLayout,
        visible: Option<usize>,
        source: &dyn Source,
    ) -> Option<((i64, i64), RgbaImage)> {
        let TextLayout {
            text,
            point,
            size,
            line,
            font,
            offset: (x_offset, y_offset),
        } = layout;
        let (stroke_width, stroke_fill) = text.get_stroke(font.stroke_width());
        let rows = line.matches('\n').count() + 1;

        let mut layer = RgbaImage::new(size.0, size.1);
        typeset::draw_text(
            &mut layer,
            (-x_offset, -y_offset),
            line,
            font,
            &DrawOptions {
                fill: typeset::parse_color(&text.color).unwrap_or([255, 255, 255, 255]),
                stroke_width,
                stroke_fill: typeset::parse_color(&stroke_fill).unwrap_or([0, 0, 0, 255]),
                spacing: -y_offset / (rows * 2) as f32,
                align: &text.align,
                visible,
            },
            &|grapheme, size| self.emoji.get(grapheme, size, source),
        );
        let layer = typeset::rotate_expand(layer, text.angle);
        let ((left, top), layer) = typeset::trim(&layer)?;
        Some(((point.0 + left as i64, point.1 + top as i64), layer))
    }

    /// Static background resized so the short side is the default size.
    fn background(&self, template: &Template, source: &dyn Source) -> Result<Arc<RgbaImage>> {
        let path = template
            .static_image
            .as_ref()
            .context("template has no image")?;
        self.backgrounds
            .try_get_with(template.id.clone(), || {
                let image = load_image(&read(source, path)?)?;
                Ok(Arc::new(resize_default(&image, true)))
            })
            .map_err(|error: Arc<anyhow::Error>| anyhow!("{error:#}"))
    }

    fn animation(
        &self,
        template: &Template,
        maximum_frames: usize,
        source: &dyn Source,
    ) -> Result<Arc<Animation>> {
        let path = template
            .animated_image
            .as_ref()
            .context("template has no image")?;
        self.animations
            .try_get_with((template.id.clone(), maximum_frames), || {
                let is_gif = path.to_lowercase().ends_with(".gif");
                load_animation(&read(source, path)?, is_gif, maximum_frames).map(Arc::new)
            })
            .map_err(|error: Arc<anyhow::Error>| anyhow!("{error:#}"))
    }
}

fn read(source: &dyn Source, path: &str) -> Result<bytes::Bytes> {
    source
        .read(path)
        .with_context(|| format!("could not read {path}"))
}

fn load_image(bytes: &[u8]) -> Result<RgbaImage> {
    let mut decoder = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image.into_rgba8())
}

fn load_animation(bytes: &[u8], is_gif: bool, maximum_frames: usize) -> Result<Animation> {
    let (sources, duration) = if is_gif {
        let decoder = GifDecoder::new(Cursor::new(bytes))?;
        let frames = decoder.into_frames().collect_frames()?;
        let duration = frames
            .first()
            .map(|frame| {
                let (numerator, denominator) = frame.delay().numer_denom_ms();
                numerator / denominator.max(1)
            })
            .unwrap_or(100);
        (
            frames
                .into_iter()
                .map(|frame| frame.into_buffer())
                .collect::<Vec<_>>(),
            duration,
        )
    } else {
        (vec![load_image(bytes)?], 100)
    };
    let total = sources.len();

    let modulus = if maximum_frames >= total {
        1.0
    } else if maximum_frames > 0 {
        round1(total as f64 / maximum_frames as f64).max(1.0)
    } else {
        round1(total as f64 / MAXIMUM_FRAMES as f64).max(1.0)
    };

    let frames = sources
        .into_par_iter()
        .enumerate()
        .filter(|(index, _)| (*index as f64 % modulus) < 1.0)
        .map(|(index, frame)| (index, resize_default(&frame, false)))
        .collect();
    Ok(Animation {
        frames,
        total,
        duration,
    })
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// Upstream `resize_image(image, 0, 0, pad=False, expand=...)`.
fn resize_default(image: &RgbaImage, expand: bool) -> RgbaImage {
    let (width, height) = image.dimensions();
    let ratio = width as f64 / height as f64;
    let (default_width, default_height) = settings::DEFAULT_SIZE;
    let (dw, dh) = (default_width as f64, default_height as f64);
    let size = match (ratio < 1.0, expand) {
        (true, true) => (default_width, (dh / ratio) as u32),
        (true, false) => ((dw * ratio) as u32, default_height),
        (false, true) => ((dw * ratio) as u32, default_height),
        (false, false) => (default_width, (dh / ratio) as u32),
    };
    let size = (size.0.max(1), size.1.max(1));
    if size == (width, height) {
        return image.clone();
    }
    image::imageops::resize(image, size.0, size.1, FilterType::Lanczos3)
}

/// Re-encode `rendered` in the same format, downscaled until it's at most
/// `max_bytes`. Animations keep every frame.
pub fn shrink(rendered: &Rendered, max_bytes: usize) -> Result<Vec<u8>> {
    let (frames, delays) = decode_frames(&rendered.bytes, &rendered.extension)?;
    let (width, height) = frames.first().context("no frames")?.dimensions();
    // Encoded size is roughly proportional to area.
    let mut scale = (max_bytes as f64 / rendered.bytes.len() as f64).sqrt();
    for _ in 0..8 {
        scale *= 0.9;
        let size = (
            ((width as f64 * scale) as u32).max(1),
            ((height as f64 * scale) as u32).max(1),
        );
        let frames: Vec<RgbaImage> = frames
            .par_iter()
            .map(|frame| image::imageops::resize(frame, size.0, size.1, FilterType::Lanczos3))
            .collect();
        let bytes = match rendered.extension.as_str() {
            "gif" => encode_gif(frames, &delays)?,
            "webp" => crate::webp::encode(&frames, &delays)?,
            "png" => png::Strips::new(&frames[0]).encode(&[], |_| unreachable!()),
            _ => jpeg::Rows::new(&frames[0], JPEG_QUALITY)?.encode(&[], |_| unreachable!())?,
        };
        if bytes.len() <= max_bytes {
            return Ok(bytes);
        }
    }
    Err(anyhow!(
        "could not shrink the image below {max_bytes} bytes"
    ))
}

/// Every frame of an encoded image, composited, and each frame's delay.
fn decode_frames(bytes: &[u8], extension: &str) -> Result<(Vec<RgbaImage>, Vec<u32>)> {
    let frames = match extension {
        "gif" => GifDecoder::new(Cursor::new(bytes))?.into_frames(),
        "webp" => {
            let decoder = WebPDecoder::new(Cursor::new(bytes))?;
            if !decoder.has_animation() {
                return Ok((vec![load_image(bytes)?], vec![0]));
            }
            decoder.into_frames()
        }
        _ => return Ok((vec![load_image(bytes)?], vec![0])),
    }
    .collect_frames()?;
    Ok(frames
        .into_iter()
        .map(|frame| {
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            (frame.into_buffer(), numerator / denominator.max(1))
        })
        .unzip())
}

/// Encode frames with one shared palette; later frames only store the
/// pixels that changed (the rest are transparent over the previous frame).
/// `delays` are in milliseconds, one per frame.
fn encode_gif(frames: Vec<RgbaImage>, delays: &[u32]) -> Result<Vec<u8>> {
    let first = frames.first().context("no frames")?;
    let (width, height) = (
        u16::try_from(first.width())?,
        u16::try_from(first.height())?,
    );

    // Build one palette from an evenly spaced sample of all frames.
    const SAMPLE_PIXELS: usize = 400_000;
    let total_pixels: usize = frames.iter().map(|frame| frame.as_raw().len() / 4).sum();
    let step = (total_pixels / SAMPLE_PIXELS).max(1);
    let palette = Palette::build(
        frames
            .iter()
            .flat_map(|frame| frame.as_raw().chunks_exact(4).step_by(step)),
        255,
    );
    let mut global_palette: Vec<u8> = palette.colors.iter().flatten().copied().collect();
    global_palette.resize(256 * 3, 0);
    const TRANSPARENT: u8 = 255;

    let indexed: Vec<Vec<u8>> = frames
        .par_iter()
        .map(|frame| {
            frame
                .pixels()
                .map(|pixel| palette.index_of(pixel[0], pixel[1], pixel[2]))
                .collect()
        })
        .collect();

    let row = width as usize;
    let encoded: Vec<gif::Frame<'static>> = indexed
        .par_iter()
        .enumerate()
        .map(|(index, current)| {
            let mut frame = gif::Frame {
                delay: (delays[index] as f32 / 10.0).round() as u16,
                ..Default::default()
            };
            if index == 0 {
                frame.width = width;
                frame.height = height;
                frame.buffer = current.clone().into();
                frame.make_lzw_pre_encoded();
                return frame;
            }
            let previous = &indexed[index - 1];
            let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
            for (offset, (a, b)) in current.iter().zip(previous).enumerate() {
                if a != b {
                    let (x, y) = (offset % row, offset / row);
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
            if x0 == usize::MAX {
                (x0, y0, x1, y1) = (0, 0, 0, 0);
            }
            let mut buffer = Vec::with_capacity((x1 - x0 + 1) * (y1 - y0 + 1));
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let offset = y * row + x;
                    let value = current[offset];
                    buffer.push(if value == previous[offset] {
                        TRANSPARENT
                    } else {
                        value
                    });
                }
            }
            frame.left = x0 as u16;
            frame.top = y0 as u16;
            frame.width = (x1 - x0 + 1) as u16;
            frame.height = (y1 - y0 + 1) as u16;
            frame.transparent = Some(TRANSPARENT);
            frame.dispose = gif::DisposalMethod::Keep;
            frame.buffer = buffer.into();
            frame.make_lzw_pre_encoded();
            frame
        })
        .collect();

    let mut out = Vec::new();
    {
        let mut encoder = gif::Encoder::new(&mut out, width, height, &global_palette)?;
        encoder.set_repeat(gif::Repeat::Infinite)?;
        for frame in &encoded {
            encoder.write_lzw_pre_encoded_frame(frame)?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::assets::{Directory, Files};

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn renderer_with(debug: bool) -> Renderer {
        let config = Config {
            base_url: "http://localhost:5000".into(),
            debug,
            ..Config::default()
        };
        Renderer::new(
            config,
            Arc::new(Catalog::load(&root()).unwrap()),
            Arc::new(Fonts::load(&Directory::new(root())).unwrap()),
        )
    }

    fn renderer() -> Renderer {
        renderer_with(true)
    }

    fn decode(bytes: &[u8]) -> DynamicImage {
        ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
    }

    #[test]
    fn renders_every_template_example() {
        let renderer = renderer();
        let templates: Vec<_> = renderer
            .catalog()
            .filter("", None)
            .into_iter()
            .cloned()
            .collect();
        assert!(templates.len() > 200);
        templates.par_iter().for_each(|template| {
            let request = MemeRequest {
                template_id: template.id.clone(),
                lines: template.example.clone(),
                font: String::new(),
                extension: "jpg".into(),
                animate_text: false,
            };
            let rendered = renderer
                .render(&request, &Directory::new(root()))
                .unwrap_or_else(|e| panic!("{}: {e}", template.id));
            let image = decode(&rendered.bytes);
            assert!(image.width().min(image.height()) >= 300, "{}", template.id);
        });
    }

    #[test]
    fn renders_all_formats() {
        let renderer = renderer();
        for (template, extension, content_type) in [
            ("fry", "png", "image/png"),
            ("fry", "jpeg", "image/jpeg"),
            ("fry", "gif", "image/gif"),
            ("fry", "webp", "image/webp"),
            ("oprah", "gif", "image/gif"),
            ("oprah", "webp", "image/webp"),
            ("fry", "mp4", "video/mp4"),
            ("oprah", "mp4", "video/mp4"),
        ] {
            let request = MemeRequest {
                template_id: template.into(),
                lines: vec!["hello :fire:".into(), "world".into()],
                font: "impact".into(),
                extension: extension.into(),
                animate_text: false,
            };
            let rendered = renderer.render(&request, &Directory::new(root())).unwrap();
            assert_eq!(rendered.content_type, content_type);
            match extension {
                "webp" => {}
                // Decoded in `mp4::tests`.
                "mp4" => assert_eq!(&rendered.bytes[4..8], b"ftyp"),
                _ => {
                    decode(&rendered.bytes);
                }
            }
        }
    }

    #[test]
    fn shrinks_to_fit_in_the_same_format() {
        let renderer = renderer();
        for (template, extension) in [
            ("sf", "png"),
            ("sf", "jpg"),
            ("fine", "gif"),
            ("fine", "webp"),
        ] {
            let request = MemeRequest {
                template_id: template.into(),
                lines: vec!["a".into(), "b".into()],
                font: String::new(),
                extension: extension.into(),
                animate_text: false,
            };
            let rendered = renderer.render(&request, &Directory::new(root())).unwrap();
            let max_bytes = rendered.bytes.len() / 2;
            let shrunk = shrink(&rendered, max_bytes).unwrap();
            assert!(shrunk.len() <= max_bytes, "{template}.{extension}");
            let (frames, _) = decode_frames(&shrunk, extension).unwrap();
            let (original, _) = decode_frames(&rendered.bytes, extension).unwrap();
            assert_eq!(frames.len(), original.len(), "{template}.{extension}");
            assert!(
                frames[0].width() < original[0].width(),
                "{template}.{extension}"
            );
        }
    }

    #[test]
    fn validates_requests() {
        let renderer = renderer();
        let directory = Directory::new(root());
        let request = |template: &str, font: &str, extension: &str, line: &str| MemeRequest {
            template_id: template.into(),
            lines: vec![line.into()],
            font: font.into(),
            extension: extension.into(),
            animate_text: false,
        };
        assert!(matches!(
            renderer.render(&request("nope", "", "png", "a"), &directory),
            Err(RenderError::NotFound(_))
        ));
        assert!(matches!(
            renderer.render(&request("fry", "nope", "png", "a"), &directory),
            Err(RenderError::Invalid(_))
        ));
        assert!(matches!(
            renderer.render(&request("fry", "", "bmp", "a"), &directory),
            Err(RenderError::Invalid(_))
        ));
        assert!(matches!(
            renderer.render(&request("fry", "", "png", &"a".repeat(201)), &directory),
            Err(RenderError::TooLong)
        ));
    }

    #[test]
    fn animated_template_keeps_frames() {
        let renderer = renderer();
        let template = renderer.catalog().get("oprah").unwrap().clone();
        let (frames, duration) = renderer
            .render_animation(
                &template,
                &["a".into(), "b".into()],
                "",
                0,
                &Directory::new(root()),
            )
            .unwrap();
        assert!(frames.len() > 1);
        assert!(duration > 0);
    }

    #[test]
    fn types_text_out_then_holds() {
        let renderer = renderer();
        let directory = Directory::new(root());
        let lines: Vec<String> = vec!["hi you".into(), "ok :fire:".into()];

        // A still background: a frame per mark (spaces don't count; the emoji
        // does), a pause after the first box, then the last frame holds.
        let template = renderer.catalog().get("sf").unwrap().clone();
        let (frames, delays) = renderer
            .render_typing(&template, &lines, "", 0, Reveal::Typing, &directory)
            .unwrap();
        assert_eq!(frames.len(), 8);
        let mut expected = vec![TYPING_DELAY_MS; 7];
        expected[4] += TYPING_GAP_MS;
        assert_eq!(delays[..7], expected);
        assert_eq!(delays[7], TYPING_HOLD_MS);
        assert!(frames.windows(2).all(|pair| pair[0] != pair[1]));
        let (full, _) = renderer
            .render_animation(&template, &lines, "", 0, &directory)
            .unwrap();
        assert!(frames[7] == full[0]);

        // An animated background keeps moving through the typing and the hold.
        let template = renderer.catalog().get("oprah").unwrap().clone();
        let (frames, delays) = renderer
            .render_typing(&template, &lines, "", 0, Reveal::Typing, &directory)
            .unwrap();
        let total: u32 = delays.iter().sum();
        assert!(frames.len() > 8);
        assert!(
            total >= TYPING_HOLD_MS + TYPING_GAP_MS + 8 * TYPING_DELAY_MS,
            "{delays:?}"
        );

        // Long text types several marks per frame to stay in budget.
        let template = renderer.catalog().get("sf").unwrap().clone();
        let long = vec!["a".repeat(150), "b".repeat(150)];
        let (frames, _) = renderer
            .render_typing(&template, &long, "", 0, Reveal::Typing, &directory)
            .unwrap();
        assert_eq!(frames.len(), TYPING_FRAMES.0);

        for extension in ["gif", "webp"] {
            let request = MemeRequest {
                template_id: "sf".into(),
                lines: lines.clone(),
                font: String::new(),
                extension: extension.into(),
                animate_text: true,
            };
            let rendered = renderer.render(&request, &directory).unwrap();
            let (decoded, decoded_delays) = decode_frames(&rendered.bytes, extension).unwrap();
            assert_eq!(decoded.len(), 8, "{extension}");
            assert_eq!(decoded_delays[7], TYPING_HOLD_MS, "{extension}");
            let shrunk = shrink(&rendered, rendered.bytes.len() / 2).unwrap();
            assert_eq!(decode_frames(&shrunk, extension).unwrap().1, decoded_delays);
        }
        let request = MemeRequest {
            template_id: "sf".into(),
            lines: lines.clone(),
            font: String::new(),
            extension: "mp4".into(),
            animate_text: true,
        };
        assert!(renderer.render(&request, &directory).is_ok());

        // Eased (MP4): still a frame per mark, but the delays between marks
        // shrink towards the middle of each box and grow towards its end.
        let (eased, delays) = renderer
            .render_typing(&template, &lines, "", 0, Reveal::Easing, &directory)
            .unwrap();
        let (typed, _) = renderer
            .render_typing(&template, &lines, "", 0, Reveal::Typing, &directory)
            .unwrap();
        assert_eq!(eased.len(), 8);
        assert!(eased == typed);
        let first = &delays[..4];
        assert!(first[0] > first[1] && first[1] < first[3], "{delays:?}");
        assert!(delays[4] > TYPING_GAP_MS);
        assert_eq!(delays[7], TYPING_HOLD_MS);
        let (frames, delays) = renderer
            .render_typing(
                &renderer.catalog().get("oprah").unwrap().clone(),
                &lines,
                "",
                0,
                Reveal::Easing,
                &directory,
            )
            .unwrap();
        let total: u32 = delays.iter().sum();
        assert!(frames.len() > 8);
        assert!(delays.contains(&EASING_STEP_MS), "{delays:?}");
        assert!(
            total >= TYPING_HOLD_MS + TYPING_GAP_MS + 7 * TYPING_DELAY_MS,
            "{delays:?}"
        );

        let request = MemeRequest {
            template_id: "fry".into(),
            lines,
            font: String::new(),
            extension: "png".into(),
            animate_text: true,
        };
        assert!(matches!(
            renderer.render(&request, &directory),
            Err(RenderError::Invalid(_))
        ));
    }

    #[test]
    fn lists_missing_assets() {
        let renderer = renderer_with(false);
        let directory = Directory::new(root());
        let request = |extension: &str| MemeRequest {
            template_id: "fry".into(),
            lines: vec![":fire: hot".into(), ":fire:".into()],
            font: String::new(),
            extension: extension.into(),
            animate_text: false,
        };
        assert_eq!(
            renderer.missing(&request("png")),
            ["emoji/72x72/1f525.png", "templates/fry/default.png"]
        );
        assert_eq!(
            renderer.missing(&request("gif")),
            ["emoji/72x72/1f525.png", "templates/fry/default.gif"]
        );

        // Rendering from exactly the missing files works, then nothing is missing.
        let files: Files = directory.read_all(&renderer.missing(&request("png")));
        renderer.render(&request("png"), &files).unwrap();
        assert!(renderer.missing(&request("png")).is_empty());
        let mut other = request("jpg");
        other.lines[1] = "new text".into();
        assert!(renderer.missing(&other).is_empty());
        renderer.render(&other, &Files::new()).unwrap();
    }
}
