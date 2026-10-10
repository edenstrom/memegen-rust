//! Unauthenticated MCP server exposing meme generation as tools.

#[cfg(not(target_arch = "wasm32"))]
pub mod files;
pub mod stateless;

use std::sync::Arc;

use base64::Engine as _;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    CallToolResult, ContentBlock, ExtensionCapabilities, Implementation, ListResourcesResult,
    MetaObject, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::App;
use crate::render::{AnimateText, MemeRequest};

const INSTRUCTIONS: &str = "Generate meme images from 500+ classic templates. \
Typical flow: call `list_templates` to find a template, then call `generate_meme` with its ID and one \
string per line. Prefer a `filter`: it runs a ranked search over names, keywords, and descriptions \
(e.g. \"drake\", \"surprised\", \"choosing between options\"). Without one, `list_templates` pages \
through every template with a description of what it means and what each line is for; pass \
`next_offset` back as `offset` to get the next page. \
Text is raw (no URL escaping needed); `:alias:` emoji shortcodes like `:fire:` are supported. \
Animated templates render as GIF/WebP/MP4; use `extension` to choose the format. \
To render several memes at once, call `generate_memes` with a list of them.";

#[cfg(not(target_arch = "wasm32"))]
const GENERATE_MEME: &str = "Render a meme image from a template and lines of text. Returns the \
image plus a shareable URL (served by `memegen serve`) or, over stdio, the local path of a saved \
copy (`saved_to`). Optionally saves the image to `save_to` instead. Inline images over 1 MB are \
downscaled; the URL and saved file are full size.";
#[cfg(target_arch = "wasm32")]
const GENERATE_MEME: &str = "Render a meme image from a template and lines of text. Returns the \
image and a shareable URL. Inline images over 1 MB are downscaled; the URL is full size.";

#[cfg(not(target_arch = "wasm32"))]
const GENERATE_MEMES: &str = "Render several memes in one call, e.g. variations to choose from. \
Each entry in `memes` takes the same fields as `generate_meme`. Returns, in order, each image \
and its summary (with `index`), or an `error` for entries that failed. The inline images share \
the 1 MB limit, so they are downscaled more as the batch grows; URLs and saved files are full size.";
#[cfg(target_arch = "wasm32")]
const GENERATE_MEMES: &str = "Render several memes in one call, e.g. variations to choose from. \
Each entry in `memes` takes the same fields as `generate_meme`. Returns, in order, each image \
and its summary (with `index`), or an `error` for entries that failed. The inline images share \
the 1 MB limit, so they are downscaled more as the batch grows; URLs are full size.";

/// MCP Apps (SEP-1865): hosts that support it render the meme tools' results
/// in this HTML view, where the user can edit the text and re-render.
const APP_EXTENSION: &str = "io.modelcontextprotocol/ui";
const APP_MIME_TYPE: &str = "text/html;profile=mcp-app";
const APP_URI: &str = "ui://memegen/meme.html";
const APP_HTML: &str = include_str!("mcp/app.html");

/// Tool `_meta` linking a tool to the view. `ui/resourceUri` is the older,
/// flat form some hosts still read.
fn app_meta() -> MetaObject {
    let meta = json!({ "ui": { "resourceUri": APP_URI }, "ui/resourceUri": APP_URI });
    MetaObject(meta.as_object().cloned().unwrap_or_default())
}

fn app_resource_meta() -> MetaObject {
    let meta = json!({ "ui": { "prefersBorder": false } });
    MetaObject(meta.as_object().cloned().unwrap_or_default())
}

/// Most memes one `generate_memes` call renders.
const MAX_BATCH: usize = 10;

/// Templates per `list_templates` page with a `filter`, best match first.
const DEFAULT_MATCHES: usize = 20;

/// Templates per `list_templates` page without a `filter`, and the most any
/// page returns. The full catalog is too large for one tool result.
const MAX_PAGE: usize = 100;

/// Clients reject tool results over 1 MB. Base64 adds a third,
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
    /// Number of templates to skip: the `next_offset` of the previous page.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Templates per page, at most 100. Defaults to 20 with a `filter` and 100 without.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTemplateRequest {
    /// Template ID, e.g. "fry" or "drake".
    pub id: String,
}

/// `animate_text`: on or off, or how to type the text out.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum AnimateTextArg {
    Flag(bool),
    Unit(TextUnit),
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TextUnit {
    Characters,
    Words,
}

