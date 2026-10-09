//! Ranked template search. A query is split into words, and each word is
//! scored against a template's ID, name, keywords, description and example
//! text, so word order doesn't matter and near-miss spellings still match.

/// Words too common to say which template is wanted. They're only searched
/// for when the query has nothing else.
const STOPWORDS: &[&str] = &[
    "a",
    "an",
    "and",
    "are",
    "as",
    "at",
    "be",
    "by",
    "for",
    "from",
    "in",
    "is",
    "it",
    "meme",
    "memes",
    "of",
    "on",
    "or",
    "template",
    "templates",
    "that",
    "the",
    "this",
    "to",
    "with",
];

// How much a matching word counts for in each field.
const ID: f32 = 3.0;
const NAME: f32 = 3.0;
const KEYWORDS: f32 = 3.0;
const DESCRIPTION: f32 = 1.5;
const EXAMPLE: f32 = 1.0;

// How much each kind of word match counts for.
const EXACT: f32 = 1.0;
const PREFIX: f32 = 0.75;
const STEM: f32 = 0.5;
const TYPO: f32 = 0.5;

/// A template's searchable words, built once when the catalog loads.
#[derive(Debug, Default)]
pub struct Index {
    id: String,
    fields: Vec<(f32, Vec<String>)>,
    /// Name and keywords as space-padded word runs, for whole-phrase matches.
    phrases: Vec<String>,
}

impl Index {
    pub fn new(
        id: &str,
        name: &str,
        keywords: &[String],
        description: Option<&str>,
        example: &[String],
    ) -> Self {
        let mut phrases = vec![phrase(&words(name))];
        phrases.extend(keywords.iter().map(|keyword| phrase(&words(keyword))));
        Self {
            id: id.to_lowercase(),
            fields: vec![
                (ID, words(id)),
                (NAME, words(name)),
                (KEYWORDS, keywords.iter().flat_map(|k| words(k)).collect()),
                (DESCRIPTION, description.map(words).unwrap_or_default()),
                (
                    EXAMPLE,
                    example.iter().flat_map(|line| words(line)).collect(),
                ),
            ],
            phrases,
        }
    }
}

pub struct Query {
    raw: String,
    terms: Vec<String>,
}

impl Query {
    pub fn new(text: &str) -> Self {
        let all = words(text);
        let terms: Vec<String> = all
            .iter()
            .filter(|word| !STOPWORDS.contains(&word.as_str()))
            .cloned()
            .collect();
        Self {
            raw: text.trim().to_lowercase(),
            terms: if terms.is_empty() { all } else { terms },
        }
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// `items` ordered by how well their indexes match, best first, without
    /// the ones that don't match at all. Equal scores keep their input order.
    pub fn rank<'a, T>(&self, items: impl IntoIterator<Item = (&'a Index, T)>) -> Vec<T> {
        let matches: Vec<(Vec<f32>, &Index, T)> = items
            .into_iter()
            .map(|(index, item)| (self.matches(index), index, item))
            .collect();
        // Rarer words say more about which template is wanted, so "you" in a
        // query counts for less than "pigeon".
        let count = matches.len() as f32;
        let rarity: Vec<f32> = (0..self.terms.len())
            .map(|term| {
                let found = matches.iter().filter(|(m, ..)| m[term] > 0.0).count();
                (1.0 + count / found.max(1) as f32).ln()
            })
            .collect();
        let mut scored: Vec<(f32, T)> = matches
            .into_iter()
            .map(|(matches, index, item)| (self.score(&matches, &rarity, index), item))
            .filter(|(score, _)| *score > 0.0)
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        scored.into_iter().map(|(_, item)| item).collect()
    }

    /// The best field-weighted match for each term in `index`.
    fn matches(&self, index: &Index) -> Vec<f32> {
        self.terms
            .iter()
            .map(|term| {
                index
                    .fields
                    .iter()
                    .map(|(weight, words)| weight * quality(term, words))
                    .fold(0.0, f32::max)
            })
            .collect()
    }

    /// Templates matching more of the query (by rarity) always beat ones
    /// matching less of it, more strongly.
    fn score(&self, matches: &[f32], rarity: &[f32], index: &Index) -> f32 {
        let all: f32 = rarity.iter().sum();
        let covered: f32 = matches
            .iter()
            .zip(rarity)
            .filter(|(quality, _)| **quality > 0.0)
            .map(|(_, rarity)| rarity)
            .sum();
        if covered == 0.0 {
            return 0.0;
        }
        let total: f32 = matches.iter().zip(rarity).map(|(q, r)| q * r).sum();
        let coverage = covered / all;
        let mut score = total * coverage * coverage;
        if self.terms.len() > 1
            && index
                .phrases
                .iter()
                .any(|p| p.contains(&phrase(&self.terms)))
        {
            score += NAME * all;
        }
        if self.raw == index.id {
            score += 1000.0;
        }
        score
    }
}

