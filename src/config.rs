use crate::settings;

/// Runtime configuration, mostly sourced from environment variables
/// (mirrors upstream `DOMAIN`, `DEBUG`, `DEFAULT_*_EXTENSION`).
#[derive(Debug, Clone)]
pub struct Config {
    /// Scheme and host used for absolute URLs in API responses.
    pub base_url: String,
    /// Bypass the render cache (upstream rebuilds images when not deployed).
    pub debug: bool,
    pub default_static_extension: String,
    pub default_animated_extension: String,
}

impl Default for Config {
    fn default() -> Self {
        Self::from_vars(|_| None, "http://localhost:5000".into())
    }
}

impl Config {
    /// Build from variables looked up with `var`; `base_url` applies when
    /// `DOMAIN` isn't set.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>, base_url: String) -> Self {
        let base_url = match var("DOMAIN") {
            Some(domain) if !domain.is_empty() => format!("https://{domain}"),
            _ => base_url,
        };
        let debug = var("DEBUG").is_some_and(|value| value == "true");
        let extension = |name: &str, default: &str| {
            var(name)
                .filter(|value| settings::ALLOWED_EXTENSIONS.contains(&value.as_str()))
                .unwrap_or_else(|| default.to_string())
        };
        Self {
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

    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_env(port: u16) -> Self {
        Self::from_vars(
            |name| std::env::var(name).ok(),
            format!("http://localhost:{port}"),
        )
    }
}
