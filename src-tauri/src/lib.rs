//! LLM Wiki backend library.
//!
//! Pure server crate: everything here is consumed by the headless
//! `llm-server` binary (src/bin/llm-server.rs) — the HTTP API
//! (`api_server`), the browser-extension clip server (`clip_server`),
//! the agent runtime (`agent`), and the document/queue/vector engine
//! (`commands`). There is no GUI shell; see `headless` for startup.

mod agent;
mod api_server;
mod project_registry;
mod commands;
mod cors;
pub mod headless;
mod panic_guard;
mod proxy;
mod server_bind;
mod server_host;
#[allow(dead_code, unused_imports)]
mod types;
