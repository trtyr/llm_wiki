//! Headless server mode: the LLM Wiki API server without any GUI shell.
//!
//! Configuration is environment-driven so the same binary can be run from
//! a terminal, a service manager, or (later) a public server:
//!
//! - `LLM_WIKI_DATA_DIR`    directory holding `app-state.json`
//!                          (default: the platform app-data dir shared
//!                          with the desktop app, so config just works)
//! - `LLM_WIKI_BIND_HOST`   bind address (default `127.0.0.1`,
//!                          `0.0.0.0` exposes to the network)
//! - `LLM_WIKI_BIND_PORT`   API port (default `19828`)
//! - `LLM_WIKI_PROJECT_PATH` project directory exposed as the current
//!                          project; scaffolded if it does not exist yet
//! - `LLM_WIKI_API_TOKEN`   API token (also read from `app-state.json`)
//!
//! Everything else (LLM keys, embedding, search providers, proxy) is read
//! from `app-state.json` exactly like the desktop app.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::server_bind;
use crate::server_host::{self, ServerHost};
use crate::{api_server, project_registry, proxy};

const APP_IDENTIFIER: &str = "com.llmwiki.app";

/// Entry point for the `llm-server` binary. Blocks until Ctrl-C, then
/// returns the process exit code.
pub fn run_headless_server() -> i32 {
    eprintln!("[llm-server] LLM Wiki headless server v{}", env!("CARGO_PKG_VERSION"));

    let data_dir = resolve_data_dir();
    if let Err(err) = std::fs::create_dir_all(&data_dir) {
        eprintln!(
            "[llm-server] fatal: cannot create data dir {}: {err}",
            data_dir.display()
        );
        return 1;
    }
    let store_path = data_dir.join("app-state.json");
    eprintln!("[llm-server] data dir: {}", data_dir.display());

    // Global HTTP proxy, same behavior as the desktop setup hook.
    match proxy::read_proxy_config_from_store(&store_path) {
        Some(cfg) => {
            let summary = proxy::apply_proxy_env(&cfg);
            eprintln!("[llm-server] [proxy] {summary}");
        }
        None => eprintln!("[llm-server] [proxy] no proxyConfig in store, requests go direct"),
    }

    if let Err(err) = ensure_registered_project() {
        eprintln!("[llm-server] warning: {err}");
    }

    let bind_host = server_bind::configured_bind_host(Some(&store_path));
    let api_port = server_bind::configured_api_port();
    warn_if_exposed(&bind_host, &store_path);

    let runtime = server_host::build_server_runtime();
    let host = ServerHost::new(
        Some(store_path),
        std::sync::Arc::new(crate::agent::session::AgentSessionStore::default()),
        std::sync::Arc::new(crate::agent::cancel::AgentCancellationRegistry::default()),
        runtime.handle().clone(),
    );

    api_server::start_api_server(host);
    eprintln!(
        "[llm-server] API: http://{}{} (health: /health)",
        server_bind::bind_addr(&bind_host, api_port),
        "/api/v1"
    );

    runtime.block_on(async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => eprintln!("[llm-server] Ctrl-C received, shutting down"),
            Err(err) => {
                eprintln!("[llm-server] ctrl_c listener unavailable: {err}");
                // Without a signal listener there is no graceful path;
                // park forever like a daemon so the servers keep running.
                std::future::pending::<()>().await;
            }
        }
    });
    0
}

fn resolve_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LLM_WIKI_DATA_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    default_app_data_dir().unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// Mirror Tauri's `app_data_dir` for [`APP_IDENTIFIER`] so a headless
/// server shares configuration with a desktop installation by default.
fn default_app_data_dir() -> Option<PathBuf> {
    let base = {
        #[cfg(target_os = "windows")]
        {
            std::env::var_os("APPDATA").map(PathBuf::from)
        }
        #[cfg(target_os = "macos")]
        {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join("Library").join("Application Support"))
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            match std::env::var_os("XDG_DATA_HOME") {
                Some(xdg) if !xdg.is_empty() => Some(PathBuf::from(xdg)),
                _ => std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")),
            }
        }
    };
    base.map(|dir| dir.join(APP_IDENTIFIER))
}

/// Register `LLM_WIKI_PROJECT_PATH` (if set) as the current project so
/// API clients can address it as `current` without a desktop app having
/// written a project registry. Bare directories get a minimal scaffold.
fn ensure_registered_project() -> Result<(), String> {
    let raw = std::env::var("LLM_WIKI_PROJECT_PATH").unwrap_or_default();
    let path = raw.trim().to_string();
    if path.is_empty() {
        return Ok(());
    }
    let root = PathBuf::from(&path);
    if !root.exists() {
        std::fs::create_dir_all(&root)
            .map_err(|err| format!("cannot create project dir {}: {err}", root.display()))?;
    }
    let marker = root.join(".llm-wiki");
    if !marker.is_dir() {
        std::fs::create_dir_all(marker)
            .map_err(|err| format!("cannot create .llm-wiki in {}: {err}", root.display()))?;
    }
    let project_json = root.join(".llm-wiki").join("project.json");
    if !project_json.exists() {
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Project")
            .to_string();
        let payload = json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "name": name,
            "createdAt": chrono::Local::now().format("%Y-%m-%d").to_string(),
        });
        std::fs::write(&project_json, serde_json::to_string_pretty(&payload).unwrap_or_default())
            .map_err(|err| format!("cannot write {}: {err}", project_json.display()))?;
        // Minimal wiki layout so agents and clips have somewhere to land.
        let _ = std::fs::create_dir_all(root.join("wiki"));
        let _ = std::fs::create_dir_all(root.join("raw").join("sources"));
        eprintln!("[llm-server] scaffolded new project at {}", root.display());
    }
    project_registry::set_current_project_path(&path);
    project_registry::register_project(
        Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Project"),
        &path,
    );
    eprintln!("[llm-server] project: {path} (exposed as \"current\")");
    Ok(())
}

fn warn_if_exposed(bind_host: &str, store_path: &Path) {
    let loopback = matches!(
        bind_host,
        "127.0.0.1" | "localhost" | "::1" | "[::1]"
    );
    if loopback {
        return;
    }
    let has_token = std::env::var("LLM_WIKI_API_TOKEN")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
        || store_token(store_path).is_some();
    if !has_token {
        eprintln!(
            "[llm-server] WARNING: binding non-loopback host {bind_host:?} with no API token. \
Agent chat can invoke tools (including shell commands) on this machine. \
Set LLM_WIKI_API_TOKEN or apiConfig.token in app-state.json before exposing the server."
        );
    }
}

fn store_token(store_path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(store_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    parsed
        .get("apiConfig")
        .and_then(|config| config.get("token"))
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .map(ToOwned::to_owned)
}
