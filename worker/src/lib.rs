//! Cloudflare Workers entry point: the HTTP API plus a stateless MCP endpoint
//! at `/mcp`. Template images, fonts and emoji are static assets read through
//! the `ASSETS` binding; the template catalog is embedded at build time.

use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Response, StatusCode, header};
use futures_util::future::join_all;
use memegen::app::App;
use memegen::assets::{Assets, BoxFuture, Files};
use memegen::config::Config;
use memegen::fonts::Fonts;
use memegen::template::Catalog;
use memegen::{api, mcp};
use tower_http::cors::CorsLayer;
use tower_service::Service;
use worker::send::{SendFuture, SendWrapper};
use worker::{Cache, Context, Env, Fetcher, HttpRequest, event};

mod catalog {
    include!(concat!(env!("OUT_DIR"), "/templates.rs"));
}

/// Files served by the static assets binding.
struct StaticAssets(SendWrapper<Fetcher>);

impl StaticAssets {
    async fn fetch(&self, path: String) -> Option<(String, axum::body::Bytes)> {
        let url = format!("https://assets.invalid/{}", path.replace(' ', "%20"));
        let response = self.0.fetch(url, None).await.ok()?;
        if response.status() != StatusCode::OK {
            return None;
        }
        let body = Body::new(response.into_body());
        let bytes = axum::body::to_bytes(body, usize::MAX).await.ok()?;
        Some((path, bytes))
    }
}

impl Assets for StaticAssets {
    fn load(&self, paths: Vec<String>) -> BoxFuture<'_, Files> {
        Box::pin(SendFuture::new(async move {
            let files = join_all(paths.into_iter().map(|path| self.fetch(path))).await;
            files.into_iter().flatten().collect()
        }))
    }
}

static ROUTER: OnceLock<Router> = OnceLock::new();

/// The router for this isolate, built on the first request.
async fn router(env: &Env, request: &HttpRequest) -> worker::Result<Router> {
    if let Some(router) = ROUTER.get() {
        return Ok(router.clone());
    }
    let assets = StaticAssets(SendWrapper::new(env.assets("ASSETS")?));
    let fonts = Fonts::load(&assets.load(Fonts::paths()).await)
        .map_err(|error| worker::Error::RustError(format!("{error:#}")))?;
    let catalog = Catalog::new(catalog::TEMPLATES.iter().map(|(id, config, files)| {
        (
            *id,
            *config,
            files.iter().map(|file| file.to_string()).collect(),
        )
    }));
    // Without `DOMAIN`, URLs in responses point at the host serving this request.
    let uri = request.uri();
    let origin = format!(
        "{}://{}",
        uri.scheme_str().unwrap_or("https"),
        uri.authority()
            .map_or("localhost", |authority| authority.as_str())
    );
    let config = Config::from_vars(|name| env.var(name).ok().map(|var| var.to_string()), origin);

    let app = App::new(config, catalog, fonts, Arc::new(assets));
    let router = api::router(Arc::clone(&app))
        .merge(mcp::stateless::router(app))
        .layer(CorsLayer::permissive());
    Ok(ROUTER.get_or_init(|| router).clone())
}

#[event(fetch)]
async fn fetch(
    mut request: HttpRequest,
    env: Env,
    context: Context,
) -> worker::Result<worker::Response> {
    console_error_panic_hook::set_once();
    let mut router = router(&env, &request).await?;

    // Memes are deterministic, so keep successful renders in Cloudflare's
    // cache to skip re-rendering them in other isolates. The cache holds
    // whole bodies; a `Range` is served from them here.
    let cache_key = (request.method() == Method::GET
        && request.uri().path().starts_with("/images/"))
    .then(|| request.uri().to_string());
    let Some(key) = cache_key else {
        return router.call(request).await?.try_into();
    };
    let range = request.headers_mut().remove(header::RANGE);
    if let Some(cached) = Cache::default().get(key.as_str(), false).await? {
        let response: Response<Body> = cached.into();
        let (parts, bytes) = buffer(response).await?;
        return fixed(api::byte_range(range.as_ref(), parts, bytes)).await;
    }

    let response = router.call(request).await?;
    if response.status() != StatusCode::OK {
        return response.try_into();
    }
    let (parts, bytes) = buffer(response).await?;
    let cached =
        worker::Response::from_bytes(bytes.to_vec())?.with_headers((&parts.headers).into());
    context.wait_until(async move {
        if let Err(error) = Cache::default().put(key, cached).await {
            worker::console_warn!("Caching failed: {error}");
        }
    });
    fixed(api::byte_range(range.as_ref(), parts, bytes)).await
}

/// A response with its body in one piece, so it has a `Content-Length`
/// rather than being streamed in chunks (Safari wants it for video).
async fn fixed(response: Response<Body>) -> worker::Result<worker::Response> {
    let (parts, bytes) = buffer(response).await?;
    Ok(worker::Response::from_bytes(bytes.to_vec())?
        .with_status(parts.status.as_u16())
        .with_headers((&parts.headers).into()))
}

async fn buffer(
    response: Response<Body>,
) -> worker::Result<(axum::http::response::Parts, axum::body::Bytes)> {
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|error| worker::Error::RustError(error.to_string()))?;
    Ok((parts, bytes))
}
