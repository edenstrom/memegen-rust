//! HTTP API mirroring upstream's Sanic routes.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde_json::{Value, json};

use crate::app::App;
use crate::render::{AnimateText, Background, Custom, MemeRequest, RenderError};
use crate::textbox::TextBox;
use crate::{openapi, settings, slug};

type AppState = Arc<App>;

/// Characters to escape when building redirect locations.
const PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'%');

/// Characters to escape in any redirect location (non-ASCII is always escaped).
const LOCATION: &AsciiSet = &CONTROLS.add(b' ');

const LANDING_HTML: &str = include_str!("landing.html");

pub fn router(app: AppState) -> Router {
    Router::new()
        .route("/", get(landing))
        .route("/docs", get(docs))
        .route("/docs/", get(docs))
        .route("/openapi.json", get(openapi_spec))
        .route("/favicon.ico", get(favicon))
        .route("/favicon.svg", get(favicon_svg))
        .route("/apple-touch-icon.png", get(apple_touch_icon))
        .route("/robots.txt", get(robots))
        .route("/templates", get(list_templates))
        .route("/templates/", get(list_templates))
        .route("/templates/{id}", get(template_detail).post(template_build))
        .route("/images", get(list_examples).post(create_image))
        .route("/images/", get(list_examples).post(create_image))
        .route("/images/{filename}", get(blank_image))
        .route(
            "/images/{template_id}/{*text_filepath}",
            get(meme_image)
                .post(meme_image_on_background)
                .layer(DefaultBodyLimit::max(settings::MAX_BACKGROUND_BYTES)),
        )
        .route("/fonts", get(list_fonts))
        .route("/fonts/", get(list_fonts))
        .route("/fonts/{id}", get(font_detail))
        .with_state(app)
}