/// Lowercase words with apostrophes dropped ("ain't" → "aint") and plurals
/// trimmed, so "dogs" finds "dog".
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .replace(['\'', '’'], "")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(stem)
        .collect()
}

fn stem(word: &str) -> String {
    let len = word.len();
    if len > 4 && word.ends_with("ies") {
        format!("{}y", &word[..len - 3])
    } else if len > 3 && word.ends_with('s') && !word.ends_with("ss") && !word.ends_with("us") {
        word[..len - 1].to_string()
    } else {
        word.to_string()
    }
}

fn phrase(words: &[String]) -> String {
    format!(" {} ", words.join(" "))
}

/// The best match between `term` and any of `words`.
fn quality(term: &str, words: &[String]) -> f32 {
    words
        .iter()
        .map(|word| {
            if word == term {
                EXACT
            } else if term.len() >= 3 && word.starts_with(term) {
                PREFIX
            } else if word.len() >= 4 && term.starts_with(word.as_str()) {
                STEM
            } else if term.len() >= 5 && word.len() >= 4 && within_one_edit(term, word) {
                TYPO
            } else {
                0.0
            }
        })
        .fold(0.0, f32::max)
}

/// Whether one insertion, deletion, substitution or swap of adjacent
/// characters turns `a` into `b`.
fn within_one_edit(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let same = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let (a, b) = (&a[same..], &b[same..]);
    match (a.len(), b.len()) {
        (0, n) | (n, 0) => n <= 1,
        _ => {
            a[1..] == b[1..]
                || a[1..] == *b
                || *a == b[1..]
                || (a.len() >= 2
                    && b.len() >= 2
                    && a[0] == b[1]
                    && a[1] == b[0]
                    && a[2..] == b[2..])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::Catalog;

    fn catalog() -> Catalog {
        Catalog::load(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn top(catalog: &Catalog, query: &str) -> Vec<String> {
        catalog
            .filter(query, None)
            .iter()
            .take(3)
            .map(|template| template.id.clone())
            .collect()
    }

    #[test]
    fn edits() {
        assert!(within_one_edit("drake", "drak"));
        assert!(within_one_edit("drake", "drakes"));
        assert!(within_one_edit("drake", "brake"));
        assert!(within_one_edit("drake", "darke"));
        assert!(!within_one_edit("drake", "dark"));
        assert!(!within_one_edit("drake", "duck"));
    }

    #[test]
    fn words_are_normalized() {
        assert_eq!(words("Ain't Nobody"), ["aint", "nobody"]);
        assert_eq!(words("Two Buttons, Puppies!"), ["two", "button", "puppy"]);
        assert_eq!(words("yes boss"), ["yes", "boss"]);
    }

    fn rank(query: &str, indexes: &[Index]) -> Vec<usize> {
        Query::new(query).rank(indexes.iter().zip(0..))
    }

    #[test]
    fn ranks_name_over_example() {
        let indexes = [
            Index::new("a", "Other", &[], None, &["pikachu".into()]),
            Index::new("b", "Surprised Pikachu", &[], None, &[]),
            Index::new("c", "Unrelated", &[], None, &[]),
        ];
        assert_eq!(rank("pikachu", &indexes), [1, 0]);
        assert_eq!(rank("pikahcu", &indexes), [1, 0]);
        assert!(rank("squirtle", &indexes).is_empty());
    }

    #[test]
    fn ranks_full_coverage_first() {
        let indexes = [
            Index::new("a", "Surprised Pikachu", &[], None, &[]),
            Index::new("b", "Surprised Cat", &[], None, &[]),
        ];
        assert_eq!(rank("surprised cat", &indexes), [1, 0]);
    }

    #[test]
    fn ranks_rare_words_higher() {
        let indexes = [
            Index::new("a", "You You", &[], None, &[]),
            Index::new("b", "You Pigeon", &[], None, &[]),
            Index::new("c", "You Again", &[], None, &[]),
        ];
        assert_eq!(rank("you pigeon", &indexes)[0], 1);
    }

    #[test]
    fn ranks_catalog() {
        let catalog = catalog();
        assert_eq!(top(&catalog, "drake")[0], "drake");
        assert_eq!(top(&catalog, "fry")[0], "fry");
        assert_eq!(top(&catalog, "FRY")[0], "fry");
        assert_eq!(top(&catalog, "distracted")[0], "db");
        assert_eq!(top(&catalog, "dsitracted")[0], "db");
        assert_eq!(top(&catalog, "the distracted boyfriend meme")[0], "db");
        assert!(catalog.filter("zzzzqqq", None).is_empty());
    }
}
