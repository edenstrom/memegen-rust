//! Unauthenticated MCP server exposing meme generation as tools.

#[cfg(not(target_arch = "wasm32"))]
pub mod files;
pub mod stateless;

use std::sync::Arc;

use base64::Engine as _;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::App;
use crate::render::MemeRequest;

const INSTRUCTIONS: &str = "Generate meme images from 200+ classic templates. \
Typical flow: call `list_templates` to find a template, then call `generate_meme` with its ID and one \
string per line. Without a `filter`, `list_templates` returns every template with a description of what \
it means and what each line is for, so you can pick the best fit; with a `filter` it runs a ranked search \
over names, keywords, and descriptions (e.g. \"drake\", \"surprised\", \"choosing between options\"). \
Text is raw (no URL escaping needed); `:alias:` emoji shortcodes like `:fire:` are supported. \
Animated templates render as GIF/WebP; use `extension` to choose the format.";

#[cfg(not(target_arch = "wasm32"))]
const GENERATE_MEME: &str = "Render a meme image from a template and lines of text. Returns the \
image plus a shareable URL (served by `memegen serve`) or, over stdio, the local path of a saved \
copy (`saved_to`). Optionally saves the image to `save_to` instead. Inline images over 1 MB are \
downscaled; the URL and saved file are full size.";
#[cfg(target_arch = "wasm32")]
const GENERATE_MEME: &str = "Render a meme image from a template and lines of text. Returns the \
image and a shareable URL. Inline images over 1 MB are downscaled; the URL is full size.";

/// Most results `list_templates` returns for a `filter`, best match first.
const MAX_MATCHES: usize = 20;

/// Clients reject tool results with images over 1 MB. Base64 adds a third,
/// so this keeps the encoded image under 1,000,000 bytes.
const MAX_INLINE_IMAGE_BYTES: usize = 750_000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTemplatesRequest {
    /// Words describing the template or the idea to express, e.g. "drake" or "awkward silence". Matched against ID, name, keywords, description, and example text, tolerating typos; best matches first. Omit to list every template.
    #[serde(default)]
    pub filter: Option<String>,
    /// Only animated templates (true) or only static templates (false).
    #[serde(default)]
    pub animated: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTemplateRequest {
    /// Template ID, e.g. "fry" or "drake".
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateMemeRequest {
    /// Template ID from `list_templates`, e.g. "fry".
    pub template_id: String,
    /// Lines of text in order, one per text box (see the template's `lines`). Use "" to leave a box empty.
    pub text: Vec<String>,
    /// Image format: "png" (default for static templates), "jpg", "gif" (default for animated templates), or "webp".
    #[serde(default)]
    pub extension: Option<String>,
    /// Font ID or alias from `list_fonts`, e.g. "impact" or "comic". Defaults to each template's font.
    #[serde(default)]
    pub font: Option<String>,
    /// Absolute file path to write the image to, e.g. "/tmp/meme.png".
    #[cfg(not(target_arch = "wasm32"))]
    #[serde(default)]
    pub save_to: Option<String>,
    /// Return the image inline in the tool result (default true).
    #[serde(default)]
    pub include_image: Option<bool>,
}

#[derive(Serialize)]
struct TemplateSummary<'a> {
    id: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    lines: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    animated: bool,
    example: &'a [String],
}

#[derive(Clone)]
pub struct MemegenMcp {
    app: Arc<App>,
    tool_router: ToolRouter<Self>,
    #[cfg(not(target_arch = "wasm32"))]
    output: Option<Arc<Output>>,
}

/// Where memes are saved when the caller doesn't pass `save_to`.
#[cfg(not(target_arch = "wasm32"))]
struct Output {
    dir: std::path::PathBuf,
    /// Unix seconds of the last sweep for expired files.
    last_sweep: std::sync::atomic::AtomicU64,
}

impl MemegenMcp {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            tool_router: Self::tool_router(),
            #[cfg(not(target_arch = "wasm32"))]
            output: None,
        }
    }

    /// Save every meme to `dir` and return its path, deleting saved memes
    /// older than [`files::MAX_AGE`].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_output_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.output = Some(Arc::new(Output {
            dir,
            last_sweep: Default::default(),
        }));
        self
    }

    /// Delete expired memes in the background, at most once an hour.
    #[cfg(not(target_arch = "wasm32"))]
    fn sweep(&self) {
        use std::sync::atomic::Ordering;

        let Some(output) = self.output.clone() else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last = output.last_sweep.load(Ordering::Relaxed);
        if now.saturating_sub(last) < 60 * 60
            || output
                .last_sweep
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        tokio::task::spawn_blocking(move || match files::sweep(&output.dir, files::MAX_AGE) {
            Ok(0) => {}
            Ok(removed) => tracing::info!("Deleted {removed} expired memes"),
            Err(error) => tracing::warn!("Could not clean up {}: {error}", output.dir.display()),
        });
    }
}

fn json_text(value: &impl Serialize) -> Result<CallToolResult, ErrorData> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