impl From<Option<AnimateTextArg>> for AnimateText {
    fn from(arg: Option<AnimateTextArg>) -> Self {
        match arg {
            None | Some(AnimateTextArg::Flag(false)) => AnimateText::Off,
            Some(AnimateTextArg::Flag(true) | AnimateTextArg::Unit(TextUnit::Characters)) => {
                AnimateText::Characters
            }
            Some(AnimateTextArg::Unit(TextUnit::Words)) => AnimateText::Words,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateMemeRequest {
    /// Template ID from `list_templates`, e.g. "fry".
    pub template_id: String,
    /// Lines of text in order, one per text box (see the template's `lines`). Use "" to leave a box empty.
    pub text: Vec<String>,
    /// Image format: "png" (default for static templates), "jpg", "gif" (default for animated templates), "webp", or "mp4" (H.264 video, far smaller than GIF; returned as a URL or saved file, not inline).
    #[serde(default)]
    pub extension: Option<String>,
    /// Font ID or alias from `list_fonts`, e.g. "impact" or "comic". Defaults to each template's font.
    #[serde(default)]
    pub font: Option<String>,
    /// Type the text boxes out one after another, then hold the finished meme for a few seconds before looping: true or "characters" for a character at a time, "words" for a word at a time (MP4 eases the timing: slow, fast, then slow again). Works with any template; needs "gif" (the default when this is set), "webp" or "mp4".
    #[serde(default)]
    pub animate_text: Option<AnimateTextArg>,
    /// Absolute file path to write the image to, e.g. "/tmp/meme.png".
    #[cfg(not(target_arch = "wasm32"))]
    #[serde(default)]
    pub save_to: Option<String>,
    /// Return the image inline in the tool result (default true).
    #[serde(default)]
    pub include_image: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateMemesRequest {
    /// Memes to render, 1 to 10.
    pub memes: Vec<GenerateMemeRequest>,
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

#[derive(Serialize)]
struct TemplatePage<'a> {
    templates: Vec<TemplateSummary<'a>>,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<usize>,
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
        text. With `filter` it returns the 20 templates that match best. Without `filter` it pages \
        through every template, 100 at a time. Pass `next_offset` back as `offset` for the next \
        page; it is omitted on the last one."
    )]
    async fn list_templates(
        &self,
        Parameters(request): Parameters<ListTemplatesRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let filter = request.filter.unwrap_or_default();
        let templates = self.app.catalog().filter(&filter, request.animated);
        let searching = !filter.trim().is_empty();
        if searching && templates.is_empty() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "No templates match {filter:?}. Call list_templates without a filter to \
                browse every template and its description."
            ))]));
        }
        let default_limit = if searching { DEFAULT_MATCHES } else { MAX_PAGE };
        let limit = request.limit.unwrap_or(default_limit).clamp(1, MAX_PAGE);
        let total = templates.len();
        let start = request.offset.unwrap_or(0).min(total);
        let end = start.saturating_add(limit).min(total);
        let page = TemplatePage {
            templates: templates[start..end]
                .iter()
                .map(|template| TemplateSummary {
                    id: &template.id,
                    name: &template.name,
                    description: template.description.as_deref(),
                    lines: template.text.len(),
                    animated: template.is_animated(),
                    example: &template.example,
                })
                .collect(),
            total,
            next_offset: Some(end).filter(|&end| end < total),
        };
        // Compact: catalog pages are the largest results an agent reads.
        let text = serde_json::to_string(&page)
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

    #[tool(description = GENERATE_MEME, meta = app_meta())]
    async fn generate_meme(
        &self,
        Parameters(request): Parameters<GenerateMemeRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.generate(request, MAX_INLINE_IMAGE_BYTES).await {
            Ok(generated) => Ok(CallToolResult::success(generated.content())),
            Err(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        }
    }

    #[tool(description = GENERATE_MEMES, meta = app_meta())]
    async fn generate_memes(
        &self,
        Parameters(request): Parameters<GenerateMemesRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let count = request.memes.len();
        if count == 0 || count > MAX_BATCH {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Pass between 1 and {MAX_BATCH} memes; got {count}."
            ))]));
        }
        // The limit applies to the whole result, so the inline images share it.
        let inline = request
            .memes
            .iter()
            .filter(|meme| meme.include_image.unwrap_or(true))
            .count();
        let max_inline = MAX_INLINE_IMAGE_BYTES / inline.max(1);
        let results = futures::future::join_all(request.memes.into_iter().map(|meme| {
            let template_id = meme.template_id.clone();
            async move { (template_id, self.generate(meme, max_inline).await) }
        }))
        .await;

        let failed = results.iter().filter(|(_, result)| result.is_err()).count();
        let mut content = Vec::new();
        for (index, (template_id, result)) in results.into_iter().enumerate() {
            match result {
                Ok(mut generated) => {
                    generated.summary["index"] = json!(index);
                    content.extend(generated.content());
                }
                Err(error) => content.push(ContentBlock::text(
                    json!({ "index": index, "template_id": template_id, "error": error })
                        .to_string(),
                )),
            }
        }
        Ok(if failed == count {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        })
    }
}

/// A rendered meme: the inline image, if requested, and a JSON summary.
struct Generated {
    image: Option<ContentBlock>,
    summary: serde_json::Value,
}

impl Generated {
    fn content(self) -> Vec<ContentBlock> {
        let summary = ContentBlock::text(self.summary.to_string());
        self.image.into_iter().chain([summary]).collect()
    }
}

