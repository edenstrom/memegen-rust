use ab_glyph::{FontArc, FontVec};
use anyhow::{Context, Result};
use serde::Serialize;

use crate::assets::Source;
use crate::settings;

struct FontDef {
    filename: &'static str,
    id: &'static str,
    alias: Option<&'static str>,
}

const FONT_DEFS: &[FontDef] = &[
    FontDef {
        filename: "TitilliumWeb-Black.ttf",
        id: "titilliumweb",
        alias: Some("thick"),
    },
    FontDef {
        filename: "NotoSans-Bold.ttf",
        id: "notosans",
        alias: None,
    },
    FontDef {
        filename: "Kalam-Regular.ttf",
        id: "kalam",
        alias: Some("comic"),
    },
    FontDef {
        filename: "Impact.ttf",
        id: "impact",
        alias: None,
    },
    FontDef {
        filename: "TitilliumWeb-SemiBold.ttf",
        id: "titilliumweb-thin",
        alias: Some("thin"),
    },
    FontDef {
        filename: "Segoe UI Bold.ttf",
        id: "segoe",
        alias: Some("tiny"),
    },
    FontDef {
        filename: "HG-Mincho-B.ttc",
        id: "hgminchob",
        alias: Some("jp"),
    },
    FontDef {
        filename: "NotoSansHebrew-Bold.ttf",
        id: "notosanshebrew",
        alias: Some("he"),
    },
];

fn path(filename: &str) -> String {
    format!("fonts/{filename}")
}

pub struct Font {
    pub id: &'static str,
    pub alias: Option<&'static str>,
    pub filename: &'static str,
    pub data: FontArc,
}

impl Font {
    pub fn is_impact(&self) -> bool {
        self.id == "impact"
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct FontInfo {
    pub id: String,
    pub alias: Option<String>,
    pub filename: String,
    #[serde(rename = "_self")]
    pub self_url: String,
}

pub struct Fonts {
    fonts: Vec<Font>,
}

impl Fonts {
    /// Asset paths of every font file.
    pub fn paths() -> Vec<String> {
        FONT_DEFS.iter().map(|def| path(def.filename)).collect()
    }

    pub fn load(source: &dyn Source) -> Result<Self> {
        let fonts = FONT_DEFS
            .iter()
            .map(|def| {
                let path = path(def.filename);
                let bytes = source
                    .read(&path)
                    .with_context(|| format!("reading font {path}"))?;
                let data = FontVec::try_from_vec_and_index(bytes.to_vec(), 0)
                    .with_context(|| format!("parsing font {path}"))?;
                Ok(Font {
                    id: def.id,
                    alias: def.alias,
                    filename: def.filename,
                    data: FontArc::new(data),
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self { fonts })
    }

    /// Look up a font by ID or alias; an empty name selects the default.
    pub fn get(&self, name: &str) -> Option<&Font> {
        let name = if name.is_empty() {
            settings::DEFAULT_FONT
        } else {
            name
        };
        self.fonts
            .iter()
            .find(|font| font.id == name || font.alias == Some(name))
    }

    pub fn all(&self) -> &[Font] {
        &self.fonts
    }

    pub fn info(&self, font: &Font, base_url: &str) -> FontInfo {
        FontInfo {
            id: font.id.to_string(),
            alias: font.alias.map(str::to_string),
            filename: font.filename.to_string(),
            self_url: format!("{base_url}/fonts/{}", font.id),
        }
    }
}