/// Redirect with upstream's status codes (axum's helpers use 303/307/308).
fn redirect(status: StatusCode, location: &str) -> Response {
    let location = utf8_percent_encode(location, LOCATION).to_string();
    match HeaderValue::from_str(&location) {
        Ok(value) => (status, [(header::LOCATION, value)]).into_response(),
        Err(_) => error(StatusCode::BAD_REQUEST, "Invalid redirect location"),
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn flag(params: &HashMap<String, String>, name: &str) -> Option<bool> {
    match params.get(name)?.to_lowercase().as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// The landing page, with `{{BASE_URL}}` filled in so its snippets point at
/// this deployment.
async fn landing(State(app): State<AppState>) -> Html<String> {
    let base_url = app
        .base_url()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    Html(LANDING_HTML.replace("{{BASE_URL}}", &base_url))
}

async fn docs() -> Html<&'static str> {
    Html(openapi::DOCS_HTML)
}

async fn openapi_spec(State(app): State<AppState>) -> Json<Value> {
    Json(openapi::spec(app.base_url()))
}

async fn static_file(app: &App, name: &str, content_type: &'static str) -> Response {
    match app.static_file(name).await {
        Some(bytes) => ([(header::CONTENT_TYPE, content_type)], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn favicon(State(app): State<AppState>) -> Response {
    static_file(&app, "favicon.ico", "image/x-icon").await
}

async fn favicon_svg(State(app): State<AppState>) -> Response {
    static_file(&app, "favicon.svg", "image/svg+xml").await
}

async fn apple_touch_icon(State(app): State<AppState>) -> Response {
    static_file(&app, "apple-touch-icon.png", "image/png").await
}

async fn robots(State(app): State<AppState>) -> Response {
    static_file(&app, "robots.txt", "text/plain; charset=utf-8").await
}

async fn list_templates(
    State(app): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let filter = params.get("filter").map(String::as_str).unwrap_or("");
    Json(app.templates(filter, flag(&params, "animated"))).into_response()
}

async fn template_detail(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    match app.catalog().get(&id) {
        Some(template) => Json(app.template_info(template)).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("Template not found: {id}")),
    }
}

async fn list_examples(
    State(app): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let filter = params.get("filter").map(String::as_str).unwrap_or("");
    Json(app.examples(filter, flag(&params, "animated"))).into_response()
}

async fn list_fonts(State(app): State<AppState>) -> Response {
    let fonts: Vec<_> = app
        .fonts()
        .all()
        .iter()
        .map(|font| app.fonts().info(font, app.base_url()))
        .collect();
    Json(fonts).into_response()
}

async fn font_detail(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    match app.fonts().get(&id).filter(|_| !id.is_empty()) {
        Some(font) => Json(app.fonts().info(font, app.base_url())).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("Font not found: {id}")),
    }
}

/// Request body for creating a meme URL (JSON or form encoded).
#[derive(Default)]
struct Payload {
    template_id: Option<String>,
    text: Vec<String>,
    font: String,
    extension: String,
    animate_text: AnimateText,
    text_boxes: Option<Vec<TextBox>>,
    redirect: bool,
}

fn json_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn parse_payload(headers: &HeaderMap, body: &Bytes) -> Payload {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let mut payload = Payload::default();

    if content_type.starts_with("application/x-www-form-urlencoded") {
        let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body).unwrap_or_default();
        let mut text = Vec::new();
        let mut text_lines = Vec::new();
        for (key, value) in pairs {
            match key.as_str() {
                "template_id" => payload.template_id = Some(value),
                "text" | "text[]" => text.push(value),
                "text_lines" | "text_lines[]" => text_lines.push(value),
                "font" => payload.font = value,
                "extension" => payload.extension = value,
                "animate_text" => payload.animate_text = AnimateText::parse(&value),
                "text_boxes" => payload.text_boxes = serde_json::from_str(&value).ok(),
                "redirect" => payload.redirect = truthy(&value),
                _ => {}
            }
        }
        payload.text = if text.is_empty() { text_lines } else { text };
        return payload;
    }

    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return payload;
    };
    let get = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| map.get(*name).filter(|v| !v.is_null()))
    };
    payload.template_id = map.get("template_id").map(json_string);
    payload.text = match get(&["text", "text[]", "text_lines", "text_lines[]"]) {
        Some(Value::Array(items)) => items.iter().map(json_string).collect(),
        Some(value) => vec![json_string(value)],
        None => vec![],
    };
    payload.font = get(&["font"]).map(json_string).unwrap_or_default();
    payload.extension = get(&["extension"]).map(json_string).unwrap_or_default();
    let boolean = |name: &str| match map.get(name) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => truthy(s),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        _ => false,
    };
    payload.animate_text = match map.get("animate_text") {
        Some(Value::String(s)) => AnimateText::parse(s),
        Some(_) if boolean("animate_text") => AnimateText::Characters,
        _ => AnimateText::Off,
    };
    payload.text_boxes = match get(&["text_boxes"]) {
        Some(Value::String(json)) => serde_json::from_str(json).ok(),
        Some(value) => serde_json::from_value(value.clone()).ok(),
        None => None,
    };
    payload.redirect = boolean("redirect");
    payload
}

fn truthy(value: &str) -> bool {
    matches!(value.to_lowercase().as_str(), "1" | "true" | "yes")
}

fn generate_url(app: &App, template_id: &str, payload: &Payload) -> Response {
    let (url, valid) = app.build_url(
        template_id,
        &payload.text,
        &payload.font,
        &payload.extension,
        payload.animate_text,
        payload.text_boxes.as_deref(),
    );
    if payload.redirect {
        return redirect(StatusCode::FOUND, &url);
    }
    let status = if valid {
        StatusCode::CREATED
    } else {
        StatusCode::NOT_FOUND
    };
    (status, Json(json!({ "url": url }))).into_response()
}

async fn template_build(
    State(app): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    generate_url(&app, &id, &parse_payload(&headers, &body))
}

async fn create_image(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let payload = parse_payload(&headers, &body);
    let Some(template_id) = payload.template_id.as_deref().map(slug::slugify) else {
        return error(StatusCode::BAD_REQUEST, "\"template_id\" is required");
    };
    generate_url(&app, &template_id, &payload)
}

