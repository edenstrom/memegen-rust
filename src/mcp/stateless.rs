//! Stateless MCP over Streamable HTTP: each POST gets one JSON response and
//! no session is kept. Used on Cloudflare Workers, where rmcp's transports
//! (which spawn Tokio tasks) can't run.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::post;
use rmcp::ServerHandler;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientJsonRpcMessage, ClientRequest, DiscoverResult,
    ErrorCode, ErrorData, JsonRpcMessage, ListToolsResult, ProtocolVersion, ServerJsonRpcMessage,
    ServerResult,
};
use serde::de::DeserializeOwned;

use super::MemegenMcp;
use crate::app::App;

/// Routes for `/mcp`.
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route(
            "/mcp",
            post(handle)
                .get(|| async { StatusCode::METHOD_NOT_ALLOWED })
                .delete(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .with_state(MemegenMcp::new(app))
}

/// Versions negotiated with `initialize` (later revisions dropped it).
fn supported_versions() -> Vec<ProtocolVersion> {
    ProtocolVersion::KNOWN_VERSIONS
        .iter()
        .filter(|version| version.has_initialize())
        .cloned()
        .collect()
}

async fn handle(State(server): State<MemegenMcp>, body: Bytes) -> Response {
    let message: ClientJsonRpcMessage = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(error) => {
            let error = ErrorData::parse_error(error.to_string(), None);
            return (
                StatusCode::BAD_REQUEST,
                Json(ServerJsonRpcMessage::error(error, None)),
            )
                .into_response();
        }
    };
    let JsonRpcMessage::Request(request) = message else {
        // Notifications and responses need no reply.
        return StatusCode::ACCEPTED.into_response();
    };
    let response = match server.handle(request.request).await {
        Ok(result) => ServerJsonRpcMessage::response(result, request.id),
        Err(error) => ServerJsonRpcMessage::error(error, Some(request.id)),
    };
    Json(response).into_response()
}

