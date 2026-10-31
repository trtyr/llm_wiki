use std::path::PathBuf;
use std::sync::Arc;

use crate::agent;

/// Host-process context for the API and clip servers.
///
/// The desktop app builds it from the Tauri environment (managed state +
/// app-data dir); the headless `llm-server` binary builds it from plain
/// environment configuration. HTTP handlers depend only on this — never
/// on `AppHandle` — so the same server code runs with or without the
/// GUI shell.
#[derive(Clone)]
pub struct ServerHost {
    /// Path to `app-state.json`, the config file shared between the
    /// frontend (tauri-plugin-store) and the Rust servers.
    store_path: Option<PathBuf>,
    sessions: Arc<agent::session::AgentSessionStore>,
    cancellations: Arc<agent::cancel::AgentCancellationRegistry>,
    /// Dedicated async runtime for server work (agent chat turns,
    /// embedding, search) so the HTTP layer never touches the Tauri
    /// event-loop runtime.
    runtime: tokio::runtime::Handle,
}

impl ServerHost {
    pub fn new(
        store_path: Option<PathBuf>,
        sessions: Arc<agent::session::AgentSessionStore>,
        cancellations: Arc<agent::cancel::AgentCancellationRegistry>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            store_path,
            sessions,
            cancellations,
            runtime,
        }
    }

    /// `app-state.json` location, when the host has one.
    pub fn app_state_path(&self) -> Option<PathBuf> {
        self.store_path.clone()
    }

    pub fn sessions(&self) -> &Arc<agent::session::AgentSessionStore> {
        &self.sessions
    }

    pub fn cancellations(&self) -> &Arc<agent::cancel::AgentCancellationRegistry> {
        &self.cancellations
    }

    /// Drive a future to completion on the host's async runtime. Only
    /// legal from non-async threads (the tiny_http handler threads).
    pub fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.runtime.block_on(fut)
    }

    /// Spawn a background task on the host's async runtime.
    pub fn spawn_async<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(fut);
    }
}

/// Build the multi-thread tokio runtime backing a [`ServerHost`].
pub fn build_server_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio server runtime")
}