async fn render_response(
    app: &AppState,
    request: MemeRequest,
    range: Option<&HeaderValue>,
) -> Response {
    match app.render(request).await {
        Ok(rendered) => {
            let (mut parts, ()) = Response::new(()).into_parts();
            parts.headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(rendered.content_type),
            );
            parts.headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400"),
            );
            byte_range(range, parts, rendered.bytes.clone())
        }
        Err(render_error) => {
            if matches!(render_error, RenderError::Internal(_)) {
                tracing::error!("{render_error}");
            }
            let status = StatusCode::from_u16(render_error.status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            error(status, render_error.to_string())
        }
    }
}

/// `GET /images/{id}.{ext}`: the template background without text.
async fn blank_image(
    State(app): State<AppState>,
    Path(filename): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let Some((template_id, extension)) = filename.rsplit_once('.') else {
        return error(
            StatusCode::NOT_FOUND,
            format!("Template not found: {filename}"),
        );
    };
    let request = MemeRequest {
        template_id: template_id.to_string(),
        lines: vec![],
        font: params.get("font").cloned().unwrap_or_default(),
        extension: extension.to_string(),
        animate_text: AnimateText::Off,
        custom: Custom::default(),
    };
    render_response(&app, request, headers.get(header::RANGE)).await
}

/// `GET /images/{id}/{line1}/{line2}.{ext}`
async fn meme_image(
    State(app): State<AppState>,
    Path((template_id, text_filepath)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let Some((text_paths, extension)) = split_extension(&text_filepath) else {
        return error(StatusCode::NOT_FOUND, "Missing image extension (e.g. .png)");
    };
    let (normalized, changed) = slug::normalize(text_paths);
    if changed {
        let mut location = format!(
            "/images/{}/{}.{extension}",
            utf8_percent_encode(&template_id, PATH),
            utf8_percent_encode(&normalized, PATH)
        );
        if let Some(query) = query.filter(|q| !q.is_empty()) {
            location.push('?');
            location.push_str(&query);
        }
        return redirect(StatusCode::MOVED_PERMANENTLY, &location);
    }
    match meme_request(template_id, &normalized, extension, &params, None) {
        Ok(request) => render_response(&app, request, headers.get(header::RANGE)).await,
        Err(message) => error(StatusCode::UNPROCESSABLE_ENTITY, message),
    }
}

/// `POST /images/{id}/{line1}/{line2}.{ext}` with an image as the body: the
/// meme drawn on that image instead of the template's (`custom` for the
/// default top and bottom text boxes).
async fn meme_image_on_background(
    State(app): State<AppState>,
    Path((template_id, text_filepath)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some((text_paths, extension)) = split_extension(&text_filepath) else {
        return error(StatusCode::NOT_FOUND, "Missing image extension (e.g. .png)");
    };
    let background = match Background::new(body) {
        Ok(background) => background,
        Err(render_error) => {
            return error(StatusCode::UNPROCESSABLE_ENTITY, render_error.to_string());
        }
    };
    let (normalized, _) = slug::normalize(text_paths);
    match meme_request(
        template_id,
        &normalized,
        extension,
        &params,
        Some(background),
    ) {
        Ok(request) => render_response(&app, request, headers.get(header::RANGE)).await,
        Err(message) => error(StatusCode::UNPROCESSABLE_ENTITY, message),
    }
}

/// `{line1}/{line2}.{ext}` split into the lines and the extension.
fn split_extension(text_filepath: &str) -> Option<(&str, &str)> {
    text_filepath
        .rsplit_once('.')
        .filter(|(_, ext)| !ext.contains('/'))
}

fn meme_request(
    template_id: String,
    normalized: &str,
    extension: &str,
    params: &HashMap<String, String>,
    background: Option<Background>,
) -> Result<MemeRequest, String> {
    let text_boxes = match params.get("text_boxes").filter(|json| !json.is_empty()) {
        Some(json) => Some(
            serde_json::from_str(json)
                .map_err(|parse_error| format!("Invalid text_boxes: {parse_error}"))?,
        ),
        None => None,
    };
    Ok(MemeRequest {
        template_id,
        lines: slug::decode(normalized),
        font: params.get("font").cloned().unwrap_or_default(),
        extension: extension.to_string(),
        animate_text: params
            .get("animate_text")
            .map_or(AnimateText::Off, |value| AnimateText::parse(value)),
        custom: Custom {
            background,
            text_boxes,
        },
    })
}

/// A successful response for `bytes`, or the part a `Range` header asks
/// for. Safari won't play a video from a server without byte ranges. Only a
/// single range is served; anything else gets the whole body, as RFC 9110
/// allows.
pub fn byte_range(
    range: Option<&HeaderValue>,
    mut parts: axum::http::response::Parts,
    bytes: Bytes,
) -> Response {
    let length = bytes.len() as u64;
    let headers = &mut parts.headers;
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.remove(header::TRANSFER_ENCODING);
    let (status, body) = match range.and_then(|range| parse_range(range.to_str().ok()?, length)) {
        None => (StatusCode::OK, bytes),
        Some(None) => {
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{length}")).expect("ASCII"),
            );
            (StatusCode::RANGE_NOT_SATISFIABLE, Bytes::new())
        }
        Some(Some((start, end))) => {
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {start}-{end}/{length}")).expect("ASCII"),
            );
            let slice = bytes.slice(start as usize..=end as usize);
            (StatusCode::PARTIAL_CONTENT, slice)
        }
    };
    parts.status = status;
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    Response::from_parts(parts, body.into())
}

