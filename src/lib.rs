//! Meme rendering, the HTTP API and the MCP server, shared by the native
//! binary and the Cloudflare Workers build (`worker/`).

pub mod api;
pub mod app;
pub mod assets;
pub mod cache;
pub mod config;
pub mod emoji;
pub mod fonts;
pub mod mcp;
pub mod openapi;
pub mod quantize;
pub mod render;
pub mod settings;
pub mod slug;
pub mod template;
pub mod textbox;
pub mod typeset;
pub mod webp;