impl MemegenMcp {
    async fn handle(&self, request: ClientRequest) -> Result<ServerResult, ErrorData> {
        let supported = supported_versions();
        match request {
            ClientRequest::InitializeRequest(request) => {
                let mut info = self.get_info();
                let requested = request.params.protocol_version;
                info.protocol_version = if supported.contains(&requested) {
                    requested
                } else {
                    ProtocolVersion::LATEST_WITH_INITIALIZE
                };
                Ok(ServerResult::InitializeResult(info))
            }
            ClientRequest::DiscoverRequest(_) => Ok(ServerResult::DiscoverResult(
                DiscoverResult::from_server_info(supported, self.get_info()),
            )),
            ClientRequest::PingRequest(_) => Ok(ServerResult::empty(())),
            ClientRequest::ListToolsRequest(_) => Ok(ServerResult::ListToolsResult(
                ListToolsResult::with_all_items(self.tool_router.list_all()),
            )),
            ClientRequest::CallToolRequest(request) => self
                .call(request.params)
                .await
                .map(ServerResult::CallToolResult),
            other => Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!("Method not found: {}", other.method()),
                None,
            )),
        }
    }

    async fn call(&self, params: CallToolRequestParams) -> Result<CallToolResult, ErrorData> {
        fn arguments<T: DeserializeOwned>(
            params: CallToolRequestParams,
        ) -> Result<Parameters<T>, ErrorData> {
            let arguments = serde_json::Value::Object(params.arguments.unwrap_or_default());
            serde_json::from_value(arguments)
                .map(Parameters)
                .map_err(|error| ErrorData::invalid_params(error.to_string(), None))
        }
        match params.name.as_ref() {
            "list_templates" => self.list_templates(arguments(params)?).await,
            "get_template" => self.get_template(arguments(params)?).await,
            "list_fonts" => self.list_fonts().await,
            "generate_meme" => self.generate_meme(arguments(params)?).await,
            name => Err(ErrorData::invalid_params(
                format!("Unknown tool: {name}"),
                None,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;
    use crate::assets::Directory;
    use crate::config::Config;

    fn app() -> Arc<App> {
        App::load(
            Config::default(),
            Directory::new(env!("CARGO_MANIFEST_DIR")),
        )
        .unwrap()
    }

    async fn post(router: &Router, body: Value) -> (StatusCode, Value) {
        let request = Request::post("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    fn request(id: u32, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[tokio::test]
    async fn initializes_and_lists_tools() {
        let router = router(app());
        let (status, body) = post(
            &router,
            request(
                1,
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1" }
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(body["result"]["serverInfo"]["name"], "memegen");

        let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert_eq!(post(&router, notification).await.0, StatusCode::ACCEPTED);

        let (_, body) = post(&router, request(2, "tools/list", json!({}))).await;
        let names: Vec<&str> = body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names.len(),
            4,
            "every listed tool must be dispatched in `call`: {names:?}"
        );
        for name in names {
            let (_, body) = post(
                &router,
                request(3, "tools/call", json!({ "name": name, "arguments": { "id": "fry", "template_id": "fry", "text": ["a"] } })),
            )
            .await;
            assert!(body.get("result").is_some(), "{name}: {body}");
        }
    }

    #[tokio::test]
    async fn generates_memes_and_reports_errors() {
        let router = router(app());
        let (_, body) = post(
            &router,
            request(
                1,
                "tools/call",
                json!({ "name": "generate_meme", "arguments": { "template_id": "ds", "text": ["a", "b", "c"] } }),
            ),
        )
        .await;
        let content = &body["result"]["content"];
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["mimeType"], "image/png");

        let (_, body) = post(
            &router,
            request(2, "tools/call", json!({ "name": "nope", "arguments": {} })),
        )
        .await;
        assert_eq!(body["error"]["code"], -32602);

        let (_, body) = post(&router, request(3, "resources/list", json!({}))).await;
        assert_eq!(body["error"]["code"], -32601);

        let (status, body) = post(&router, json!("not json-rpc")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn pages_through_templates() {
        let router = router(app());
        let mut ids = Vec::new();
        let mut offset = Some(0);
        let mut total = 0;
        while let Some(next) = offset {
            let (_, body) = post(
                &router,
                request(
                    1,
                    "tools/call",
                    json!({ "name": "list_templates", "arguments": { "offset": next } }),
                ),
            )
            .await;
            let text = body["result"]["content"][0]["text"].as_str().unwrap();
            let page: Value = serde_json::from_str(text).unwrap();
            let templates = page["templates"].as_array().unwrap();
            assert!(templates.len() <= 100);
            assert!(text.len() < 50_000, "page is {} bytes", text.len());
            ids.extend(
                templates
                    .iter()
                    .map(|t| t["id"].as_str().unwrap().to_string()),
            );
            total = page["total"].as_u64().unwrap() as usize;
            offset = page["next_offset"].as_u64();
        }
        assert!(total > 100);
        assert_eq!(ids.len(), total);
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));

        let (_, body) = post(
            &router,
            request(
                2,
                "tools/call",
                json!({ "name": "list_templates", "arguments": { "filter": "surprised", "limit": 3 } }),
            ),
        )
        .await;
        let page: Value =
            serde_json::from_str(body["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(page["templates"].as_array().unwrap().len(), 3);
        assert_eq!(page["next_offset"], 3);
    }

    #[tokio::test]
    async fn falls_back_to_a_supported_version() {
        let router = router(app());
        let (_, body) = post(
            &router,
            request(
                1,
                "initialize",
                json!({
                    "protocolVersion": "2099-01-01",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1" }
                }),
            ),
        )
        .await;
        assert_eq!(
            body["result"]["protocolVersion"],
            ProtocolVersion::LATEST_WITH_INITIALIZE.as_str()
        );
    }
}
