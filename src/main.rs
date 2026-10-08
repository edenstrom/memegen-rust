mod bench;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use clap::{Args, Parser, Subcommand};
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;

use memegen::api;
use memegen::app::App;
use memegen::assets::Directory;
use memegen::config::Config;
use memegen::mcp::MemegenMcp;

#[derive(Parser)]
#[command(
    version,
    about = "High-performance memegen.link server with MCP support"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP API with an MCP endpoint at /mcp (default).
    Serve(ServeArgs),
    /// Run an MCP server over stdio.
    Mcp(CommonArgs),
    /// Measure wall-clock and CPU time per render, or load-test a running
    /// server (this one, upstream memegen, or memegen-rs) with --url.
    Bench {
        #[command(flatten)]
        common: CommonArgs,
        /// Iterations per case.
        #[arg(long, default_value_t = 30)]
        iterations: usize,
        /// Base URL of a running memegen-compatible server to load-test over HTTP.
        #[arg(long)]
        url: Option<String>,
        /// Concurrent connections for --url, one run per value.
        #[arg(long, value_delimiter = ',', default_value = "1,64")]
        connections: Vec<usize>,
        /// Seconds per case and connection count for --url.
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
}

#[derive(Args, Clone)]
struct CommonArgs {
    /// Directory containing templates/, fonts/, emoji/, and static/.
    #[arg(long, env = "MEMEGEN_ROOT", global = true)]
    root: Option<PathBuf>,
}

impl CommonArgs {
    fn directory(&self) -> Directory {
        Directory::new(
            self.root
                .clone()
                .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
        )
    }

    fn load(&self, port: u16) -> Result<Arc<App>> {
        App::load(Config::from_env(port), self.directory())
    }
}

#[derive(Args, Clone)]
struct ServeArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Address to bind.
    #[arg(long, env = "HOST", default_value = "127.0.0.1")]
    host: String,
    /// Port to listen on.
    #[arg(long, env = "PORT", default_value_t = 5000)]
    port: u16,
    /// Disable the /mcp endpoint.
    #[arg(long)]
    no_mcp: bool,
    /// Extra Host header values accepted by /mcp (localhost is always allowed).
    #[arg(long = "mcp-allowed-host", value_name = "HOST")]
    mcp_allowed_hosts: Vec<String>,
}

fn init_tracing(default: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default)),
        )
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Mcp(common)) => run_stdio(common).await,
        Some(Command::Serve(args)) => serve(args).await,
        Some(Command::Bench {
            url: Some(url),
            connections,
            seconds,
            ..
        }) => bench::run_http(&url, &connections, Duration::from_secs(seconds)).await,
        Some(Command::Bench {
            common, iterations, ..
        }) => {
            init_tracing("warn");
            let app = common.load(5000)?;
            let directory = common.directory();
            tokio::task::spawn_blocking(move || bench::run(app, &directory, iterations)).await?
        }
        None => serve(cli.serve).await,
    }
}

async fn run_stdio(args: CommonArgs) -> Result<()> {
    init_tracing("warn");
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5000);
    let app = args.load(port)?;
    let service = MemegenMcp::new(app).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

fn is_local_origin(origin: &str) -> bool {
    let host = origin.split("://").nth(1).unwrap_or(origin);
    let host = host.split('/').next().unwrap_or(host);
    let host = if host.starts_with('[') {
        host.split(']').next().map(|h| h.trim_start_matches('['))
    } else {
        host.split(':').next()
    };
    matches!(host, Some("localhost" | "127.0.0.1" | "::1"))
}

/// Reject cross-site browser requests to the unauthenticated MCP endpoint.
async fn guard_origin(request: Request, next: Next) -> Response {
    if let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        && origin != "null"
        && !is_local_origin(origin)
    {
        return (
            StatusCode::FORBIDDEN,
            "Cross-origin MCP requests are not allowed",
        )
            .into_response();
    }
    next.run(request).await
}

async fn serve(args: ServeArgs) -> Result<()> {
    init_tracing("info");
    let app = args.common.load(args.port)?;

    let mut router = api::router(Arc::clone(&app)).layer(CorsLayer::permissive());
    if !args.no_mcp {
        let mut allowed_hosts = vec!["localhost".to_string(), "127.0.0.1".into(), "::1".into()];
        allowed_hosts.extend(args.mcp_allowed_hosts);
        let mcp_app = Arc::clone(&app);
        let service: StreamableHttpService<MemegenMcp, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(MemegenMcp::new(Arc::clone(&mcp_app))),
                Default::default(),
                StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts),
            );
        router = router.nest_service(
            "/mcp",
            tower::ServiceBuilder::new()
                .layer(middleware::from_fn(guard_origin))
                .service(service),
        );
    }

    let address: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(
        "Listening on http://{address} (docs at /docs{})",
        if args.no_mcp { "" } else { ", MCP at /mcp" }
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_origins() {
        assert!(is_local_origin("http://localhost:5000"));
        assert!(is_local_origin("http://127.0.0.1"));
        assert!(is_local_origin("http://[::1]:5000"));
        assert!(!is_local_origin("https://evil.example"));
        assert!(!is_local_origin("http://localhost.evil.example"));
    }
}
