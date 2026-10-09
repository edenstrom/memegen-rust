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
    let mut encoded = String::new();
    let mut previous = None;
    for c in unquote(line).chars() {
        let c = match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            '\u{2013}' => '-',
            c => c,
        };
        match c {
            '_' => encoded.push_str("__"),
            '-' => encoded.push_str("--"),
            // A space after an underscore is `-` so `decode` doesn't read the
            // run of underscores as a leading space.
            ' ' if previous == Some('_') => encoded.push('-'),
            ' ' => encoded.push('_'),
            '?' => encoded.push_str("~q"),
            '%' => encoded.push_str("~p"),
            '#' => encoded.push_str("~h"),
            '"' => encoded.push_str("''"),
            '/' => encoded.push_str("~s"),
            '\\' => encoded.push_str("~b"),
            '\n' => encoded.push_str("~n"),
            '&' => encoded.push_str("~a"),
            '<' => encoded.push_str("~l"),
            '>' => encoded.push_str("~g"),
            c => encoded.push(c),
        }
        previous = Some(c);
    }
    encoded
}

/// Decode runs of `_` and `-`: each pair is a literal character and an odd
/// one out is a space (before underscores, after dashes, as upstream did).
fn decode_separators(slug: &str) -> String {
    let mut decoded = String::with_capacity(slug.len());
    let mut chars = slug.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '_' && c != '-' {
            decoded.push(c);
            continue;
        }
        let mut count = 1;
        while chars.next_if_eq(&c).is_some() {
            count += 1;
        }
        let literal = c.to_string().repeat(count / 2);
        match (count % 2 == 1, c) {
            (false, _) => decoded.push_str(&literal),
            (true, '_') => {
                decoded.push(' ');
                decoded.push_str(&literal);
            }
            (true, _) => {
                decoded.push_str(&literal);
                decoded.push(' ');
            }
        }
    }
    decoded
}

/// Decode a URL slug back into lines of text.
pub fn decode(slug: &str) -> Vec<String> {
    let mut slug = decode_separators(slug).replace("''", "\"");

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
    fn round_trips_spaces_next_to_dashes_and_underscores() {
        for line in [
            "a - b",
            "a -- b",
            "a -b",
            "a- b",
            "a _b",
            "a_ b",
            "a__ b",
            "a __b",
            "a_ _b",
            "a _b c_ d",
            "_lead and trail_",
            "-lead and trail-",
            "a ->b",
            "me - also me",
        ] {
            let lines = vec![line.to_string()];
            assert_eq!(decode(&encode(&lines)), lines, "{line:?}");
            assert_eq!(
                normalize(&encode(&lines)),
                (encode(&lines), false),
                "{line:?}"
            );
        }
    }

    #[test]
    fn normalizes_typographic_punctuation() {
        assert_eq!(encode(&["a \u{2013} b"]), "a_--_b");
        assert_eq!(encode(&["\u{201C}hi\u{201D} it\u{2019}s"]), "''hi''_it's");
    }

    #[test]
    fn decodes_upstream_urls() {
        assert_eq!(decode("a-b_c"), vec!["a b c"]);
        assert_eq!(decode("a_----b"), vec!["a --b"]);
        assert_eq!(decode("a_--~gb"), vec!["a ->b"]);
        assert_eq!(decode("a__-b"), vec!["a_ b"]);
        assert_eq!(decode("a___b"), vec!["a _b"]);
        assert_eq!(decode("a----b"), vec!["a--b"]);
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
