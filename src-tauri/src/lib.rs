//! LLM Wiki backend library.
//!
//! Pure server crate: everything here is consumed by the headless
//! `llm-server` binary (src/bin/llm-server.rs) — the HTTP API
//! (`api_server`) and the agent runtime (`agent`), with the document and
//! vector engine (`commands`),
//! (`commands`). There is no GUI shell; see `headless` for startup.

mod agent;
mod api_docs;
mod api_graph;
mod api_server;
mod api_sse;
mod commands;
mod cors;
pub mod headless;
mod panic_guard;
mod project_registry;
mod proxy;
mod server_bind;
mod server_host;
#[allow(dead_code, unused_imports)]
mod types;
