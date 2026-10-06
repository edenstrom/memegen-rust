//! URL text encoding, ported from upstream `app/utils/text.py`.

use percent_encoding::percent_decode_str;

fn unquote(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().into_owned()
}

/// Encode lines of text into a URL slug (`line1/line2`).
pub fn encode<S: AsRef<str>>(lines: &[S]) -> String {
    let encoded: Vec<String> = lines
        .iter()
        .map(|line| {
            let line = line.as_ref();
            if line == "/" || line.is_empty() {
                "_".to_string()
            } else {
                encode_line(line)
            }
        })
        .collect();
    let slug = encoded.join("/");
    if slug.is_empty() {
        "_".to_string()
    } else {
        slug
    }
}

fn encode_line(line: &str) -> String {
    let has_trailing_under = line.contains("_ ");
    let mut encoded = unquote(line);
    for (before, after) in [
        ("_", "__"),
        ("-", "--"),
        (" ", "_"),
        ("?", "~q"),
        ("%", "~p"),
        ("#", "~h"),
        ("\"", "''"),
        ("/", "~s"),
        ("\\", "~b"),
        ("\n", "~n"),
        ("&", "~a"),
        ("<", "~l"),
        (">", "~g"),
        ("\u{2018}", "'"),
        ("\u{2019}", "'"),
        ("\u{201C}", "\""),
        ("\u{201D}", "\""),
        ("\u{2013}", "-"),
    ] {
        encoded = encoded.replace(before, after);
    }
    if has_trailing_under {
        encoded = encoded.replace("___", "__-");
    }
    encoded
}

/// Decode a URL slug back into lines of text.
pub fn decode(slug: &str) -> Vec<String> {
    let has_dash = slug.contains("_----");
    let has_flag = slug.contains("_--");
    let has_arrow = slug.contains("_--~g");
    let has_under = slug.contains("___");

    let mut slug = slug.replace('_', " ").replace("  ", "_");
    slug = slug.replace('-', " ").replace("  ", "-");
    slug = slug.replace("''", "\"");

    if has_dash {
        slug = slug.replace("-- ", " --");
    } else if has_flag {
        slug = slug.replace("- ", " -");
    }
    if has_arrow {
        slug = slug.replace("- ~g", " -~g");
    }
    if has_under {
        slug = slug.replace("_ ", " _");
    }

    for (before, after) in [
        ("~q", "?"),
        ("~p", "%"),
        ("~h", "#"),
        ("~n", "\n"),
        ("~a", "&"),
        ("~l", "<"),
        ("~g", ">"),
        ("~b", "\\"),
    ] {
        slug = slug.replace(before, after);
    }

    slug.split('/')
        .map(|line| line.replace("~s", "/"))
        .collect()
}

/// Returns the canonical slug and whether it differs from the input.
pub fn normalize(slug: &str) -> (String, bool) {
    let slug = unquote(slug);
    let normalized = encode(&decode(&slug));
    let changed = normalized != slug;
    (normalized, changed)
}

pub fn slugify(value: &str) -> String {
    let filtered: String = value
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .collect();
    filtered.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_special_characters() {
        let lines = vec![
            "hello world".to_string(),
            "what? 100% #1 a/b \"q\" -dash_under".to_string(),
        ];
        assert_eq!(decode(&encode(&lines)), lines);
    }

    #[test]
    fn encodes_known_values() {
        assert_eq!(encode(&["a b", "c-d", "e_f"]), "a_b/c--d/e__f");
        assert_eq!(encode(&["", ""]), "_/_");
        assert_eq!(encode::<&str>(&[]), "_");
        assert_eq!(encode(&["what?"]), "what~q");
    }

    #[test]
    fn decodes_known_values() {
        assert_eq!(decode("a_b/c--d/e__f"), vec!["a b", "c-d", "e_f"]);
        assert_eq!(decode("hello~nworld"), vec!["hello\nworld"]);
        assert_eq!(decode("_"), vec![" "]);
    }

    #[test]
    fn normalizes() {
        assert_eq!(normalize("hello world"), ("hello_world".into(), true));
        assert_eq!(normalize("hello_world"), ("hello_world".into(), false));
    }
}
