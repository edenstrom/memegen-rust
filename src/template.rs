//! Template catalog, ported from upstream `app/models/template.py`.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::search::{Index, Query};
use crate::{slug, textbox::TextBox};

const PLACEHOLDER_SUFFIX: &str = "img";

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct Overlay {
    #[serde(default = "half")]
    center_x: f32,
    #[serde(default = "half")]
    center_y: f32,
    #[serde(default)]
    angle: f32,
    #[serde(default = "quarter")]
    scale: f32,
    #[serde(default)]
    start: f32,
    #[serde(default = "one")]
    stop: f32,
}

fn half() -> f32 {
    0.5
}
fn quarter() -> f32 {
    0.25
}
fn one() -> f32 {
    1.0
}

impl Default for Overlay {
    fn default() -> Self {
        Self {
            center_x: 0.5,
            center_y: 0.5,
            angle: 0.0,
            scale: 0.25,
            start: 0.0,
            stop: 1.0,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawTemplate {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    keywords: Vec<Option<String>>,
    #[serde(default)]
    text: Option<Vec<TextBox>>,
    #[serde(default)]
    example: Option<Vec<Option<String>>>,
    #[serde(default)]
    overlay: Option<Vec<Overlay>>,
}

#[derive(Debug)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub source: Option<String>,
    /// What the meme expresses and when to use it.
    pub description: Option<String>,
    pub keywords: Vec<String>,
    pub text: Vec<TextBox>,
    pub example: Vec<String>,
    pub overlays: usize,
    pub styles: Vec<String>,
    /// Asset path of the background for static output (first frame if only
    /// a GIF exists).
    pub static_image: Option<String>,
    /// Asset path of the background for animated output (GIF preferred).
    pub animated_image: Option<String>,
    index: Index,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct Example {
    pub text: Vec<String>,
    pub url: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TemplateInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub lines: usize,
    pub overlays: usize,
    pub styles: Vec<String>,
    pub blank: String,
    pub example: Example,
    pub source: Option<String>,
    pub keywords: Vec<String>,
    #[serde(rename = "_self")]
    pub self_url: String,
}

impl Template {
    /// Build a template from its `config.yml` and the file names in its
    /// directory.
    pub fn parse(id: &str, config: &str, files: &[String]) -> Result<Self> {
        let raw: RawTemplate = serde_yaml::from_str(config).context("parsing config.yml")?;

        let mut files = files.to_vec();
        files.sort();

        let stem = |name: &str| match name.rsplit_once('.') {
            Some((stem, _)) if !stem.is_empty() => stem.to_string(),
            _ => name.to_string(),
        };
        let ext = |name: &str| match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => ext.to_lowercase(),
            _ => String::new(),
        };

        let overlay = raw.overlay.unwrap_or_else(|| vec![Overlay::default()]);
        let mut styles: Vec<String> = Vec::new();
        for name in &files {
            let stem = stem(name);
            if stem.starts_with(['.', '_']) {
                continue;
            }
            if stem != "config" && stem != "default" {
                styles.push(stem);
            } else if name == "default.gif" {
                styles.push("animated".into());
            }
        }
        if !styles.is_empty() || overlay != vec![Overlay::default()] {
            styles.push("default".into());
        }
        styles.sort();

        let defaults: Vec<&String> = files
            .iter()
            .filter(|name| stem(name) == "default" && ext(name) != PLACEHOLDER_SUFFIX)
            .collect();
        let path = |name: &String| format!("templates/{id}/{name}");
        let gif = defaults
            .iter()
            .find(|name| ext(name) == "gif")
            .map(|name| path(name));
        let still = defaults
            .iter()
            .find(|name| ext(name) != "gif")
            .map(|name| path(name));

        let mut template = Self {
            id: id.to_string(),
            name: raw.name.unwrap_or_default(),
            source: raw.source.filter(|s| !s.is_empty()),
            description: raw.description.filter(|s| !s.is_empty()),
            keywords: raw.keywords.into_iter().flatten().collect(),
            text: raw.text.unwrap_or_else(|| {
                vec![
                    TextBox::default(),
                    TextBox {
                        anchor_y: 0.8,
                        ..Default::default()
                    },
                ]
            }),
            example: raw
                .example
                .map(|lines| lines.into_iter().map(Option::unwrap_or_default).collect())
                .unwrap_or_else(|| vec!["Top Line".into(), "Bottom Line".into()]),
            overlays: overlay.len(),
            styles,
            static_image: still.clone().or_else(|| gif.clone()),
            animated_image: gif.or(still),
            index: Index::default(),
        };
        template.index = Index::new(
            &template.id,
            &template.name,
            &template.keywords,
            template.description.as_deref(),
            &template.example,
        );
        Ok(template)
    }

    /// Public, renderable template (mirrors upstream `Template.valid`).
    pub fn valid(&self) -> bool {
        !self.id.starts_with('_') && !self.name.contains('/') && self.static_image.is_some()
    }

    pub fn is_animated(&self) -> bool {
        self.styles.iter().any(|style| style == "animated")
    }

    pub fn default_extension<'a>(&self, static_ext: &'a str, animated_ext: &'a str) -> &'a str {
        if self.is_animated() {
            animated_ext
        } else {
            static_ext
        }
    }

    /// Lowercase lines where the matching text box style would anyway.
    pub fn normalize_lines(&self, lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .enumerate()
            .map(|(index, line)| match self.text.get(index) {
                Some(text) => text.normalize(line),
                None => line.clone(),
            })
            .collect()
    }

    pub fn image_url(&self, base_url: &str, lines: &[String], extension: &str) -> String {
        format!(
            "{base_url}/images/{}/{}.{extension}",
            self.id,
            slug::encode(lines)
        )
    }

    pub fn info(&self, base_url: &str, static_ext: &str, animated_ext: &str) -> TemplateInfo {
        let extension = self.default_extension(static_ext, animated_ext);
        TemplateInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            lines: self.text.len(),
            overlays: if self.styles.is_empty() {
                0
            } else {
                self.overlays
            },
            styles: self.styles.clone(),
            blank: format!("{base_url}/images/{}.{static_ext}", self.id),
            example: Example {
                text: if self.example.iter().any(|line| !line.is_empty()) {
                    self.example.clone()
                } else {
                    vec![]
                },
                url: self.image_url(base_url, &self.example, extension),
            },
            source: self.source.clone(),
            keywords: self.keywords.clone(),
            self_url: format!("{base_url}/templates/{}", self.id),
        }
    }
}

pub struct Catalog {
    templates: BTreeMap<String, Arc<Template>>,
}

impl Catalog {
    /// Build the catalog from `(id, config.yml contents, file names)` entries,
    /// skipping templates whose config doesn't parse.
    pub fn new<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str, Vec<String>)>) -> Self {
        let mut templates = BTreeMap::new();
        for (id, config, files) in entries {
            match Template::parse(id, config, &files) {
                Ok(template) => {
                    templates.insert(id.to_string(), Arc::new(template));
                }
                Err(error) => tracing::warn!("Skipping template {id}: {error:#}"),
            }
        }
        Self { templates }
    }

    /// Load every template directory under `root/templates`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load(root: &std::path::Path) -> Result<Self> {
        let directory = root.join("templates");
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("reading {}", directory.display()))?
        {
            let path = entry?.path();
            let Ok(config) = std::fs::read_to_string(path.join("config.yml")) else {
                continue;
            };
            let Some(id) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let files: Vec<String> = std::fs::read_dir(&path)?
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect();
            entries.push((id.to_string(), config, files));
        }
        Ok(Self::new(entries.iter().map(|(id, config, files)| {
            (id.as_str(), config.as_str(), files.clone())
        })))
    }

    pub fn get(&self, id: &str) -> Option<&Arc<Template>> {
        self.templates.get(id)
    }

    /// Valid templates sorted by ID or, given a query, by how well they
    /// match it (see [`crate::search`]).
    pub fn filter(&self, query: &str, animated: Option<bool>) -> Vec<&Arc<Template>> {
        let query = Query::new(query);
        let templates = self
            .templates
            .values()
            .filter(|template| template.valid())
            .filter(|template| animated.is_none_or(|animated| template.is_animated() == animated));
        if query.is_empty() {
            return templates.collect();
        }
        query.rank(templates.map(|template| (&template.index, template)))
    }

    pub fn len(&self) -> usize {
        self.templates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }
}