impl MemegenMcp {
    /// Render, save, and summarize one meme, shrinking the inline image to
    /// `max_inline` bytes.
    async fn generate(
        &self,
        request: GenerateMemeRequest,
        max_inline: usize,
    ) -> Result<Generated, String> {
        let config = self.app.config();
        let template = self
            .app
            .catalog()
            .get(&request.template_id)
            .filter(|t| t.valid());
        let Some(template) = template else {
            return Err(format!(
                "Template not found: {}. Use list_templates to find valid IDs.",
                request.template_id
            ));
        };
        let animate_text = AnimateText::from(request.animate_text);
        let extension = request
            .extension
            .filter(|ext| !ext.is_empty())
            .unwrap_or_else(|| {
                if animate_text.is_on() {
                    return config.default_animated_extension.clone();
                }
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

        let rendered = self
            .app
            .render(MemeRequest {
                template_id: template.id.clone(),
                lines: request.text.clone(),
                font: font.clone(),
                extension: extension.clone(),
                animate_text,
            })
            .await
            .map_err(|error| error.to_string())?;

        #[cfg(not(target_arch = "wasm32"))]
        let saved_to = {
            self.sweep();
            match (
                request.save_to.filter(|path| !path.is_empty()),
                &self.output,
            ) {
                (Some(path), _) => Some(save(path.into(), &rendered.bytes).await?),
                (None, Some(output)) => {
                    let key =
                        files::key(&template.id, &request.text, &font, &extension, animate_text);
                    let (dir, existing) = (output.dir.clone(), key.clone());
                    let reused = tokio::task::spawn_blocking(move || files::reuse(&dir, &existing))
                        .await
                        .ok()
                        .flatten();
                    Some(match reused {
                        Some(path) => path.display().to_string(),
                        None => {
                            let name = files::file_name(&key, std::time::SystemTime::now());
                            save(output.dir.join(name), &rendered.bytes).await?
                        }
                    })
                }
                (None, None) => None,
            }
        };
        #[cfg(target_arch = "wasm32")]
        let saved_to: Option<String> = None;

        let (url, _) =
            self.app
                .build_url(&template.id, &request.text, &font, &extension, animate_text);
        // Over stdio nothing serves localhost URLs, so return only the file.
        #[cfg(not(target_arch = "wasm32"))]
        let url = Some(url).filter(|_| {
            self.output.is_none() || !self.app.base_url().starts_with("http://localhost:")
        });
        let content_type = rendered.content_type;
        let bytes = rendered.bytes.len();
        // MCP image content can't hold a video.
        let inline = if request.include_image.unwrap_or(true) && content_type.starts_with("image/")
        {
            Some(
                self.app
                    .fit(rendered, max_inline)
                    .await
                    .map_err(|error| error.to_string())?,
            )
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
        let image = inline.map(|inline| {
            let data = base64::engine::general_purpose::STANDARD.encode(&inline);
            ContentBlock::image(data, content_type)
        });
        Ok(Generated { image, summary })
    }
}

/// Write the image to `path`; returns the path written.
#[cfg(not(target_arch = "wasm32"))]
async fn save(path: std::path::PathBuf, bytes: &[u8]) -> Result<String, String> {
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
    Ok(path.display().to_string())
}

impl MemegenMcp {
    fn resources(&self) -> ListResourcesResult {
        let resource = Resource::new(APP_URI, "meme-viewer")
            .with_title("Meme viewer")
            .with_description("Shows generated memes and lets the user edit their text.")
            .with_mime_type(APP_MIME_TYPE)
            .with_meta(app_resource_meta());
        ListResourcesResult::with_all_items(vec![resource])
    }

    fn resource(&self, uri: &str) -> Result<ReadResourceResult, ErrorData> {
        if uri != APP_URI {
            return Err(ErrorData::resource_not_found(
                format!("Resource not found: {uri}"),
                None,
            ));
        }
        Ok(ReadResourceResult::new(vec![
            ResourceContents::TextResourceContents {
                uri: APP_URI.into(),
                mime_type: Some(APP_MIME_TYPE.into()),
                text: APP_HTML.into(),
                meta: Some(app_resource_meta()),
            },
        ]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MemegenMcp {
    fn get_info(&self) -> ServerConfig {
        let mut extensions = ExtensionCapabilities::new();
        extensions.insert(
            APP_EXTENSION.into(),
            json!({ "mimeTypes": [APP_MIME_TYPE] })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        let capabilities = ServerCapabilities::builder()
            .enable_extensions_with(extensions)
            .enable_resources()
            .enable_tools()
            .build();
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("memegen", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(self.resources())
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        self.resource(&request.uri).map(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_animate_text() {
        let parse = |value: serde_json::Value| {
            let request: GenerateMemeRequest = serde_json::from_value(
                json!({ "template_id": "fry", "text": [], "animate_text": value }),
            )
            .unwrap();
            AnimateText::from(request.animate_text)
        };
        assert_eq!(parse(json!(true)), AnimateText::Characters);
        assert_eq!(parse(json!("characters")), AnimateText::Characters);
        assert_eq!(parse(json!("words")), AnimateText::Words);
        assert_eq!(parse(json!(false)), AnimateText::Off);
        assert_eq!(parse(json!(null)), AnimateText::Off);
    }
}
