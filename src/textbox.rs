//! Text box model and casing styles, ported from upstream `app/models/text.py`.

use serde::{Deserialize, Serialize};

use crate::{emoji, settings};

fn default_style() -> String {
    "upper".into()
}
fn default_color() -> String {
    "white".into()
}
fn default_font() -> String {
    settings::DEFAULT_FONT.into()
}
fn default_scale_x() -> f32 {
    1.0
}
fn default_scale_y() -> f32 {
    0.2
}
fn default_align() -> String {
    "center".into()
}
fn default_stop() -> f32 {
    1.0
}

/// Where and how one line of text is drawn. Positions and sizes are
/// fractions of the image's width and height.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TextBox {
    /// Letter case: "upper" (default), "lower", "title", "capitalize", "swapcase", "mock" (aLtErNaTiNg), "none" (as typed), or "default" (as typed, capitalizing sentences).
    #[serde(default = "default_style")]
    pub style: String,
    /// Text color, a CSS color name or hex code: "white" (default), "black", "#FF0000".
    #[serde(default = "default_color")]
    pub color: String,
    /// Font ID or alias from the fonts list (default "impact"). A request's `font` overrides it.
    #[serde(default = "default_font")]
    pub font: String,
    /// Left edge of the box, 0 to 1 of the image width (default 0).
    #[serde(default)]
    pub anchor_x: f32,
    /// Top edge of the box, 0 to 1 of the image height (default 0).
    #[serde(default)]
    pub anchor_y: f32,
    /// Counterclockwise rotation in degrees (default 0).
    #[serde(default)]
    pub angle: f32,
    /// Box width, 0 to 1 of the image width (default 1). Text wraps and shrinks to fit the box.
    #[serde(default = "default_scale_x")]
    pub scale_x: f32,
    /// Box height, 0 to 1 of the image height (default 0.2).
    #[serde(default = "default_scale_y")]
    pub scale_y: f32,
    /// "left", "center" (default) or "right".
    #[serde(default = "default_align")]
    pub align: String,
    /// On an animated background, when the text appears, 0 to 1 of the animation (default 0). Ignored with animated text.
    #[serde(default)]
    pub start: f32,
    /// On an animated background, when the text disappears, 0 to 1 of the animation (default 1).
    #[serde(default = "default_stop")]
    pub stop: f32,
}

/// Most text boxes a request can set.
pub const MAX_TEXT_BOXES: usize = 20;

const STYLES: &[&str] = &[
    "upper",
    "lower",
    "title",
    "capitalize",
    "swapcase",
    "casefold",
    "mock",
    "none",
    "default",
];
const ALIGNMENTS: &[&str] = &["left", "center", "right"];

/// The text boxes of a template without its own: a top and a bottom line.
pub fn default_boxes() -> Vec<TextBox> {
    vec![
        TextBox::default(),
        TextBox {
            anchor_y: 0.8,
            ..Default::default()
        },
    ]
}

/// `lines` normalized for canonical URLs by the box each goes in.
pub fn normalize_lines(boxes: &[TextBox], lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| match boxes.get(index) {
            Some(text) => text.normalize(line),
            None => line.clone(),
        })
        .collect()
}

/// Text boxes as compact JSON, leaving out fields at their defaults; used
/// in URLs and cache keys.
pub fn compact(boxes: &[TextBox]) -> String {
    // Through a string, so f32s keep their short form (0.8, not
    // 0.800000011920929).
    let value = |text: &TextBox| -> serde_json::Value {
        let json = serde_json::to_string(text).expect("serializes");
        serde_json::from_str(&json).expect("parses")
    };
    let default = value(&TextBox::default());
    let boxes: Vec<serde_json::Value> = boxes
        .iter()
        .map(|text| {
            let mut value = value(text);
            if let (Some(fields), Some(default)) = (value.as_object_mut(), default.as_object()) {
                fields.retain(|name, value| default.get(name) != Some(value));
            }
            value
        })
        .collect();
    serde_json::to_string(&boxes).expect("serializes")
}

/// Check text boxes from a request; returns what's wrong with the first bad
/// one. Fonts are checked by the renderer.
pub fn validate(boxes: &[TextBox]) -> Result<(), String> {
    if boxes.is_empty() || boxes.len() > MAX_TEXT_BOXES {
        return Err(format!(
            "Pass between 1 and {MAX_TEXT_BOXES} text boxes; got {}",
            boxes.len()
        ));
    }
    for (index, text) in boxes.iter().enumerate() {
        text.validate()
            .map_err(|error| format!("Text box {}: {error}", index + 1))?;
    }
    Ok(())
}

impl Default for TextBox {
    fn default() -> Self {
        serde_yaml::from_str("{}").expect("defaults deserialize")
    }
}

