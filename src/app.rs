//! Shared application state and operations used by the HTTP API and MCP.

use std::sync::Arc;

use bytes::Bytes;
use serde::Serialize;

use crate::assets::Assets;
use crate::config::Config;
use crate::fonts::Fonts;
use crate::render::{MemeRequest, RenderError, Rendered, Renderer};
use crate::settings;
use crate::template::{Catalog, Template, TemplateInfo};

pub struct App {
    pub renderer: Renderer,
    assets: Arc<dyn Assets>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ExampleImage {
    pub url: String,
    pub template: String,
}

impl App {
    pub fn new(
        config: Config,
        catalog: Catalog,
        fonts: Fonts,
        assets: Arc<dyn Assets>,
    ) -> Arc<Self> {
        tracing::info!(
            "Loaded {} templates and {} fonts",
            catalog.len(),
            fonts.all().len()
        );
        Arc::new(Self {
            renderer: Renderer::new(config, Arc::new(catalog), Arc::new(fonts)),
            assets,
        })
    }

    /// Load templates and fonts from a local directory.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load(config: Config, directory: crate::assets::Directory) -> anyhow::Result<Arc<Self>> {
        let catalog = Catalog::load(directory.root())?;
        let fonts = Fonts::load(&directory)?;
        Ok(Self::new(config, catalog, fonts, Arc::new(directory)))
    }

    pub fn config(&self) -> &Config {
        self.renderer.config()
    }

    pub fn catalog(&self) -> &Catalog {
        self.renderer.catalog()
    }

    pub fn fonts(&self) -> &Fonts {
        self.renderer.fonts()
    }

    pub fn base_url(&self) -> &str {
        &self.config().base_url
    }

    pub fn template_info(&self, template: &Template) -> TemplateInfo {
        let config = self.config();
        template.info(
            &config.base_url,
            &config.default_static_extension,
            &config.default_animated_extension,
        )
    }

    pub fn templates(&self, filter: &str, animated: Option<bool>) -> Vec<TemplateInfo> {
        self.catalog()
            .filter(filter, animated)
            .into_iter()
            .map(|template| self.template_info(template))
            .collect()
    }

    /// Upstream `get_example_images`.
    pub fn examples(&self, filter: &str, animated: Option<bool>) -> Vec<ExampleImage> {
        let config = self.config();
        self.catalog()
            .filter(filter, animated)
            .into_iter()
            .map(|template| {
                let extension = if template.is_animated() && animated != Some(false) {
                    &config.default_animated_extension
                } else {
                    &config.default_static_extension
                };
                ExampleImage {
                    url: template.image_url(&config.base_url, &template.example, extension),
                    template: format!("{}/templates/{}", config.base_url, template.id),
                }
            })
            .collect()
    }

    /// Upstream `Template.build_custom_url`; returns the URL and whether the
    /// template exists.
    pub fn build_url(
        &self,
        template_id: &str,
        lines: &[String],
        font: &str,
        extension: &str,
    ) -> (String, bool) {
        let config = self.config();
        let template = self
            .catalog()
            .get(template_id)
            .filter(|template| template.valid());
        let (lines, default_extension) = match template {
            Some(template) => (
                template.normalize_lines(lines),
                template
                    .default_extension(
                        &config.default_static_extension,
                        &config.default_animated_extension,
                    )
                    .to_string(),
            ),
            None => (lines.to_vec(), config.default_static_extension.clone()),
        };
        let extension = if settings::ALLOWED_EXTENSIONS.contains(&extension) {
            extension.to_string()
        } else {
            default_extension
        };
        let mut url = format!(
            "{}/images/{template_id}/{}.{extension}",
            config.base_url,
            crate::slug::encode(&lines)
        );
        if !font.is_empty() && font != settings::PLACEHOLDER {
            url.push_str("?font=");
            url.push_str(font);
        }
        (url, template.is_some())
    }

    /// A file from `static/`.
    pub async fn static_file(&self, name: &str) -> Option<Bytes> {
        let path = format!("static/{name}");
        self.assets.load(vec![path.clone()]).await.remove(&path)
    }

    /// Render on the blocking pool; rendering is CPU-bound.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn render(
        self: &Arc<Self>,
        request: MemeRequest,
    ) -> Result<Arc<Rendered>, RenderError> {
        let files = match self.assets.source() {
            Some(_) => Default::default(),
            None => self.assets.load(self.renderer.missing(&request)).await,
        };
        let app = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let source = app.assets.source().unwrap_or(&files);
            app.renderer.render(&request, source)
        })
        .await
        .map_err(|error| RenderError::Internal(error.to_string()))?
    }

    /// Load the files the render needs, then render on this thread (Workers
    /// isolates are single-threaded).
    #[cfg(target_arch = "wasm32")]
    pub async fn render(
        self: &Arc<Self>,
        request: MemeRequest,
    ) -> Result<Arc<Rendered>, RenderError> {
        let files = self.assets.load(self.renderer.missing(&request)).await;
        self.renderer.render(&request, &files)
    }
}
