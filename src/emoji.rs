//! Emoji support: `:alias:` expansion and Twemoji image lookup (upstream uses
//! the `emoji` package plus Pilmoji, which also draws Twemoji graphics).

use std::sync::Arc;

use image::RgbaImage;
use unicode_segmentation::UnicodeSegmentation;

use crate::assets::Source;
use crate::cache::Cache;

/// Replace `:shortcode:` aliases with emoji characters.
pub fn emojize(text: &str) -> String {
    if !text.contains(':') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(':') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find(':') {
            Some(end)
                if end > 0
                    && after[..end]
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_+-".contains(c)) =>
            {
                if let Some(emoji) = emojis::get_by_shortcode(&after[..end]) {
                    out.push_str(emoji.as_str());
                    rest = &after[end + 1..];
                } else {
                    out.push(':');
                    rest = after;
                }
            }
            _ => {
                out.push(':');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whether a grapheme cluster should be drawn as an emoji image.
pub fn is_emoji(grapheme: &str) -> bool {
    let presentation = grapheme.chars().any(|c| {
        let c = c as u32;
        c >= 0x1F000
            || c == 0xFE0F
            || (0x2600..=0x27BF).contains(&c)
            || (0x2B00..=0x2BFF).contains(&c)
    });
    presentation && emojis::get(grapheme).is_some()
}

pub fn segments(text: &str) -> impl Iterator<Item = &str> {
    text.graphemes(true)
}

/// Twemoji file name for an emoji sequence (`1f468-200d-1f469`).
fn twemoji_names(grapheme: &str) -> Vec<String> {
    let code = |chars: &mut dyn Iterator<Item = char>| {
        chars
            .map(|c| format!("{:x}", c as u32))
            .collect::<Vec<_>>()
            .join("-")
    };
    let full = code(&mut grapheme.chars());
    let stripped = code(&mut grapheme.chars().filter(|c| *c != '\u{FE0F}'));
    if grapheme.contains('\u{200D}') {
        vec![full, stripped]
    } else {
        vec![stripped, full]
    }
}

/// Asset paths that may hold the image for an emoji, in order of preference.
fn paths(grapheme: &str) -> impl Iterator<Item = String> {
    twemoji_names(grapheme)
        .into_iter()
        .map(|name| format!("emoji/72x72/{name}.png"))
}

type Image = Option<Arc<RgbaImage>>;

fn weight<K>(_: &K, image: &Image) -> u32 {
    image
        .as_ref()
        .map_or(1, |image| image.as_raw().len() as u32)
}

pub struct EmojiImages {
    originals: Cache<String, Image>,
    scaled: Cache<(String, u32), Image>,
}

impl Default for EmojiImages {
    fn default() -> Self {
        Self {
            originals: Cache::new(8 * 1024 * 1024, weight),
            scaled: Cache::new(16 * 1024 * 1024, weight),
        }
    }
}

impl EmojiImages {
    /// Asset paths to load before drawing `text`: candidates for every emoji
    /// that isn't cached yet.
    pub fn missing(&self, text: &str) -> Vec<String> {
        let text = emojize(text);
        segments(&text)
            .filter(|grapheme| {
                is_emoji(grapheme) && !self.originals.contains(&grapheme.to_string())
            })
            .flat_map(paths)
            .collect()
    }

    /// Load an emoji image scaled to `size` pixels square.
    pub fn get(&self, grapheme: &str, size: u32, source: &dyn Source) -> Image {
        if size == 0 {
            return None;
        }
        self.scaled.get_with((grapheme.to_string(), size), || {
            let image = self.originals.get_with(grapheme.to_string(), || {
                let bytes = paths(grapheme).find_map(|path| source.read(&path))?;
                Some(Arc::new(image::load_from_memory(&bytes).ok()?.into_rgba8()))
            })?;
            Some(Arc::new(image::imageops::resize(
                &*image,
                size,
                size,
                image::imageops::FilterType::Lanczos3,
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_aliases() {
        assert_eq!(emojize("hi :smile:"), "hi 😄");
        assert_eq!(emojize(":thumbsup: 10:30 :nope:"), "👍 10:30 :nope:");
    }

    #[test]
    fn detects_emoji() {
        assert!(is_emoji("😄"));
        assert!(is_emoji("❤️"));
        assert!(!is_emoji("a"));
        assert!(!is_emoji("©"));
    }

    #[test]
    fn names() {
        assert_eq!(twemoji_names("❤️")[0], "2764");
        assert_eq!(twemoji_names("👍")[0], "1f44d");
    }
}