impl TextBox {
    fn validate(&self) -> Result<(), String> {
        for (name, value, range) in [
            ("anchor_x", self.anchor_x, 0.0..=1.0),
            ("anchor_y", self.anchor_y, 0.0..=1.0),
            ("scale_x", self.scale_x, 0.0..=1.0),
            ("scale_y", self.scale_y, 0.0..=1.0),
            ("angle", self.angle, -360.0..=360.0),
            ("start", self.start, 0.0..=1.0),
            ("stop", self.stop, 0.0..=1.0),
        ] {
            if !range.contains(&value) {
                return Err(format!(
                    "{name} must be between {} and {}, not {value}",
                    range.start(),
                    range.end()
                ));
            }
        }
        if !STYLES.contains(&self.style.as_str()) {
            return Err(format!(
                "Unknown style {:?} (expected one of {})",
                self.style,
                STYLES.join(", ")
            ));
        }
        if !ALIGNMENTS.contains(&self.align.as_str()) {
            return Err(format!(
                "Unknown align {:?} (expected left, center or right)",
                self.align
            ));
        }
        if crate::typeset::parse_color(&self.color).is_none() {
            return Err(format!("Unknown color {:?}", self.color));
        }
        Ok(())
    }

    pub fn get_anchor(&self, (width, height): (u32, u32)) -> (i64, i64) {
        (
            (width as f32 * self.anchor_x) as i64,
            (height as f32 * self.anchor_y) as i64,
        )
    }

    pub fn get_size(&self, (width, height): (u32, u32)) -> (u32, u32) {
        (
            (width as f32 * self.scale_x).max(0.0) as u32,
            (height as f32 * self.scale_y).max(0.0) as u32,
        )
    }

    /// Returns `(stroke_width, stroke_color)` for the given font-based width.
    pub fn get_stroke(&self, width: u32) -> (u32, String) {
        if self.color == "black" {
            (1, "#FFFFFF7F".into())
        } else if self.color.contains('#') {
            let mut color = String::from("#000000");
            if self.color.len() >= color.len() + 2 {
                color.push_str(&self.color[self.color.len() - 2..]);
            }
            (1, color)
        } else {
            (width, "black".into())
        }
    }

    /// Normalize text for canonical URLs (lowercase unless case matters).
    pub fn normalize(&self, text: &str) -> String {
        if matches!(self.style.as_str(), "none" | "default" | "mock") {
            text.to_string()
        } else {
            text.to_lowercase()
        }
    }

    pub fn stylize(&self, text: &str, all_lines: &[String]) -> String {
        let text = emoji::emojize(text);
        let lines: Vec<&String> = all_lines
            .iter()
            .filter(|line| !line.trim().is_empty())
            .collect();

        match self.style.as_str() {
            "none" => text,
            "default" => {
                let all_lower = lines.iter().all(|line| is_lower(line));
                let includes_sentence = lines.iter().any(|line| line.ends_with(['.', '?', '!']));
                let mut text = text;
                if is_lower(&text) && (all_lower || includes_sentence) {
                    text = capitalize(&text);
                }
                capitalize_pronoun_i(&text)
            }
            "mock" => spongemock(&text, 0.75),
            "upper" | "" => text.to_uppercase(),
            "lower" => text.to_lowercase(),
            "title" => title(&text),
            "capitalize" => capitalize(&text),
            "swapcase" => swapcase(&text),
            "casefold" => text.to_lowercase(),
            other => {
                tracing::warn!("Unsupported text style: {other}");
                text
            }
        }
    }
}

fn is_cased(c: char) -> bool {
    c.is_lowercase() || c.is_uppercase()
}

/// Python's `str.islower()`: at least one cased character and no uppercase.
fn is_lower(text: &str) -> bool {
    text.chars().any(is_cased) && !text.chars().any(char::is_uppercase)
}

/// Python's `str.capitalize()`.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