#[tool_router]
impl MemegenMcp {
    #[tool(
        description = "List meme templates with their ID, name, description (what the meme means \
        and what each line is for), number of text lines, whether they are animated, and example \
        text. Without `filter` this returns every template. With `filter` it returns up to 20 \
        templates ranked by how well they match."
    )]
    async fn list_templates(
        &self,
        Parameters(request): Parameters<ListTemplatesRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let filter = request.filter.unwrap_or_default();
        let mut templates = self.app.catalog().filter(&filter, request.animated);
        if !filter.trim().is_empty() {
            if templates.is_empty() {
                return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "No templates match {filter:?}. Call list_templates without a filter to \
                    browse every template and its description."
                ))]));
            }
            templates.truncate(MAX_MATCHES);
        }
        let summaries: Vec<TemplateSummary> = templates
            .iter()
            .map(|template| TemplateSummary {
                id: &template.id,
                name: &template.name,
                description: template.description.as_deref(),
                lines: template.text.len(),
                animated: template.is_animated(),
                example: &template.example,
            })
            .collect();
        // Compact: the full catalog is the largest result an agent reads.
        let text = serde_json::to_string(&summaries)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Get full details for one meme template, including keywords, source, and example URL."
    )]
    async fn get_template(
        &self,
        Parameters(request): Parameters<GetTemplateRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.app.catalog().get(&request.id) {
            Some(template) => json_text(&self.app.template_info(template)),
            None => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Template not found: {}. Use list_templates to find valid IDs.",
                request.id
            ))])),
        }
    }

    #[tool(description = "List fonts that can be passed as `font` to generate_meme.")]
    async fn list_fonts(&self) -> Result<CallToolResult, ErrorData> {
        let fonts: Vec<_> = self
            .app
            .fonts()
            .all()
            .iter()
            .map(|font| json!({ "id": font.id, "alias": font.alias, "filename": font.filename }))
            .collect();
        json_text(&fonts)
    }

    #[tool(description = GENERATE_MEME)]
    async fn generate_meme(
        &self,
        Parameters(request): Parameters<GenerateMemeRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let config = self.app.config();
        let template = self
            .app
            .catalog()
            .get(&request.template_id)
            .filter(|t| t.valid());
        let Some(template) = template else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Template not found: {}. Use list_templates to find valid IDs.",
                request.template_id
            ))]));
        };
        let extension = request
            .extension
            .filter(|ext| !ext.is_empty())
            .unwrap_or_else(|| {
                template
                    .default_extension(
                        &config.default_static_extension,
                        &config.default_animated_extension,
                    )
                    .to_string()
            })
            .trim_start_matches('.')
            .to_lowercase();
        let font = request.font.unwrap_or_default();

        let rendered = match self
            .app
            .render(MemeRequest {
                template_id: template.id.clone(),
                lines: request.text.clone(),
                font: font.clone(),
                extension: extension.clone(),
            })
            .await
        {
            Ok(rendered) => rendered,
            Err(error) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    error.to_string(),
                )]));
            }
        };

        #[cfg(not(target_arch = "wasm32"))]
        let saved_to = {
            self.sweep();
            let path = match request.save_to.filter(|path| !path.is_empty()) {
                Some(path) => Some(std::path::PathBuf::from(path)),
                None => self.output.as_ref().map(|output| {
                    output.dir.join(files::file_name(
                        &template.id,
                        &request.text,
                        &font,
                        &extension,
                    ))
                }),
            };
            match save(path, &rendered.bytes).await {
                Ok(saved_to) => saved_to,
                Err(message) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(message)]));
                }
            }
        };
        #[cfg(target_arch = "wasm32")]
        let saved_to: Option<String> = None;

        let (url, _) = self
            .app
            .build_url(&template.id, &request.text, &font, &extension);
        // Over stdio nothing serves localhost URLs, so return only the file.
        #[cfg(not(target_arch = "wasm32"))]
        let url = Some(url).filter(|_| {
            self.output.is_none() || !self.app.base_url().starts_with("http://localhost:")
        });
        let content_type = rendered.content_type;
        let bytes = rendered.bytes.len();
        let inline = if request.include_image.unwrap_or(true) {
            match self.app.fit(rendered, MAX_INLINE_IMAGE_BYTES).await {
                Ok(inline) => Some(inline),
                Err(error) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        error.to_string(),
                    )]));
                }
            }
        } else {
            None
        };
        let summary = json!({
            "url": url,
            "template_id": template.id,
            "text": request.text,
            "extension": extension,
            "content_type": content_type,
            "bytes": bytes,
            "inline_bytes": inline.as_ref().map(|inline| inline.len()),
            "saved_to": saved_to,
        });

        let mut content = Vec::new();
        if let Some(inline) = inline {
            let data = base64::engine::general_purpose::STANDARD.encode(&inline);
            content.push(ContentBlock::image(data, content_type));
        }
        content.push(ContentBlock::text(summary.to_string()));
        Ok(CallToolResult::success(content))
    }
}

/// Write the image to `path` if one was given; returns the path written.
#[cfg(not(target_arch = "wasm32"))]
async fn save(path: Option<std::path::PathBuf>, bytes: &[u8]) -> Result<Option<String>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.is_absolute() {
        return Err("`save_to` must be an absolute path".into());
    }
    let written = async {
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(&path, bytes).await
    };
    if let Err(error) = written.await {
        return Err(format!(
            "Rendered the meme but could not write {}: {error}",
            path.display()
        ));
    }
    Ok(Some(path.display().to_string()))
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MemegenMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("memegen", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
