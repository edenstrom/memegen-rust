//! Emoji support: `:alias:` expansion and Twemoji image lookup (upstream uses
//! the `emoji` package plus Pilmoji, which also draws Twemoji graphics).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use image::RgbaImage;
use moka::sync::Cache;
use unicode_segmentation::UnicodeSegmentation;

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

pub struct EmojiImages {
    directory: PathBuf,
    cache: Cache<(String, u32), Option<Arc<RgbaImage>>>,
}

impl EmojiImages {
    pub fn new(root: &Path) -> Self {
        Self {
            directory: root.join("emoji").join("72x72"),
            cache: Cache::new(2_048),
        }
    }

    /// Load an emoji image scaled to `size` pixels square.
    pub fn get(&self, grapheme: &str, size: u32) -> Option<Arc<RgbaImage>> {
        if size == 0 {
            return None;
        }
        self.cache.get_with((grapheme.to_string(), size), || {
            let path = twemoji_names(grapheme)
                .into_iter()
                .map(|name| self.directory.join(format!("{name}.png")))
                .find(|path| path.exists())?;
            let image = image::open(path).ok()?.into_rgba8();
            Some(Arc::new(image::imageops::resize(
                &image,
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
