use std::path::PathBuf;

use crate::settings;

/// Runtime configuration, mostly sourced from environment variables
/// (mirrors upstream `DOMAIN`, `DEBUG`, `DEFAULT_*_EXTENSION`).
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory containing `templates/`, `fonts/`, `emoji/`, and `static/`.
    pub root: PathBuf,
    /// Scheme and host used for absolute URLs in API responses.
    pub base_url: String,
    /// Bypass the render cache (upstream rebuilds images when not deployed).
    pub debug: bool,
    pub default_static_extension: String,
    pub default_animated_extension: String,
}

impl Config {
    pub fn from_env(root: Option<PathBuf>, port: u16) -> Self {
        let root = root
            .or_else(|| std::env::var_os("MEMEGEN_ROOT").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        let base_url = match std::env::var("DOMAIN") {
            Ok(domain) if !domain.is_empty() => format!("https://{domain}"),
            _ => format!("http://localhost:{port}"),
        };
        let debug = std::env::var("DEBUG").is_ok_and(|value| value == "true");
        let extension = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| settings::ALLOWED_EXTENSIONS.contains(&value.as_str()))
                .unwrap_or_else(|| default.to_string())
        };
        Self {
            root,
            base_url,
            debug,
            default_static_extension: extension(
                "DEFAULT_STATIC_EXTENSION",
                settings::DEFAULT_STATIC_EXTENSION,
            ),
            default_animated_extension: extension(
                "DEFAULT_ANIMATED_EXTENSION",
                settings::DEFAULT_ANIMATED_EXTENSION,
            ),
        }
    }
}