/// Python's `str.title()`.
fn title(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous_cased = false;
    for c in text.chars() {
        if is_cased(c) {
            if previous_cased {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            previous_cased = true;
        } else {
            out.push(c);
            previous_cased = false;
        }
    }
    out
}

fn swapcase(text: &str) -> String {
    text.chars()
        .flat_map(|c| -> Box<dyn Iterator<Item = char>> {
            if c.is_uppercase() {
                Box::new(c.to_lowercase())
            } else if c.is_lowercase() {
                Box::new(c.to_uppercase())
            } else {
                Box::new(std::iter::once(c))
            }
        })
        .collect()
}

/// Equivalent of `re.sub(r"\bi\b", "I", text)`.
fn capitalize_pronoun_i(text: &str) -> String {
    let is_word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let chars: Vec<char> = text.chars().collect();
    chars
        .iter()
        .enumerate()
        .map(|(index, &c)| {
            let before = index.checked_sub(1).map(|i| chars[i]);
            let after = chars.get(index + 1).copied();
            if c == 'i' && !is_word(before) && !is_word(after) {
                'I'
            } else {
                c
            }
        })
        .collect()
}

/// Port of `spongemock.mock(text, diversity_bias, random_seed=0)`, including
/// CPython's seeded Mersenne Twister so output matches upstream.
fn spongemock(text: &str, diversity_bias: f64) -> String {
    let mut rng = PyRandom::seeded_zero();
    let mut out = String::with_capacity(text.len());
    let mut last_was_upper = true;
    let mut swap_chance = 0.5;
    for c in text.chars() {
        if c.is_alphabetic() {
            if rng.random() < swap_chance {
                last_was_upper = !last_was_upper;
                swap_chance = 0.5;
            }
            if last_was_upper {
                out.extend(c.to_uppercase());
            } else {
                out.extend(c.to_lowercase());
            }
            swap_chance += (1.0 - swap_chance) * diversity_bias;
        } else {
            out.push(c);
        }
    }
    out
}

/// MT19937 seeded the way CPython's `random.seed(0)` does.
struct PyRandom {
    state: [u32; 624],
    index: usize,
}

impl PyRandom {
    fn seeded_zero() -> Self {
        let mut rng = Self {
            state: [0; 624],
            index: 624,
        };
        rng.init_genrand(19_650_218);
        // init_by_array with key = [0]
        let key = [0u32];
        let (mut i, mut j) = (1usize, 0usize);
        for _ in 0..624.max(key.len()) {
            let prev = rng.state[i - 1];
            rng.state[i] = (rng.state[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1_664_525))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= 624 {
                rng.state[0] = rng.state[623];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
        }
        for _ in 0..623 {
            let prev = rng.state[i - 1];
            rng.state[i] = (rng.state[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1_566_083_941))
                .wrapping_sub(i as u32);
            i += 1;
            if i >= 624 {
                rng.state[0] = rng.state[623];
                i = 1;
            }
        }
        rng.state[0] = 0x8000_0000;
        rng
    }

    fn init_genrand(&mut self, seed: u32) {
        self.state[0] = seed;
        for i in 1..624 {
            let prev = self.state[i - 1];
            self.state[i] = 1_812_433_253u32
                .wrapping_mul(prev ^ (prev >> 30))
                .wrapping_add(i as u32);
        }
        self.index = 624;
    }

    fn next_u32(&mut self) -> u32 {
        if self.index >= 624 {
            for i in 0..624 {
                let y = (self.state[i] & 0x8000_0000) | (self.state[(i + 1) % 624] & 0x7fff_ffff);
                let mut next = self.state[(i + 397) % 624] ^ (y >> 1);
                if y & 1 != 0 {
                    next ^= 0x9908_b0df;
                }
                self.state[i] = next;
            }
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^ (y >> 18)
    }

    fn random(&mut self) -> f64 {
        let a = (self.next_u32() >> 5) as f64;
        let b = (self.next_u32() >> 6) as f64;
        (a * 67_108_864.0 + b) * (1.0 / 9_007_199_254_740_992.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled(style: &str, text: &str, lines: &[&str]) -> String {
        let lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        TextBox {
            style: style.into(),
            ..Default::default()
        }
        .stylize(text, &lines)
    }

    #[test]
    fn python_random_matches_cpython() {
        // random.seed(0); random.random()
        let mut rng = PyRandom::seeded_zero();
        assert!((rng.random() - 0.8444218515250481).abs() < 1e-15);
        assert!((rng.random() - 0.7579544029403025).abs() < 1e-15);
    }

    #[test]
    fn styles() {
        assert_eq!(styled("upper", "hello", &["hello"]), "HELLO");
        assert_eq!(styled("title", "they're here", &[]), "They'Re Here");
        assert_eq!(
            styled("default", "i think i can", &["i think i can"]),
            "I think I can"
        );
        assert_eq!(
            styled("default", "Hello there", &["Hello there"]),
            "Hello there"
        );
        assert_eq!(styled("none", "MiXeD", &[]), "MiXeD");
    }

    #[test]
    fn stroke() {
        let black = TextBox {
            color: "black".into(),
            ..Default::default()
        };
        assert_eq!(black.get_stroke(3), (1, "#FFFFFF7F".into()));
        let hex = TextBox {
            color: "#FFFFFFA6".into(),
            ..Default::default()
        };
        assert_eq!(hex.get_stroke(3), (1, "#000000A6".into()));
        assert_eq!(TextBox::default().get_stroke(3), (3, "black".into()));
    }

    #[test]
    fn compacts_and_validates() {
        let boxes = vec![
            TextBox::default(),
            TextBox {
                anchor_y: 0.8,
                color: "black".into(),
                ..Default::default()
            },
        ];
        let json = compact(&boxes);
        assert_eq!(json, r#"[{},{"anchor_y":0.8,"color":"black"}]"#);
        assert_eq!(serde_json::from_str::<Vec<TextBox>>(&json).unwrap(), boxes);
        assert!(validate(&boxes).is_ok());

        assert!(validate(&[]).is_err());
        let bad = |text: TextBox| validate(&[text]).unwrap_err();
        assert!(
            bad(TextBox {
                scale_x: 2.0,
                ..Default::default()
            })
            .contains("scale_x")
        );
        assert!(
            bad(TextBox {
                anchor_y: f32::NAN,
                ..Default::default()
            })
            .contains("anchor_y")
        );
        assert!(
            bad(TextBox {
                color: "nope".into(),
                ..Default::default()
            })
            .contains("color")
        );
        assert!(
            bad(TextBox {
                align: "middle".into(),
                ..Default::default()
            })
            .contains("align")
        );
    }
}
