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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextBox {
    #[serde(default = "default_style")]
    pub style: String,
    #[serde(default = "default_color")]
    pub color: String,
    #[serde(default = "default_font")]
    pub font: String,
    #[serde(default)]
    pub anchor_x: f32,
    #[serde(default)]
    pub anchor_y: f32,
    #[serde(default)]
    pub angle: f32,
    #[serde(default = "default_scale_x")]
    pub scale_x: f32,
    #[serde(default = "default_scale_y")]
    pub scale_y: f32,
    #[serde(default = "default_align")]
    pub align: String,
    #[serde(default)]
    pub start: f32,
    #[serde(default = "default_stop")]
    pub stop: f32,
}

impl Default for TextBox {
    fn default() -> Self {
        serde_yaml::from_str("{}").expect("defaults deserialize")
    }
}

impl TextBox {
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
}