/// The inclusive byte range a single-range `Range` header selects:
/// `None` to ignore the header, `Some(None)` when it can't be satisfied.
fn parse_range(range: &str, length: u64) -> Option<Option<(u64, u64)>> {
    let spec = range.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let (first, last) = (first.trim(), last.trim());
    let selected = if first.is_empty() {
        // The last `suffix` bytes.
        let suffix: u64 = last.parse().ok()?;
        (suffix > 0 && length > 0).then(|| (length.saturating_sub(suffix), length - 1))
    } else {
        let start: u64 = first.parse().ok()?;
        let end = if last.is_empty() {
            u64::MAX
        } else {
            last.parse().ok()?
        };
        if end < start {
            return None;
        }
        (start < length).then(|| (start, end.min(length - 1)))
    };
    Some(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges() {
        assert_eq!(parse_range("bytes=0-1", 10), Some(Some((0, 1))));
        assert_eq!(parse_range("bytes=5-", 10), Some(Some((5, 9))));
        assert_eq!(parse_range("bytes=2-100", 10), Some(Some((2, 9))));
        assert_eq!(parse_range("bytes=-3", 10), Some(Some((7, 9))));
        assert_eq!(parse_range("bytes=-30", 10), Some(Some((0, 9))));
        assert_eq!(parse_range("bytes=10-", 10), Some(None));
        assert_eq!(parse_range("bytes=-0", 10), Some(None));
        assert_eq!(parse_range("bytes=0-1,4-5", 10), None);
        assert_eq!(parse_range("bytes=3-1", 10), None);
        assert_eq!(parse_range("items=0-1", 10), None);
    }

    #[tokio::test]
    async fn serves_byte_ranges() {
        let body = |response: Response| async {
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
        };
        let bytes = Bytes::from_static(b"0123456789");
        let parts = || Response::new(()).into_parts().0;

        let full = byte_range(None, parts(), bytes.clone());
        assert_eq!(full.status(), StatusCode::OK);
        assert_eq!(full.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(full.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(body(full).await, "0123456789");

        let range = HeaderValue::from_static("bytes=2-4");
        let partial = byte_range(Some(&range), parts(), bytes.clone());
        assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(partial.headers()[header::CONTENT_RANGE], "bytes 2-4/10");
        assert_eq!(partial.headers()[header::CONTENT_LENGTH], "3");
        assert_eq!(body(partial).await, "234");

        let range = HeaderValue::from_static("bytes=20-");
        let unsatisfiable = byte_range(Some(&range), parts(), bytes);
        assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(unsatisfiable.headers()[header::CONTENT_RANGE], "bytes */10");
    }

    #[tokio::test]
    async fn draws_on_uploaded_backgrounds_and_in_custom_text_boxes() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        use crate::assets::Directory;
        use crate::config::Config;

        let app = App::load(
            Config::default(),
            Directory::new(env!("CARGO_MANIFEST_DIR")),
        )
        .unwrap();
        let router = router(app);
        let send = |request: Request<Body>| {
            let router = router.clone();
            async move {
                let response = router.oneshot(request).await.unwrap();
                let status = response.status();
                let content_type = response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .map(|value| value.to_str().unwrap().to_string());
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (status, content_type, body)
            }
        };

        let mut png = Vec::new();
        image::RgbaImage::from_pixel(40, 30, image::Rgba([0, 90, 200, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let upload = |uri: &str, body: Vec<u8>| {
            Request::post(uri)
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(body))
                .unwrap()
        };
        let (status, content_type, body) =
            send(upload("/images/custom/hello/world.jpg", png.clone())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("image/jpeg"));
        let image = image::load_from_memory(&body).unwrap();
        assert_eq!((image.width(), image.height()), (800, 600));

        let (status, ..) = send(upload("/images/fry/a/b.png", png)).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, body) =
            send(upload("/images/custom/a.png", b"not an image".to_vec())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(String::from_utf8_lossy(&body).contains("PNG"));
        let get = |uri: &str| Request::get(uri).body(Body::empty()).unwrap();
        let (status, ..) = send(get("/images/custom/a.png")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        // Text boxes in the URL, as `POST /images/` builds it.
        let request = Request::post("/images/")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"template_id": "fry", "text": ["A", "b"], "extension": "png", "text_boxes": [{"anchor_y": 0.4, "style": "none"}, {}]}"#,
            ))
            .unwrap();
        let (status, _, body) = send(request).await;
        assert_eq!(status, StatusCode::CREATED);
        let url: Value = serde_json::from_slice(&body).unwrap();
        let url = url["url"].as_str().unwrap();
        let path = url.strip_prefix("http://localhost:5000").unwrap();
        assert_eq!(
            path,
            "/images/fry/A/b.png?text_boxes=[{\"anchor_y\":0.4,\"style\":\"none\"},{}]"
                .replace('"', "%22")
                .replace('{', "%7B")
                .replace('}', "%7D")
                .replace('[', "%5B")
                .replace(']', "%5D")
        );
        let (status, content_type, _) = send(get(path)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("image/png"));
        let (status, ..) = send(get("/images/fry/a.png?text_boxes=nope")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let (status, ..) = send(get("/images/fry/a.png?text_boxes=[{%22scale_x%22:5}]")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn parses_json_payload() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let body = Bytes::from(
            r#"{"template_id": "fry", "text": ["a", "b"], "animate_text": true, "redirect": true}"#,
        );
        let payload = parse_payload(&headers, &body);
        assert_eq!(payload.template_id.as_deref(), Some("fry"));
        assert_eq!(payload.text, vec!["a", "b"]);
        assert_eq!(payload.animate_text, AnimateText::Characters);
        assert!(payload.redirect);
    }

    #[test]
    fn parses_form_payload() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        let body = Bytes::from("template_id=fry&text%5B%5D=a&text%5B%5D=b&extension=jpg");
        let payload = parse_payload(&headers, &body);
        assert_eq!(payload.text, vec!["a", "b"]);
        assert_eq!(payload.extension, "jpg");
        assert_eq!(payload.animate_text, AnimateText::Off);

        let body = Bytes::from("template_id=fry&animate_text=words");
        let payload = parse_payload(&headers, &body);
        assert_eq!(payload.animate_text, AnimateText::Words);
    }

    #[test]
    fn parses_animate_text() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        for (value, expected) in [
            ("true", AnimateText::Characters),
            ("1", AnimateText::Characters),
            (r#""characters""#, AnimateText::Characters),
            (r#""Words""#, AnimateText::Words),
            ("false", AnimateText::Off),
            (r#""nope""#, AnimateText::Off),
        ] {
            let body = Bytes::from(format!(r#"{{"animate_text": {value}}}"#));
            assert_eq!(
                parse_payload(&headers, &body).animate_text,
                expected,
                "{value}"
            );
        }
    }
}
