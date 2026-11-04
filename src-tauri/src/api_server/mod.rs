use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tiny_http::{Header, Method, Response, Server, StatusCode};
use uuid::Uuid;

#[cfg(test)]
use crate::api_graph::{build_graph, graph_aliases, resolve_link, ApiGraphEdge, ApiGraphNode};
use crate::api_graph::{invalidate_graph_cache, load_graph, select_graph_page};
use crate::api_sse::{send_terminal_sse, sse_frame, try_send_sse, SseReader, SseSendResult};
use crate::cors::{local_cors_headers, request_origin};
use crate::server_host::ServerHost;
use crate::{agent, commands, project_registry, server_bind};
#[cfg(test)]
use std::sync::Arc;

mod params;
mod projects;
mod router;

use projects::{
    is_public_project_rel, is_text_content_rel, list_public_roots, list_tree, load_projects,
    normalize_path, read_project_id, resolve_project, safe_join, ReviewQuery, ReviewStatus,
};

mod reviews;

#[cfg(test)]
use reviews::{
    delete_review_item, load_review_items, normalize_review_title, parse_review_query,
    patch_review_item, resolve_review_items, review_id_for_parts, BulkResolveRequest,
};
use reviews::{
    handle_bulk_resolve_reviews, handle_delete_review, handle_patch_review, handle_reviews,
};
mod respond;

#[cfg(test)]
use auth::constant_time_eq;
#[cfg(test)]
use projects::project_path_matches;
use respond::{
    allow_request, err, err_internal, ok, respond_error, respond_json, respond_options,
    should_rate_limit, ApiResponse,
};
#[cfg(test)]
use router::{body_limit_for_request, is_token_required_request, wants_streaming_chat};
use router::{cors_headers, process_request};

pub(crate) use params::parse_query;
use params::{percent_decode, split_url};

mod auth;

use auth::{
    api_allow_lan_access, api_allow_unauthenticated, api_auth_required, api_enabled,
    api_mcp_enabled, api_token, api_token_source, is_authorized, is_token_authorized,
};

const API_PREFIX: &str = "/api/v1";
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_CHAT_BODY_BYTES: usize = 40 * 1024 * 1024;
// JSON may encode one input byte as a six-byte `\u00XX` escape. Keep the
// transport boundary above the authoritative 2 MiB decoded-content limit.
const MAX_PAGE_WRITE_BODY_BYTES: usize = 6 * 2 * 1024 * 1024 + 16 * 1024;
const MAX_FILE_CONTENT_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_MAX_FILES: usize = 2_000;
const HARD_MAX_FILES: usize = 10_000;
const DEFAULT_MAX_REVIEWS: usize = 200;
const HARD_MAX_REVIEWS: usize = 1_000;
const MAX_SEARCH_RESULTS: usize = 50;
const BIND_RETRY_DELAY_SECS: u64 = 2;
const MAX_BIND_RETRIES: u32 = 3;
const APP_STATE_CACHE_TTL: Duration = Duration::from_secs(5);
const MAX_IN_FLIGHT_REQUESTS: usize = 64;
const MAX_IN_FLIGHT_CHAT_STREAMS: usize = 8;
const MAX_IN_FLIGHT_PAGE_EMBEDS: usize = 4;
const SSE_QUEUE_CAPACITY: usize = 64;
const SSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// API status: 0=starting, 1=running, 2=port_conflict, 3=error
static API_STATUS: AtomicU8 = AtomicU8::new(0);
static IN_FLIGHT_REQUESTS: AtomicUsize = AtomicUsize::new(0);
static IN_FLIGHT_CHAT_STREAMS: AtomicUsize = AtomicUsize::new(0);
static IN_FLIGHT_PAGE_EMBEDS: AtomicUsize = AtomicUsize::new(0);
static SSE_DROPPED_EVENTS: AtomicU64 = AtomicU64::new(0);
static SSE_DROPPED_HEARTBEATS: AtomicU64 = AtomicU64::new(0);
static APP_STATE_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedAppState>>> = OnceLock::new();

#[derive(Clone)]
struct CachedAppState {
    loaded_at: Instant,
    value: Option<Value>,
}

pub fn get_api_status() -> &'static str {
    match API_STATUS.load(Ordering::Relaxed) {
        0 => "starting",
        1 => "running",
        2 => "port_conflict",
        _ => "error",
    }
}

/// Drops the app-state config cache so the next request re-reads
/// app-state.json from disk. PUT /settings calls this after every write.
pub fn invalidate_config_cache() {
    if let Some(lock) = APP_STATE_CACHE.get() {
        if let Ok(mut cache) = lock.lock() {
            cache.clear();
        }
    }
}

/// Process-wide serialization for every app-state.json read-modify-write
/// (PUT /settings, project registration, project deletion). The file holds
/// all credentials; concurrent writers with a deterministic tmp name could
/// lose each other's updates while both reporting success (F-25).
fn app_state_write_guard() -> std::sync::MutexGuard<'static, ()> {
    static APP_STATE_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // Poison recovery: a panicking writer must not wedge every future
    // config write; the file itself stays consistent via tmp+rename.
    APP_STATE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Read-modify-write app-state.json under the write lock, persisting via a
/// unique tmp name + rename. `mutate` receives the parsed current state
/// (empty object when the file is missing) and returns false to abort
/// without writing. Error strings are prefixed `corrupt:`/`unreadable:`/
/// `write:` so callers can classify them.
fn update_app_state_file(
    path: &std::path::Path,
    mutate: impl FnOnce(&mut Value) -> bool,
) -> Result<(), String> {
    let _guard = app_state_write_guard();
    let mut merged = match fs::read_to_string(path) {
        Ok(raw) if !raw.trim().is_empty() => match serde_json::from_str::<Value>(&raw) {
            Ok(v) if v.is_object() => v,
            _ => return Err("corrupt: app-state.json is not a JSON object".to_string()),
        },
        Ok(_) => json!({}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("unreadable: {e}")),
    };
    if !mutate(&mut merged) {
        return Ok(());
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = path.with_extension(format!("json.{}-{nanos}.tmp", std::process::id()));
    let payload = serde_json::to_string_pretty(&merged).unwrap_or_default();
    if let Err(e) = fs::write(&tmp, payload) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("write: {e}"));
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("write: rename failed: {e}"));
    }
    invalidate_config_cache();
    Ok(())
}

/// Ensure `<project>/.llm-wiki/project.json` exists with a stable UUID and
/// return that id. The identity file is what keeps a project addressable
/// across restarts, so a failed write must surface as an error instead of
/// handing out an id that turns into a 404 after the next restart (F-17).
fn ensure_project_identity(path: &str, name: &str) -> Result<String, String> {
    if let Some(id) = read_project_id(path) {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let meta_dir = std::path::Path::new(path).join(".llm-wiki");
    fs::create_dir_all(&meta_dir)
        .map_err(|e| format!("failed to create {}: {e}", meta_dir.display()))?;
    let payload = json!({ "id": id, "name": name });
    let file = meta_dir.join("project.json");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = meta_dir.join(format!("project.json.{}-{nanos}.tmp", std::process::id()));
    fs::write(
        &tmp,
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    )
    .map_err(|e| format!("failed to write {}: {e}", file.display()))?;
    if let Err(e) = fs::rename(&tmp, &file) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("failed to finalize {}: {e}", file.display()));
    }
    Ok(id)
}

pub fn start_api_server(app: ServerHost) {
    thread::spawn(move || loop {
        API_STATUS.store(0, Ordering::Relaxed);
        let (server, addr) = match bind_server_with_retry(&app) {
            Some(bound) => bound,
            None => {
                API_STATUS.store(2, Ordering::Relaxed);
                thread::sleep(Duration::from_secs(BIND_RETRY_DELAY_SECS));
                continue;
            }
        };

        API_STATUS.store(1, Ordering::Relaxed);
        eprintln!("[API Server] Listening on http://{addr}{API_PREFIX}");

        for request in server.incoming_requests() {
            let method = request.method().clone();
            let url = request.url().to_string();
            let origin = request_origin(&request);
            if should_rate_limit(&method, &url)
                && !allow_request(request.remote_addr().map(|addr| addr.ip()))
            {
                respond_error(request, 429, "Too many requests", origin.as_deref());
                continue;
            }
            let Some(slot) = try_acquire_request_slot() else {
                respond_error(request, 503, "API server is busy", origin.as_deref());
                continue;
            };
            let app = app.clone();
            thread::spawn(move || {
                let _slot = slot;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    process_request(app, request);
                }));
                if let Err(payload) = result {
                    eprintln!("[API Server] request handler panicked: {payload:?}");
                }
            });
        }

        API_STATUS.store(3, Ordering::Relaxed);
        eprintln!("[API Server] server loop exited; restarting");
        thread::sleep(Duration::from_secs(BIND_RETRY_DELAY_SECS));
    });
}

fn bind_server_with_retry(app: &ServerHost) -> Option<(Server, String)> {
    let host = server_bind::configured_bind_host(app.app_state_path().as_deref());
    let addr = server_bind::bind_addr(&host, server_bind::configured_api_port());
    for attempt in 1..=MAX_BIND_RETRIES {
        match Server::http(&addr) {
            Ok(server) => return Some((server, addr)),
            Err(err) => {
                eprintln!(
                    "[API Server] Failed to bind {addr} (attempt {attempt}/{MAX_BIND_RETRIES}): {err}"
                );
                if attempt < MAX_BIND_RETRIES {
                    thread::sleep(Duration::from_secs(BIND_RETRY_DELAY_SECS));
                }
            }
        }
    }
    None
}

struct RequestSlot;

impl Drop for RequestSlot {
    fn drop(&mut self) {
        IN_FLIGHT_REQUESTS.fetch_sub(1, Ordering::Relaxed);
    }
}

struct ChatStreamSlot;

impl Drop for ChatStreamSlot {
    fn drop(&mut self) {
        IN_FLIGHT_CHAT_STREAMS.fetch_sub(1, Ordering::Relaxed);
    }
}

fn try_acquire_chat_stream_slot() -> Option<ChatStreamSlot> {
    let mut current = IN_FLIGHT_CHAT_STREAMS.load(Ordering::Relaxed);
    loop {
        if current >= MAX_IN_FLIGHT_CHAT_STREAMS {
            return None;
        }
        match IN_FLIGHT_CHAT_STREAMS.compare_exchange_weak(
            current,
            current + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Some(ChatStreamSlot),
            Err(next) => current = next,
        }
    }
}

fn try_acquire_request_slot() -> Option<RequestSlot> {
    let mut current = IN_FLIGHT_REQUESTS.load(Ordering::Relaxed);
    loop {
        if current >= MAX_IN_FLIGHT_REQUESTS {
            return None;
        }
        match IN_FLIGHT_REQUESTS.compare_exchange_weak(
            current,
            current + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Some(RequestSlot),
            Err(next) => current = next,
        }
    }
}

fn load_app_state(app: &ServerHost) -> Option<Value> {
    let now = Instant::now();
    let path = app.app_state_path()?;
    let cache_map = APP_STATE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut previous = None;
    if let Ok(cache) = cache_map.lock() {
        if let Some(cached) = cache.get(&path) {
            if now.duration_since(cached.loaded_at) < APP_STATE_CACHE_TTL {
                return cached.value.clone();
            }
            previous = cached.value.clone();
        }
    }

    let loaded = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    if loaded.is_none() && (previous.is_some() || path.exists()) {
        // A hand-edited or corrupt config used to silently keep serving the
        // stale cached value; say so, or "changed a setting, nothing
        // happened" becomes undebuggable (F-11).
        eprintln!(
            "[api] app-state.json unreadable or corrupt{}; config reads fall back{}",
            if previous.is_some() {
                " - serving previous cached value"
            } else {
                ""
            },
            if previous.is_some() {
                " to cache"
            } else {
                " to empty"
            },
        );
    }
    let value = loaded.or(previous);

    if let Ok(mut cache) = cache_map.lock() {
        cache.insert(
            path.clone(),
            CachedAppState {
                loaded_at: now,
                value: value.clone(),
            },
        );
    }
    value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectEntry {
    id: String,
    name: String,
    path: String,
    current: bool,
}

fn handle_projects(app: &ServerHost) -> ApiResponse {
    let projects = load_projects(app);
    let current_project = projects.iter().find(|project| project.current).cloned();
    ok(json!({
        "ok": true,
        "projects": projects,
        "currentProject": current_project,
    }))
}

fn handle_delete_file(app: &ServerHost, project_id: &str, query: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let params = parse_query(query);
    let Some(rel) = params.get("path") else {
        return err(400, "Missing path query parameter");
    };
    if !is_public_project_rel(rel) {
        return err(403, "Path is not exposed by the local API");
    }
    let path = match safe_join(&project.path, rel) {
        Ok(path) => path,
        Err(e) => return err(400, e),
    };
    if !path.is_file() {
        return err(404, format!("File not found: {rel}"));
    }
    let history_recorded =
        commands::file_history::record_file_version(&path, "api", "api.files.delete");
    if let Err(e) = fs::remove_file(&path) {
        return err_internal("file delete", e);
    }

    let mut vectors_cleared = false;
    let mut vectors_failed = false;
    if let Some(page_id) = commands::vectorstore::canonical_page_id(&normalize_path(rel)) {
        // The LanceDB filter is an exact string match: try the id as
        // written, then the all-lowercase form written by older builds,
        // so pages with uppercase segments cannot leave ghost vectors
        // behind (F-03). Failures are logged with their cause (F-04)
        // instead of collapsing into a bare false.
        let mut candidates = vec![page_id.clone(), page_id.to_lowercase()];
        candidates.dedup();
        for candidate in &candidates {
            match app.block_on(commands::vectorstore::vector_delete_page(
                project.path.clone(),
                candidate.clone(),
            )) {
                Ok(()) => vectors_cleared = true,
                Err(e) => {
                    vectors_failed = true;
                    eprintln!("[api] vector cleanup failed for page '{candidate}': {e}");
                }
            }
        }
    }
    invalidate_graph_cache(&project.path);

    let mut body = json!({
        "ok": true,
        "projectId": project.id,
        "deleted": normalize_path(rel),
        "historyRecorded": history_recorded,
        "vectorsCleared": vectors_cleared,
    });
    if vectors_failed {
        body["vectorsError"] = json!(
            "vector store update failed - deleted page may still appear in search results (see server log)"
        );
    }
    if !history_recorded {
        body["hint"] = json!("file history is disabled for this project; PUT /history/settings {enabled:true} to make deletions restorable");
    }
    ok(body)
}

/// GET /api/v1/projects/{id}/sources/queue - inspect the change queue
/// (previously implemented but unreachable: no route exposed it, F-45).
fn handle_sources_queue(app: &ServerHost, project_id: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    match commands::file_sync::get_file_change_queue(project.path.clone()) {
        Ok(queue) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "queue": queue,
        })),
        Err(e) => err_internal("sources queue", e),
    }
}

/// POST /api/v1/projects/{id}/sources/queue/{taskId}/retry - re-arm a
/// failed task and process it immediately (F-45).
fn handle_sources_queue_retry(app: &ServerHost, project_id: &str, task_id: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    match commands::file_sync::retry_file_change_task(
        project.id.clone(),
        project.path.clone(),
        task_id.to_string(),
    ) {
        Ok(queue) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "queue": queue,
        })),
        Err(e) => err_internal("sources queue retry", e),
    }
}

/// DELETE /api/v1/projects/{id}/sources/queue/{taskId} - drop a failed
/// task from the queue so it stops accumulating (F-45).
fn handle_sources_queue_ignore(app: &ServerHost, project_id: &str, task_id: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    match commands::file_sync::ignore_file_change_task(
        project.id.clone(),
        project.path.clone(),
        task_id.to_string(),
    ) {
        Ok(queue) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "queue": queue,
        })),
        Err(e) => err_internal("sources queue ignore", e),
    }
}

fn handle_files(app: &ServerHost, project_id: &str, query: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let params = parse_query(query);
    let root = params.get("root").map(String::as_str).unwrap_or("wiki");
    let recursive = params
        .get("recursive")
        .map(|v| v != "false")
        .unwrap_or(true);
    let max_files = params
        .get("maxFiles")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_FILES)
        .clamp(1, HARD_MAX_FILES);
    let rel = match root {
        "wiki" => "wiki",
        "sources" | "raw" | "raw/sources" => "raw/sources",
        "all" | "" => "",
        _ => return err(400, "root must be wiki, sources, or all"),
    };
    if rel.is_empty() {
        let mut count = 0;
        return match list_public_roots(&project.path, recursive, max_files, &mut count) {
            Ok(files) => ok(json!({
                "ok": true,
                "projectId": project.id,
                "root": "all",
                "files": files,
                "truncated": count > max_files,
            })),
            Err(e) => err_internal("files list", e),
        };
    }
    let dir = match safe_join(&project.path, rel) {
        Ok(path) => path,
        Err(e) => return err(400, e),
    };
    let mut count = 0;
    match list_tree(&project.path, &dir, recursive, max_files, &mut count) {
        Ok(files) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "root": rel,
            "files": files,
            "truncated": count > max_files,
        })),
        Err(e) => err_internal("files list", e),
    }
}

fn handle_file_content(app: &ServerHost, project_id: &str, query: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let params = parse_query(query);
    let Some(rel) = params.get("path") else {
        return err(400, "Missing path query parameter");
    };
    if !is_public_project_rel(rel) {
        return err(403, "Path is not exposed by the local API");
    }
    if !is_text_content_rel(rel) {
        return err(
            415,
            "Only text-like project files can be read via this endpoint",
        );
    }
    let path = match safe_join(&project.path, rel) {
        Ok(path) => path,
        Err(e) => return err(400, e),
    };
    let meta = match fs::metadata(&path) {
        Ok(meta) => meta,
        Err(e) => return err(404, format!("File not found: {e}")),
    };
    if meta.len() > MAX_FILE_CONTENT_BYTES {
        return err(413, "File is too large to return via API");
    }
    match fs::read_to_string(&path) {
        Ok(content) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "path": rel,
            "content": content,
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            err(404, format!("File not found: {}", rel))
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            err(403, format!("File is not readable: {}", e))
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            err(415, "File is not valid UTF-8 text")
        }
        Err(e) => err_internal("file read", e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchRequest {
    query: String,
    top_k: Option<usize>,
    include_content: Option<bool>,
    query_embedding: Option<Vec<f32>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmbedPageRequest {
    path: String,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WritePageRequest {
    path: String,
    content: String,
    #[serde(default)]
    allow_overwrite: bool,
}

fn handle_write_page(app: &ServerHost, project_id: &str, body: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let req: WritePageRequest = match serde_json::from_str(body) {
        Ok(req) => req,
        Err(e) => return err(400, format!("Invalid JSON: {e}")),
    };
    if req.path.trim().is_empty() {
        return err(400, "path is required");
    }
    match agent::tools::write_wiki_page_verified(
        &project.path,
        &req.path,
        &req.content,
        req.allow_overwrite,
    ) {
        Ok(result) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "result": result,
        })),
        Err(error) => {
            let status = agent::tools::page_write_error_status(&error);
            err(status, error)
        }
    }
}

struct PageEmbedSlot;

impl Drop for PageEmbedSlot {
    fn drop(&mut self) {
        IN_FLIGHT_PAGE_EMBEDS.fetch_sub(1, Ordering::Relaxed);
    }
}

fn try_acquire_page_embed_slot() -> Option<PageEmbedSlot> {
    IN_FLIGHT_PAGE_EMBEDS
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current < MAX_IN_FLIGHT_PAGE_EMBEDS).then_some(current + 1)
        })
        .ok()
        .map(|_| PageEmbedSlot)
}

fn handle_embed_page(app: &ServerHost, project_id: &str, body: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let req: EmbedPageRequest = match serde_json::from_str(body) {
        Ok(req) => req,
        Err(e) => return err(400, format!("Invalid JSON: {e}")),
    };
    if req.path.trim().is_empty() {
        return err(400, "path is required");
    }
    let Some(config) = load_embedding_config(app) else {
        return err(400, "Embedding is not configured");
    };
    let Some(_slot) = try_acquire_page_embed_slot() else {
        return err(503, "Too many page indexing requests are already running");
    };
    let result = app.block_on(commands::page_embedding::embed_wiki_page(
        &project.path,
        &req.path,
        config,
        req.force,
    ));
    match result {
        Ok(result) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "result": result,
        })),
        Err(error) => {
            let status = match error.kind {
                commands::page_embedding::PageEmbeddingErrorKind::InvalidRequest => 400,
                commands::page_embedding::PageEmbeddingErrorKind::NotFound => 404,
                commands::page_embedding::PageEmbeddingErrorKind::Provider => 502,
                commands::page_embedding::PageEmbeddingErrorKind::Storage => 500,
                commands::page_embedding::PageEmbeddingErrorKind::Timeout => 504,
            };
            err(status, error.message)
        }
    }
}

fn handle_search(app: &ServerHost, project_id: &str, body: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let req: SearchRequest = match serde_json::from_str(body) {
        Ok(req) => req,
        Err(e) => return err(400, format!("Invalid JSON: {e}")),
    };
    if req.query.trim().is_empty() {
        return err(400, "query is required");
    }
    let top_k = req.top_k.unwrap_or(10).clamp(1, MAX_SEARCH_RESULTS);
    let query = req.query;
    // Embedding is optional by design: if resolution fails (provider
    // misconfig, upstream outage) degrade to keyword-only search instead
    // of failing the whole request — the failure is server-side, not the
    // client's fault. The degradation is surfaced to the client via the
    // response's `warnings` array; the provider error detail stays in the
    // server log.
    let mut warnings: Vec<String> = Vec::new();
    let query_embedding = match app.block_on(commands::search::resolve_query_embedding(
        &query,
        req.query_embedding,
        load_embedding_config(app),
    )) {
        Ok(embedding) => embedding,
        Err(e) => {
            eprintln!("[api] query embedding unavailable, falling back to keyword-only: {e}");
            warnings.push("embedding provider unavailable — results are keyword-only".to_string());
            None
        }
    };
    match app.block_on(commands::search::search_project_inner(
        project.path.clone(),
        query,
        top_k,
        req.include_content.unwrap_or(false),
        query_embedding,
    )) {
        Ok(search) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "mode": search.mode,
            "note": "Search uses the shared backend hybrid retrieval service, combining keyword, vector, and one-hop knowledge-graph candidates. When embeddingConfig is enabled, the API automatically includes LanceDB vector results; clients may also pass queryEmbedding explicitly.",
            "tokenHits": search.token_hits,
            "vectorHits": search.vector_hits,
            "graphHits": search.graph_hits,
            "warnings": warnings,
            "results": search.results,
        })),
        Err(e) => err_internal("handler error", e),
    }
}

struct PreparedChat {
    project: ProjectEntry,
    request: agent::AgentChatRequest,
    runtime: agent::AgentRuntime,
    user_message: String,
    persist_session: bool,
    session_id: String,
    run_id: String,
    cancellation: Option<agent::cancel::AgentCancellationToken>,
}

fn prepare_chat(
    app: &ServerHost,
    project_id: &str,
    body: &str,
) -> Result<PreparedChat, ApiResponse> {
    let project = resolve_project(app, project_id).map_err(|e| err(404, e))?;
    let mut req: agent::AgentChatRequest =
        serde_json::from_str(body).map_err(|e| err(400, format!("Invalid JSON: {e}")))?;
    // This is an internal desktop preflight escape hatch for CLI-backed chat.
    // The public API must retain its actionable generator-configuration error.
    req.allow_empty_retrieval = false;
    if req.message.trim().is_empty() {
        return Err(err(400, "message is required"));
    }
    if req
        .session_id
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        req.session_id = Some(format!("api_{}", Uuid::new_v4()));
    }
    if req
        .run_id
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        req.run_id = Some(format!("run_{}", Uuid::new_v4()));
    }
    let requested_session_id = req.session_id.clone();
    if let Some(session_id) = requested_session_id.as_deref() {
        if req.history.is_empty() && !req.history_explicit {
            req.history = app
                .sessions()
                .recent_messages(&project.path, session_id, 12)
                .into_iter()
                .map(|message| agent::types::AgentConversationMessage {
                    role: message.role,
                    content: message.content,
                })
                .collect();
        }
    }
    let runtime_config = load_agent_runtime_config(app, Some(&project.id));
    let runtime = agent::AgentRuntime::new(
        project.id.clone(),
        project.path.clone(),
        runtime_config.embedding,
        runtime_config.llm,
        runtime_config.web_search,
        runtime_config.anytxt,
    );
    let user_message_for_session = req.message.clone();
    let persist_session = req.persist_session;
    let session_id = req.session_id.clone().unwrap_or_default();
    let run_id = req.run_id.clone().unwrap_or_default();
    let cancellation = app.cancellations().start(&project.id, &session_id, &run_id);
    Ok(PreparedChat {
        project,
        request: req,
        runtime,
        user_message: user_message_for_session,
        persist_session,
        session_id,
        run_id,
        cancellation: Some(cancellation),
    })
}

fn persist_chat_response(
    app: &ServerHost,
    prepared: &PreparedChat,
    response: &agent::types::AgentChatResponse,
) -> bool {
    if !prepared.persist_session {
        return true; // nothing requested, nothing to fail
    }
    app.sessions().append_turn(
        &prepared.project.path,
        &prepared.project.id,
        &response.session_id,
        &prepared.user_message,
        &response.message,
    )
}

fn external_chat_response(mut response: agent::types::AgentChatResponse) -> Value {
    for event in &mut response.events {
        event.redact_for_external_api();
    }
    json!({
        "ok": true,
        "projectId": response.project_id,
        "sessionId": response.session_id,
        "mode": response.mode,
        "message": {
            "role": "assistant",
            "content": response.message,
        },
        "references": response.references,
        "toolEvents": response.tool_events,
        "events": response.events,
        "usage": response.usage,
    })
}

/// Maps an agent-loop error string to an HTTP status: client-side request
/// problems are 400, cancellations 499, upstream/provider failures 502.
/// The agent runtime reports errors as free-form strings, so this is
/// necessarily a heuristic — co-located here with the known producer
/// messages from the runtime.
// Heuristic classifier over agent error strings. Producers and their
// intended codes:
// - 499  client cancel ("Agent turn cancelled")
// - 400  caller-fixable request problems: tool argument validators
//        ("requires query/path/an active skill", "unknown agent tool",
//        "unsupported") and model-produced invalid tool plans
//        ("invalid request", "invalid tool")
// - 503  THIS service is not ready: LLM not configured (runtime errors
//        starting "Backend Agent LLM is not configured") - retrying
//        without reconfiguring is pointless, so never 502 (F-28)
// - 502  upstream provider failures (HTTP errors, non-JSON bodies);
//        provider-local config damage ("Invalid API key header") is a
//        deployment problem, not the caller's, so 503 rather than the
//        old blanket 400 (F-42)
// - 500  anything unrecognized
fn chat_error_status(error: &str) -> u16 {
    if error == "Agent turn cancelled" {
        return 499;
    }
    let lower = error.to_ascii_lowercase();
    if lower.starts_with("backend agent llm is not configured")
        || lower.contains("invalid api key header")
        || lower.contains("invalid authorization header")
    {
        return 503;
    }
    if lower.contains("requires query")
        || lower.contains("requires path")
        || lower.contains("requires an active skill")
        || lower.contains("unsupported")
        || lower.starts_with("unknown agent tool")
        || lower.starts_with("invalid request")
        || lower.starts_with("invalid tool")
    {
        return 400;
    }
    if lower.contains("invalid llm json") || lower.contains("llm http ") {
        return 502;
    }
    502
}

fn handle_chat(app: &ServerHost, project_id: &str, body: &str) -> ApiResponse {
    let mut prepared = match prepare_chat(app, project_id, body) {
        Ok(prepared) => prepared,
        Err(response) => return response,
    };
    let result = app.block_on(prepared.runtime.run_once_with_cancel(
        std::mem::take(&mut prepared.request),
        prepared.cancellation.take(),
    ));
    app.cancellations()
        .finish(&prepared.project.id, &prepared.session_id, &prepared.run_id);
    match result {
        Ok(response) => {
            let persisted = persist_chat_response(app, &prepared, &response);
            let mut body = external_chat_response(response);
            body["persisted"] = json!(persisted);
            ok(body)
        }
        Err(e) => err(chat_error_status(&e), e),
    }
}

fn respond_chat_sse(
    request: tiny_http::Request,
    app: ServerHost,
    project_id: &str,
    body: &str,
    origin: Option<&str>,
    _stream_slot: ChatStreamSlot,
) {
    let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        prepare_chat(&app, project_id, body)
    }));
    let mut prepared = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(response)) => {
            respond_json(request, response.status, response.body, origin);
            return;
        }
        Err(_) => {
            respond_error(request, 500, "Internal API server error", origin);
            return;
        }
    };
    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(SSE_QUEUE_CAPACITY);
    // The meta frame carries sessionId/runId; a dropped meta frame means
    // the client cannot address cancel at all, so it is never silent (F-43).
    match try_send_sse(
        &sender,
        sse_frame(
            "meta",
            &json!({
                "projectId": prepared.project.id,
                "sessionId": prepared.session_id,
                "runId": prepared.run_id,
            }),
        ),
    ) {
        SseSendResult::Sent => {}
        SseSendResult::Full => eprintln!(
            "[api] SSE meta frame dropped (queue full) for run {}",
            prepared.run_id
        ),
        SseSendResult::Disconnected => eprintln!(
            "[api] SSE meta frame lost (client gone) for run {}",
            prepared.run_id
        ),
    }

    let (heartbeat_stop, heartbeat_done) = mpsc::channel::<()>();
    let heartbeat_sender = sender.clone();
    let heartbeat_registry = app.cancellations().clone();
    let heartbeat_project = prepared.project.id.clone();
    let heartbeat_session = prepared.session_id.clone();
    let heartbeat_run = prepared.run_id.clone();
    thread::spawn(move || loop {
        match heartbeat_done.recv_timeout(SSE_HEARTBEAT_INTERVAL) {
            Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                match try_send_sse(&heartbeat_sender, b": keepalive\n\n".to_vec()) {
                    SseSendResult::Disconnected => {
                        heartbeat_registry.cancel(
                            &heartbeat_project,
                            &heartbeat_session,
                            Some(&heartbeat_run),
                        );
                        break;
                    }
                    SseSendResult::Sent => {}
                    SseSendResult::Full => {
                        SSE_DROPPED_HEARTBEATS.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    });

    let task_app = app.clone();
    app.spawn_async(async move {
        let event_sender = sender.clone();
        let registry = task_app.cancellations().clone();
        let cancel_project = prepared.project.id.clone();
        let cancel_session = prepared.session_id.clone();
        let cancel_run = prepared.run_id.clone();
        let sink: agent::runtime::AgentEventSink = std::sync::Arc::new(move |mut event| {
            event.redact_for_external_api();
            let payload = serde_json::to_value(event)
                .unwrap_or_else(|err| json!({ "type": "error", "message": err.to_string() }));
            match try_send_sse(&event_sender, sse_frame("agent", &payload)) {
                SseSendResult::Disconnected => {
                    registry.cancel(&cancel_project, &cancel_session, Some(&cancel_run));
                }
                SseSendResult::Sent => {}
                // Intermediate events may be dropped for a temporarily slow
                // client. The terminal `done` frame still carries the complete
                // aggregate response, so correctness is preserved - but the
                // gap is now counted and logged instead of invisible (F-43).
                SseSendResult::Full => {
                    let dropped = SSE_DROPPED_EVENTS.fetch_add(1, Ordering::Relaxed) + 1;
                    eprintln!(
                        "[api] SSE queue full; dropped agent event #{dropped} (run {cancel_run})"
                    );
                }
            }
        });
        let result =
            std::panic::AssertUnwindSafe(prepared.runtime.run_once_with_cancel_and_events(
                std::mem::take(&mut prepared.request),
                prepared.cancellation.take(),
                Some(sink),
            ))
            .catch_unwind()
            .await;
        task_app.cancellations().finish(
            &prepared.project.id,
            &prepared.session_id,
            &prepared.run_id,
        );
        match result {
            Ok(Ok(response)) => {
                if !persist_chat_response(&task_app, &prepared, &response) {
                    eprintln!(
                        "[api] chat session {} failed to persist (disk?)",
                        response.session_id
                    );
                }
                send_terminal_sse(
                    sender.clone(),
                    sse_frame("done", &external_chat_response(response)),
                );
            }
            Ok(Err(message)) => {
                let event = if message == "Agent turn cancelled" {
                    "cancelled"
                } else {
                    "error"
                };
                send_terminal_sse(
                    sender.clone(),
                    sse_frame(event, &json!({ "ok": false, "error": message })),
                );
            }
            Err(_) => {
                send_terminal_sse(
                    sender.clone(),
                    sse_frame(
                        "error",
                        &json!({ "ok": false, "error": "Agent chat task panicked" }),
                    ),
                );
            }
        }
        let _ = heartbeat_stop.send(());
    });

    let mut response = Response::new(
        StatusCode(200),
        vec![
            Header::from_bytes("Content-Type", "text/event-stream; charset=utf-8").unwrap(),
            Header::from_bytes("Cache-Control", "no-cache, no-transform").unwrap(),
            Header::from_bytes("X-Accel-Buffering", "no").unwrap(),
        ],
        SseReader::new(receiver),
        None,
        None,
    );
    for header in cors_headers(origin, crate::cors::requests_private_network(&request)) {
        response.add_header(header);
    }
    let _ = request.respond(response);
}

fn handle_cancel_chat(app: &ServerHost, project_id: &str, session_id: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    ok(json!({
        "ok": true,
        "cancelled": app
            .cancellations()
            .cancel(&project.id, session_id, None),
        "sessionId": session_id,
    }))
}

/// GET /api/v1/projects/{id}/chat/sessions
///
/// Lists the agent chat sessions persisted under the project, newest first
/// (by `updatedAt`). `?limit=N` (default 100, max 500) caps the response.
fn handle_list_chat_sessions(app: &ServerHost, project_id: &str, query: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let params = parse_query(query);
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    let mut sessions = app.sessions().list_sessions(&project.path);
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions.truncate(limit);
    ok(json!({
        "ok": true,
        "projectId": project.id,
        "sessions": sessions,
    }))
}

/// GET /api/v1/projects/{id}/chat/sessions/{sessionId}?limit=N
///
/// Returns the last N (default 40, max 200) messages of one chat session,
/// oldest first.
fn handle_get_chat_session(
    app: &ServerHost,
    project_id: &str,
    session_id: &str,
    query: &str,
) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let params = parse_query(query);
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(40)
        .clamp(1, 200);
    let session_id = percent_decode(session_id);
    if session_id.trim().is_empty() {
        return err(400, "sessionId is required");
    }
    let messages = app
        .sessions()
        .recent_messages(&project.path, &session_id, limit);
    ok(json!({
        "ok": true,
        "projectId": project.id,
        "sessionId": session_id,
        "messages": messages,
    }))
}

fn load_embedding_config(app: &ServerHost) -> Option<commands::search::SearchEmbeddingConfig> {
    let parsed = load_app_state(app)?;
    let value = parsed.get("embeddingConfig")?.clone();
    serde_json::from_value::<commands::search::SearchEmbeddingConfig>(value).ok()
}

#[derive(Debug, Clone, Default)]
struct AgentRuntimeConfig {
    embedding: Option<commands::search::SearchEmbeddingConfig>,
    llm: Option<agent::provider::LlmConfig>,
    web_search: Option<agent::tools::WebSearchConfig>,
    anytxt: Option<agent::tools::AnyTxtConfig>,
}

fn project_llm_config(parsed: &Value, project_id: &str) -> Option<agent::provider::LlmConfig> {
    let global = parsed.get("llmConfig").cloned();
    let Some(project) = parsed
        .get("projectLlmOverrides")
        .and_then(|value| value.get(project_id))
    else {
        return global.and_then(|value| serde_json::from_value(value).ok());
    };
    if project.get("enabled").and_then(Value::as_bool) != Some(true) {
        return global.and_then(|value| serde_json::from_value(value).ok());
    }

    // The frontend persists a resolved, non-secret profile. Merge the current
    // provider override here so credential rotation and endpoint edits apply to
    // native API calls without duplicating API keys in every project record.
    let mut profile = project.get("profile")?.clone();
    let profile_object = profile.as_object_mut()?;
    let preset_id = project.get("presetId").and_then(Value::as_str)?;
    let provider = parsed
        .get("providerConfigs")
        .and_then(|value| value.get(preset_id))
        .and_then(Value::as_object);
    // `customLlmPresets` is the authoritative registry. A stale providerConfigs
    // entry may survive an interrupted multi-key settings save, so credentials
    // alone must not resurrect a deleted custom preset.
    let custom_preset_exists = parsed
        .get("customLlmPresets")
        .and_then(Value::as_array)
        .map(|presets| {
            presets
                .iter()
                .any(|preset| preset.get("id").and_then(Value::as_str) == Some(preset_id))
        })
        .unwrap_or(false);
    // Match resolveProjectLlmConfig in src/lib/llm-task-routing.ts: deleting a
    // custom preset must make every project that referenced it fall back to
    // the global config, including projects that are not currently open.
    if preset_id.starts_with("custom-") && !custom_preset_exists {
        return global.and_then(|value| serde_json::from_value(value).ok());
    }
    if let Some(provider) = provider {
        for key in [
            "apiKey",
            "apiMode",
            "azureApiVersion",
            "maxContextSize",
            "reasoning",
            "streamingEnabled",
            "customHeaders",
            "extraRequestBody",
        ] {
            if let Some(value) = provider.get(key) {
                profile_object.insert(key.to_string(), value.clone());
            }
        }
        if project
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            if let Some(value) = provider.get("model") {
                profile_object.insert("model".to_string(), value.clone());
            }
        }
        if let Some(value) = provider.get("baseUrl") {
            let endpoint_key =
                if profile_object.get("provider").and_then(Value::as_str) == Some("ollama") {
                    "ollamaUrl"
                } else {
                    "customEndpoint"
                };
            profile_object.insert(endpoint_key.to_string(), value.clone());
        }
    }
    serde_json::from_value(profile).ok()
}

fn load_agent_runtime_config(app: &ServerHost, project_id: Option<&str>) -> AgentRuntimeConfig {
    let Some(parsed) = load_app_state(app) else {
        return AgentRuntimeConfig::default();
    };
    AgentRuntimeConfig {
        embedding: parsed
            .get("embeddingConfig")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok()),
        llm: project_id
            .and_then(|id| project_llm_config(&parsed, id))
            .or_else(|| {
                parsed
                    .get("llmConfig")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
            }),
        web_search: parsed
            .get("searchApiConfig")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok()),
        anytxt: parsed
            .get("searchApiConfig")
            .and_then(|value| value.get("anyTxt"))
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok()),
    }
}

fn handle_graph(app: &ServerHost, project_id: &str, query: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let params = parse_query(query);
    let q = params.get("q").map(|s| s.to_lowercase());
    let node_type = params.get("nodeType").map(|s| s.to_lowercase());
    let paged_edges = match params.get("edgeScope").map(String::as_str) {
        Some("filtered") => true,
        Some("page") | None => false,
        Some(_) => return err(400, "edgeScope must be 'page' or 'filtered'"),
    };
    let offset = match params.get("offset") {
        Some(value) => match value.parse::<usize>() {
            Ok(value) => value,
            Err(_) => return err(400, "offset must be a non-negative integer"),
        },
        None => 0,
    };
    let limit = match params.get("limit") {
        Some(value) => match value.parse::<usize>() {
            Ok(value) if value > 0 => value.clamp(1, 1000),
            _ => return err(400, "limit must be a positive integer"),
        },
        None => 200,
    };

    match load_graph(&project.path) {
        Ok(graph) => {
            let page = select_graph_page(
                &graph.nodes,
                &graph.edges,
                q.as_deref(),
                node_type.as_deref(),
                offset,
                limit,
                paged_edges,
            );
            ok(json!({
                "ok": true,
                "projectId": project.id,
                "nodes": page.nodes,
                "edges": page.edges,
                "offset": offset,
                "limit": limit,
                "totalCount": page.total_count,
                "hasMore": page.has_more,
            }))
        }
        Err(e) => err_internal("handler error", e),
    }
}

fn handle_rescan(app: &ServerHost, project_id: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    invalidate_graph_cache(&project.path);
    let source_watch_config = load_source_watch_config(app, &project.id);
    match commands::file_sync::rescan_project_files_detached(
        project.id.clone(),
        project.path.clone(),
        source_watch_config,
        None,
    ) {
        Ok(result) => ok(json!({ "ok": true, "projectId": project.id, "result": result })),
        Err(e) => err_internal("handler error", e),
    }
}

fn load_source_watch_config(
    app: &ServerHost,
    project_id: &str,
) -> Option<commands::file_sync::SourceWatchConfig> {
    let parsed = load_app_state(app)?;
    let settings = parsed.get("sourceWatchConfig").and_then(Value::as_object);
    if let Some(value) = settings
        .and_then(|s| s.get(project_id).or_else(|| s.get("default")))
        .cloned()
    {
        if let Ok(config) = serde_json::from_value::<commands::file_sync::SourceWatchConfig>(value)
        {
            return Some(config);
        }
    }
    let legacy_enabled = parsed
        .get("projectFileSyncEnabled")
        .and_then(Value::as_object)
        .and_then(|settings| {
            settings
                .get(project_id)
                .or_else(|| settings.get("default"))
                .and_then(Value::as_bool)
        });
    legacy_enabled.and_then(|enabled| {
        serde_json::from_value::<commands::file_sync::SourceWatchConfig>(
            json!({ "enabled": enabled }),
        )
        .ok()
    })
}

/// Persist a project registration into app-state.json `projectRegistry`
/// so API-created projects survive a server restart (the in-memory
/// project_registry is process-local by design).
fn persist_project_registration(app: &ServerHost, id: &str, name: &str, path: &str) -> bool {
    let Some(store) = app.app_state_path() else {
        return false;
    };
    // Read-modify-write under the app-state lock (F-25); the outcome is
    // reported so a silent "registered in memory only" project cannot be
    // returned as ok:true (F-23).
    let result = update_app_state_file(&store, |merged| {
        if let Some(map) = merged.as_object_mut() {
            let registry = map
                .entry("projectRegistry".to_string())
                .or_insert_with(|| json!({}));
            if let Some(reg) = registry.as_object_mut() {
                reg.insert(
                    id.to_string(),
                    json!({ "name": name, "path": path.replace('\\', "/") }),
                );
            }
        }
        true
    });
    match result {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[api] project registration for {id} failed to persist: {e}");
            false
        }
    }
}

const OPENAPI_JSON: &str = include_str!("../../openapi.json");

fn openapi_document() -> Value {
    static PARSED: OnceLock<Value> = OnceLock::new();
    PARSED
        .get_or_init(|| match serde_json::from_str(OPENAPI_JSON) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "[api] embedded openapi.json failed to parse: {e} - /openapi.json and /docs will be empty"
                );
                Value::Null
            }
        })
        .clone()
}

/// GET /api/v1/settings — returns the merged app-state configuration
/// (llmConfig, embeddingConfig, searchApiConfig, apiConfig, ...). Values
/// under credential-bearing keys are masked as `********`; PUTting the
/// mask back keeps the existing value (round-trip safe, see
/// deep_merge_masked). The bearer token gates the endpoint.
fn handle_get_settings(app: &ServerHost) -> ApiResponse {
    let Some(path) = app.app_state_path() else {
        return err(404, "No app-state file in this deployment");
    };
    match fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(mut value) => {
                mask_secrets(&mut value);
                ok(value)
            }
            Err(e) => err(500, format!("app-state.json is not valid JSON: {e}")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ok(json!({})),
        Err(e) => err_internal("settings GET: read failed", e),
    }
}

/// PUT /api/v1/settings — merges top-level config sections (llmConfig,
/// embeddingConfig, searchApiConfig, apiConfig, ...) into app-state.json.
/// Unknown keys are preserved; the config cache is invalidated immediately.
fn handle_put_settings(app: &ServerHost, body: &str) -> ApiResponse {
    let Some(path) = app.app_state_path() else {
        return err(404, "No app-state file in this deployment");
    };
    let update: Value = match serde_json::from_str::<Value>(body) {
        Ok(v) if v.is_object() => v,
        _ => return err(400, "Body must be a JSON object of config sections"),
    };
    // Read-modify-write runs under the app-state write lock (F-25): a
    // torn or lost update here drops the API token and every provider key.
    // A missing file is a legitimate first run (empty base); anything
    // else — IO failure, corrupt JSON — must REFUSE the write instead of
    // silently treating the config as empty and wiping token + keys.
    let mut merged_out: Option<Value> = None;
    let result = update_app_state_file(&path, |merged| {
        deep_merge_masked(merged, &update);
        merged_out = Some(merged.clone());
        true
    });
    match result {
        Ok(()) => {}
        Err(msg) if msg.starts_with("corrupt") => {
            return err_internal(
                "settings PUT: app-state.json corrupt, refusing overwrite",
                "app-state.json is corrupt or not a JSON object; refusing to overwrite (see server log)",
            )
        }
        Err(e) => return err_internal("settings PUT: persist failed", e),
    }
    let mut merged = merged_out.unwrap_or_else(|| json!({}));
    ok(mask_secrets(&mut merged).clone())
}

const SECRET_MASK: &str = "********";

/// Keys whose values are credentials wherever they appear. Beyond the
/// literal config keys (`apiKey`, `token`, ...) this covers the credential
/// header names users put into `customHeaders` / `extraHeaders` and the
/// key names used inside `extraRequestBody` fragments (F-01).
fn is_sensitive_key(lower: &str) -> bool {
    matches!(
        lower,
        "apikey"
            | "api_key"
            | "api-key"
            | "token"
            | "accesstoken"
            | "access_token"
            | "access-token"
            | "authorization"
            | "proxy-authorization"
            | "x-api-key"
            | "x-goog-api-key"
            | "x-subscription-token"
            | "secret"
            | "clientsecret"
            | "client_secret"
            | "client-secret"
            | "password"
            | "privatekey"
            | "private_key"
    )
}

/// Recursively mask values under credential-bearing keys (`apiKey`,
/// `token`, `accessToken`, plus credential header names per
/// `is_sensitive_key`) for GET responses.
fn mask_secrets(value: &mut Value) -> &mut Value {
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if is_sensitive_key(&key.to_lowercase()) {
                    if !item.is_null() {
                        *item = Value::String(SECRET_MASK.to_string());
                    }
                } else {
                    mask_secrets(item);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                mask_secrets(item);
            }
        }
        _ => {}
    }
    value
}

/// Remove `"********"` leaves from an incoming update so a masked GET
/// response PUT back wholesale can never persist the mask literal under a
/// key that did not exist before (F-26).
fn strip_mask_leaves(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if v.as_str() == Some(SECRET_MASK) {
                    continue;
                }
                out.insert(k.clone(), strip_mask_leaves(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(strip_mask_leaves).collect()),
        other => other.clone(),
    }
}

/// Deep merge for PUT /settings semantics:
/// - object + object merges recursively
/// - `null` deletes the key
/// - the secret mask `"********"` keeps the existing value (round-trip
///   safe: masked GET output can be PUT back without leaking or losing keys)
fn deep_merge_masked(existing: &mut Value, update: &Value) {
    if let (Some(existing_map), Some(update_map)) = (existing.as_object_mut(), update.as_object()) {
        for (key, update_value) in update_map {
            if update_value.is_null() {
                existing_map.remove(key);
                continue;
            }
            let masked = update_value.as_str() == Some(SECRET_MASK);
            let nested = existing_map
                .get(key)
                .map(|current| current.is_object() && update_value.is_object())
                .unwrap_or(false);
            if nested {
                deep_merge_masked(existing_map.get_mut(key).unwrap(), update_value);
            } else if masked {
                // keep whatever already exists; if the key does not exist
                // there is nothing to keep - persisting the literal mask
                // would later be sent to real providers as a credential
                // and fail there (F-26), so leave it unset
            } else {
                existing_map.insert(key.clone(), strip_mask_leaves(update_value));
            }
        }
    }
}

/// POST /api/v1/projects/create — scaffold a new wiki project directory
/// (wiki/, raw/sources/, purpose.md, schema.md, .llm-wiki/project.json)
/// and pin it as the "current" project.
fn handle_create_project(app: &ServerHost, body: &str) -> ApiResponse {
    #[derive(Deserialize)]
    struct CreateProjectRequest {
        name: String,
        path: String,
    }
    let req: CreateProjectRequest = match serde_json::from_str(body) {
        Ok(req) => req,
        Err(e) => return err(400, format!("Invalid JSON: {e}")),
    };
    // Path-safety: `name` must be a single safe path component (it is
    // joined onto `path`), and `path` must be an existing directory. This
    // endpoint can write files, so it does not accept traversal or
    // arbitrary new locations.
    let name = req.name.trim();
    if name.is_empty() {
        return err(400, "name is required");
    }
    if name.len() > 120 {
        return err(400, "name is too long (max 120 characters)");
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.starts_with('.') {
        return err(
            400,
            "invalid name: use a single path component without '..' or leading dots",
        );
    }
    let parent = std::path::Path::new(req.path.trim());
    match fs::metadata(parent) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return err(400, "path must be an existing directory"),
        Err(e) => return err(400, format!("parent path does not exist: {e}")),
    }
    let project = match commands::project::create_project(name.to_string(), req.path.clone()) {
        Ok(project) => project,
        Err(e) => return err(400, e),
    };
    project_registry::register_project(name, &project.path);
    project_registry::set_current_project_path(&project.path);
    // Ensure the project has a stable identity BEFORE registering it in
    // app-state — otherwise the registry key falls back to the path and
    // clients can no longer address the project by the returned id.
    let project_id = match ensure_project_identity(&project.path, name) {
        Ok(id) => id,
        Err(e) => return err_internal("project identity", e),
    };
    let registered = persist_project_registration(app, &project_id, name, &project.path);
    if !registered {
        eprintln!(
            "[api] project {} created on disk but registration did not persist",
            project_id
        );
    }
    invalidate_graph_cache(&project.path);
    ok(json!({
        "ok": true,
        "registered": registered,
        "project": {
            "id": project_id,
            "name": project.name,
            "path": project.path,
        },
        "note": "registered as the current project; chat/search accept project_id 'current'",
    }))
}

/// POST /api/v1/projects/current — body {path}: re-pin the project that
/// "current" refers to (replaces the old MCP set_project capability).
fn handle_set_current_project(app: &ServerHost, body: &str) -> ApiResponse {
    #[derive(Deserialize)]
    struct SetCurrentRequest {
        path: String,
    }
    let req: SetCurrentRequest = match serde_json::from_str(body) {
        Ok(req) => req,
        Err(e) => return err(400, format!("Invalid JSON: {e}")),
    };
    let root = std::path::PathBuf::from(&req.path);
    if !root.is_dir() {
        return err(404, format!("Project directory not found: {}", req.path));
    }
    // Only wiki-shaped directories can be registered. Registering an
    // arbitrary directory combined with DELETE /projects/{id}
    // ?deleteFiles=true used to make "switch project" a two-step remote
    // disk wiper (F-18); requiring a wiki/ root scopes the deletable set
    // to actual wiki projects (F-54).
    if !root.join("wiki").is_dir() {
        return err(
            400,
            "Not a wiki project: missing wiki/ directory (scaffold one with POST /api/v1/projects/create)",
        );
    }
    let display_name = std::path::Path::new(&req.path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Project")
        .to_string();
    project_registry::register_project(&display_name, &req.path);
    project_registry::set_current_project_path(&req.path);
    // Give dirs without an identity file one (same as projects/create), so
    // the project is addressable by a stable UUID instead of its path.
    let project_id = match ensure_project_identity(&req.path, &display_name) {
        Ok(id) => id,
        Err(e) => return err_internal("project identity", e),
    };
    let registered = persist_project_registration(app, &project_id, &display_name, &req.path);
    invalidate_config_cache();
    ok(json!({
        "ok": true,
        "registered": registered,
        "current": req.path.replace('\\', "/")
    }))
}

/// GET /api/v1/projects/{id}/history?path=<rel> — version list of one
/// project file, newest first.
fn handle_list_file_history(app: &ServerHost, project_id: &str, query: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let params = parse_query(query);
    let Some(rel) = params.get("path") else {
        return err(400, "Missing path query parameter");
    };
    // Same guard order as files/content and DELETE files/content: the
    // public-surface check answers first so non-public paths get the
    // fixed 403 instead of safe_join's resolver error text (F-22).
    if !is_public_project_rel(rel) {
        return err(403, "Path is not exposed by the local API");
    }
    let abs_path = match safe_join(&project.path, rel) {
        Ok(abs_path) => abs_path.to_string_lossy().into_owned(),
        Err(e) => return err(400, e),
    };
    match app.block_on(commands::file_history::list_file_history(
        project.path.clone(),
        abs_path,
    )) {
        Ok(entries) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "path": rel,
            "entries": entries,
        })),
        Err(e) => err(404, e),
    }
}

/// DELETE /api/v1/projects/{projectId} — remove a project from the
/// registry. Disk files are kept unless ?deleteFiles=true; with the flag
/// the project directory is removed recursively. Deleting by 'current' is
/// rejected — pass the explicit project id.
fn handle_delete_project(app: &ServerHost, project_id: &str, query: &str) -> ApiResponse {
    if project_id.eq_ignore_ascii_case("current") {
        return err(
            400,
            "refusing to delete by 'current' — pass the explicit project id",
        );
    }
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let params = parse_query(query);
    let delete_files = params
        .get("deleteFiles")
        .map(|v| v == "true")
        .unwrap_or(false);
    if delete_files {
        // Recursive disk removal needs a second, explicit confirmation
        // naming the directory (F-18): a stale client or mistyped URL must
        // not be able to remove a project from disk.
        let dir_name = std::path::Path::new(&project.path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if params.get("confirm").map(String::as_str) != Some(dir_name) {
            return err(
                400,
                format!(
                    "deleteFiles=true requires confirm={dir_name} (the project directory name)"
                ),
            );
        }
    }

    // 1) Remove from app-state.json projectRegistry + recentProjects
    //    (read-modify-write under the app-state lock, atomic tmp+rename).
    let mut removed_from_store = false;
    if let Some(store) = app.app_state_path() {
        let path_norm = normalize_path(&project.path);
        let result = update_app_state_file(&store, |merged| {
            if let Some(map) = merged.as_object_mut() {
                if let Some(reg) = map
                    .get_mut("projectRegistry")
                    .and_then(Value::as_object_mut)
                {
                    reg.remove(&project.id);
                    let stale: Vec<String> = reg
                        .iter()
                        .filter(|(_, v)| {
                            v.get("path")
                                .and_then(Value::as_str)
                                .map(normalize_path)
                                .as_deref()
                                == Some(path_norm.as_str())
                        })
                        .map(|(k, _)| k.clone())
                        .collect();
                    for k in stale {
                        reg.remove(&k);
                    }
                }
                if let Some(recents) = map.get_mut("recentProjects").and_then(Value::as_array_mut) {
                    recents.retain(|v| {
                        v.get("path")
                            .and_then(Value::as_str)
                            .map(normalize_path)
                            .as_deref()
                            != Some(path_norm.as_str())
                    });
                }
            }
            true
        });
        match result {
            Ok(()) => removed_from_store = true,
            Err(e) => {
                eprintln!("[api] project deletion: app-state update failed: {e}");
            }
        }
    }

    // 2) Remove from the in-memory session registry; clears 'current' if it
    //    pointed at the removed project.
    project_registry::remove_project_by_path(&project.path);
    invalidate_graph_cache(&project.path);

    // 3) Optional disk removal (off by default).
    let mut files_deleted = false;
    if delete_files && !project.path.is_empty() {
        match fs::remove_dir_all(&project.path) {
            Ok(()) => files_deleted = true,
            Err(e) => return err_internal("project deleteFiles", e),
        }
    }

    ok(json!({
        "ok": true,
        "removed": removed_from_store,
        "id": project.id,
        "path": project.path,
        "filesDeleted": files_deleted,
    }))
}

/// POST /api/v1/projects/{id}/history/restore — body {path, entryId}.
fn handle_restore_file_history(app: &ServerHost, project_id: &str, body: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    #[derive(Deserialize)]
    struct RestoreRequest {
        path: String,
        #[serde(rename = "entryId")]
        entry_id: String,
    }
    let req: RestoreRequest = match serde_json::from_str(body) {
        Ok(req) => req,
        Err(e) => return err(400, format!("Invalid JSON: {e}")),
    };
    if !is_public_project_rel(&req.path) {
        return err(403, "Path is not exposed by the local API");
    }
    let abs_path = match safe_join(&project.path, &req.path) {
        Ok(abs_path) => abs_path.to_string_lossy().into_owned(),
        Err(e) => return err(400, e),
    };
    match app.block_on(commands::file_history::restore_file_history(
        project.path.clone(),
        abs_path,
        req.entry_id.clone(),
    )) {
        Ok(message) => {
            invalidate_graph_cache(&project.path);
            ok(json!({
                "ok": true,
                "projectId": project.id,
                "path": req.path,
                "restored": req.entry_id,
                "message": message,
            }))
        }
        Err(e) => err(400, e),
    }
}

/// GET /api/v1/projects/{id}/history/stats
fn handle_file_history_stats(app: &ServerHost, project_id: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    match app.block_on(commands::file_history::get_file_history_stats(
        project.path.clone(),
    )) {
        Ok(stats) => ok(json!({ "ok": true, "projectId": project.id, "stats": stats })),
        Err(e) => err_internal("handler error", e),
    }
}

/// GET /api/v1/projects/{id}/history/settings
fn handle_get_history_settings(app: &ServerHost, project_id: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    match app.block_on(commands::file_history::get_file_history_settings(
        project.path.clone(),
    )) {
        Ok(settings) => ok(json!({ "ok": true, "projectId": project.id, "settings": settings })),
        Err(e) => err_internal("handler error", e),
    }
}

/// PUT /api/v1/projects/{id}/history/settings
fn handle_put_history_settings(app: &ServerHost, project_id: &str, body: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let parsed = match serde_json::from_str(body) {
        Ok(parsed) => parsed,
        Err(e) => return err(400, format!("Invalid JSON: {e}")),
    };
    match app.block_on(commands::file_history::set_file_history_settings(
        project.path.clone(),
        parsed,
    )) {
        Ok(settings) => ok(json!({ "ok": true, "projectId": project.id, "settings": settings })),
        Err(e) => err(400, e),
    }
}

/// POST /api/v1/projects/{id}/maintenance/rebuild-index — regenerates
/// wiki/index.md from the current page set.
fn handle_rebuild_index(app: &ServerHost, project_id: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    match app.block_on(commands::project_maintenance::rebuild_wiki_index(
        project.path.clone(),
    )) {
        Ok(result) => ok(json!({ "ok": true, "projectId": project.id, "result": result })),
        Err(e) => err_internal("handler error", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_project_dir() -> PathBuf {
        // Per-process atomic sequence appended to the timestamp so two
        // tests calling this concurrently can't collide on the same dir
        // (nanos alone are not unique enough under parallel `cargo test`,
        // which would let one test's remove_dir_all delete another's files).
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("llm-wiki-api-test-{id}-{seq}"));
        fs::create_dir_all(path.join("wiki")).unwrap();
        path
    }

    #[test]
    fn safe_join_rejects_traversal() {
        let root = test_project_dir();
        let root_str = root.to_string_lossy();
        assert!(safe_join(&root_str, "../secret.md").is_err());
        assert!(safe_join(&root_str, "wiki/../../secret.md").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn safe_join_accepts_project_relative_paths() {
        let root = test_project_dir();
        let root_str = root.to_string_lossy();
        let joined = safe_join(&root_str, "wiki/index.md").unwrap();
        assert_eq!(joined, root.join("wiki/index.md"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn graph_uses_relative_slugs_and_resolves_path_links() {
        let root = test_project_dir();
        fs::create_dir_all(root.join("wiki/topic")).unwrap();
        fs::write(
            root.join("wiki/index.md"),
            "---\ntype: overview\n---\n# Index\n[[topic/Detail Page.md#section|Detail]]",
        )
        .unwrap();
        fs::write(
            root.join("wiki/topic/Detail Page.md"),
            "---\ntype: entity\n---\n# Detail Page",
        )
        .unwrap();

        let (nodes, edges) = build_graph(root.to_string_lossy().as_ref()).unwrap();
        assert!(nodes.iter().any(|node| node.id == "Detail Page"));
        assert!(edges
            .iter()
            .any(|edge| { edge.source == "index" && edge.target == "Detail Page" }));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn graph_includes_frontmatter_related_links() {
        let root = test_project_dir();
        fs::write(
            root.join("wiki/source.md"),
            "---\nrelated: [target]\n---\n# Source",
        )
        .unwrap();
        fs::write(root.join("wiki/target.md"), "# Target").unwrap();

        let (_, edges) = build_graph(root.to_string_lossy().as_ref()).unwrap();
        assert!(edges
            .iter()
            .any(|edge| edge.source == "source" && edge.target == "target"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn graph_drops_edges_to_hidden_query_pages() {
        let root = test_project_dir();
        fs::write(
            root.join("wiki/source.md"),
            "---\nrelated: [hidden]\n---\n# Source",
        )
        .unwrap();
        fs::write(
            root.join("wiki/hidden.md"),
            "---\ntype: query\n---\n# Hidden\n[[source]]",
        )
        .unwrap();

        let (nodes, edges) = build_graph(root.to_string_lossy().as_ref()).unwrap();
        assert!(nodes.iter().all(|node| node.id != "hidden"));
        assert!(edges.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn graph_does_not_resolve_ambiguous_bare_stems() {
        let root = test_project_dir();
        fs::create_dir_all(root.join("wiki/a")).unwrap();
        fs::create_dir_all(root.join("wiki/b")).unwrap();
        fs::write(root.join("wiki/index.md"), "# Index\n[[same]]").unwrap();
        fs::write(root.join("wiki/a/same.md"), "# A").unwrap();
        fs::write(root.join("wiki/b/same.md"), "# B").unwrap();

        let (nodes, edges) = build_graph(root.to_string_lossy().as_ref()).unwrap();
        assert_eq!(nodes.len(), 3);
        assert!(nodes.iter().any(|node| node.id == "a/same"));
        assert!(nodes.iter().any(|node| node.id == "b/same"));
        assert!(edges.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn graph_exact_id_wins_over_a_colliding_title() {
        let root = test_project_dir();
        fs::write(root.join("wiki/index.md"), "# Home").unwrap();
        fs::write(root.join("wiki/glossary.md"), "# Index\n[[index]]").unwrap();

        let (_, edges) = build_graph(root.to_string_lossy().as_ref()).unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target, "index");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn graph_exact_id_wins_over_a_normalized_id_collision() {
        let mut raw = BTreeMap::new();
        for id in ["Machine Learning", "machine-learning"] {
            raw.insert(
                id.to_string(),
                (
                    id.to_string(),
                    "entity".to_string(),
                    format!("wiki/{id}.md"),
                    vec![],
                ),
            );
        }
        let aliases = graph_aliases(&raw);
        assert_eq!(
            resolve_link("Machine Learning", &aliases).as_deref(),
            Some("Machine Learning")
        );
        assert_eq!(
            resolve_link("machine-learning", &aliases).as_deref(),
            Some("machine-learning")
        );
    }

    #[test]
    fn graph_link_normalization_accepts_common_path_forms() {
        let mut raw = BTreeMap::new();
        raw.insert(
            "Page".to_string(),
            (
                "Display Title".to_string(),
                "entity".to_string(),
                "wiki/Page.md".to_string(),
                vec![],
            ),
        );
        let aliases = graph_aliases(&raw);
        for link in [
            "./Page",
            "wiki/Page",
            "/wiki/Page",
            "Page.MD",
            "Display Title",
        ] {
            assert_eq!(
                resolve_link(link, &aliases).as_deref(),
                Some("Page"),
                "{link}"
            );
        }
    }

    #[test]
    fn graph_pagination_can_be_merged_without_losing_edges() {
        let nodes = ["a", "b", "c"]
            .into_iter()
            .map(|id| ApiGraphNode {
                id: id.to_string(),
                label: id.to_string(),
                node_type: "entity".to_string(),
                path: format!("wiki/{id}.md"),
                link_count: 1,
            })
            .collect::<Vec<_>>();
        let edges = vec![
            ApiGraphEdge {
                source: "a".to_string(),
                target: "c".to_string(),
                weight: 1.0,
            },
            ApiGraphEdge {
                source: "b".to_string(),
                target: "c".to_string(),
                weight: 1.0,
            },
        ];

        let first = select_graph_page(&nodes, &edges, None, None, 0, 2, true);
        let second = select_graph_page(&nodes, &edges, None, None, 2, 2, true);
        assert_eq!(first.nodes.len(), 2);
        assert_eq!(first.total_count, 3);
        assert!(first.has_more);
        assert_eq!(second.nodes.len(), 1);
        assert!(!second.has_more);
        assert_eq!(first.edges.len() + second.edges.len(), 2);
    }

    #[test]
    fn graph_legacy_response_keeps_edge_endpoints_in_the_node_page() {
        let nodes = ["a", "b", "c"]
            .into_iter()
            .map(|id| ApiGraphNode {
                id: id.to_string(),
                label: id.to_string(),
                node_type: "entity".to_string(),
                path: format!("wiki/{id}.md"),
                link_count: 1,
            })
            .collect::<Vec<_>>();
        let edges = vec![ApiGraphEdge {
            source: "a".to_string(),
            target: "c".to_string(),
            weight: 1.0,
        }];
        let page = select_graph_page(&nodes, &edges, None, None, 0, 2, false);
        assert!(page.edges.is_empty());
    }

    #[test]
    fn graph_cache_invalidates_when_a_page_changes() {
        let root = test_project_dir();
        let page = root.join("wiki/page.md");
        fs::write(&page, "# Before").unwrap();
        let before = load_graph(root.to_string_lossy().as_ref()).unwrap();
        assert_eq!(before.nodes[0].label, "Before");
        let cached = load_graph(root.to_string_lossy().as_ref()).unwrap();
        assert!(Arc::ptr_eq(&before, &cached));

        fs::write(&page, "# After with a different length").unwrap();
        invalidate_graph_cache(root.to_string_lossy().as_ref());
        let after = load_graph(root.to_string_lossy().as_ref()).unwrap();
        assert_eq!(after.nodes[0].label, "After with a different length");
        assert!(!Arc::ptr_eq(&before, &after));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn query_parser_decodes_percent_and_plus() {
        // RFC 3986 (F-19): plus signs are literals in query values; only
        // %XX escapes decode. Clients must percent-encode literal pluses.
        let parsed = parse_query("path=wiki%2Fhello+world.md&token=a%2Bb");
        assert_eq!(parsed.get("path").unwrap(), "wiki/hello+world.md");
        assert_eq!(parsed.get("token").unwrap(), "a+b");
    }

    #[test]
    fn snippet_handles_unicode_boundaries() {
        let content = "前言。这里是关于知识图谱过滤的中文内容。后续说明。";
        let snippet = commands::search::build_snippet(content, "知识图谱");
        assert!(snippet.contains("知识图谱"));
    }

    // F-01: credential header names inside free-form maps must be masked,
    // not just the literal apiKey/token config keys.
    #[test]
    fn mask_secrets_masks_header_map_values() {
        let mut v = json!({
            "llmConfig": {
                "customHeaders": { "authorization": "sk-live", "content-type": "application/json" },
                "extraRequestBody": { "api_key": "abc", "temperature": 0.5 }
            },
            "embeddingConfig": { "extraHeaders": { "x-api-key": "emb-1" } }
        });
        mask_secrets(&mut v);
        assert_eq!(
            v["llmConfig"]["customHeaders"]["authorization"],
            SECRET_MASK
        );
        assert_eq!(v["llmConfig"]["extraRequestBody"]["api_key"], SECRET_MASK);
        assert_eq!(
            v["embeddingConfig"]["extraHeaders"]["x-api-key"],
            SECRET_MASK
        );
        // non-credential entries stay readable
        assert_eq!(
            v["llmConfig"]["customHeaders"]["content-type"],
            "application/json"
        );
        assert_eq!(v["llmConfig"]["extraRequestBody"]["temperature"], 0.5);
    }

    // F-26: PUTting a masked GET response back must never persist the
    // mask literal - neither at top level nor inside inserted objects.
    #[test]
    fn deep_merge_never_persists_the_mask_literal() {
        let mut base = json!({ "providerConfigs": { "existing": { "apiKey": "real-key" } } });
        let update = json!({
            "providerConfigs": {
                "existing": { "apiKey": SECRET_MASK },
                "brand-new": { "apiKey": SECRET_MASK, "model": "m1" }
            },
            "freshToken": SECRET_MASK,
            "llmConfig": { "apiKey": SECRET_MASK, "model": "glm" }
        });
        deep_merge_masked(&mut base, &update);
        let raw = serde_json::to_string(&base).unwrap();
        assert!(!raw.contains(SECRET_MASK), "mask literal leaked: {raw}");
        // existing key keeps its value; masked unknown keys stay unset;
        // merged sections still merge their non-secret fields
        assert_eq!(base["providerConfigs"]["existing"]["apiKey"], "real-key");
        assert!(base["providerConfigs"]["brand-new"]["apiKey"].is_null());
        assert_eq!(base["providerConfigs"]["brand-new"]["model"], "m1");
        assert!(base["freshToken"].is_null());
        assert_eq!(base["llmConfig"]["model"], "glm");
        assert!(base["llmConfig"]["apiKey"].is_null());
    }

    // F-25: concurrent read-modify-write on one app-state file must not
    // lose updates (each thread writes its own keys; all must survive).
    #[test]
    fn concurrent_app_state_writers_do_not_lose_updates() {
        let dir = std::env::temp_dir().join(format!("appstate-f25-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let shared = std::sync::Arc::new(dir.join("app-state.json"));
        std::fs::write(shared.as_path(), "{}").unwrap();
        let mut handles = Vec::new();
        for t in 0..4 {
            let shared = shared.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..10 {
                    let key = format!("thread{t}k{i}");
                    let path = shared.clone();
                    let res = update_app_state_file(&path, move |merged| {
                        merged
                            .as_object_mut()
                            .unwrap()
                            .insert(key.clone(), json!(i));
                        true
                    });
                    assert!(res.is_ok());
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let raw = std::fs::read_to_string(shared.as_path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        for t in 0..4 {
            for i in 0..10 {
                assert_eq!(
                    v[format!("thread{t}k{i}")],
                    json!(i),
                    "lost update t{t}k{i}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // F-03: the canonical page-id mapping preserves case and scopes to
    // wiki/*.md; writer, filter and delete now share one implementation.
    #[test]
    fn canonical_page_id_preserves_case_and_scopes_to_wiki() {
        assert_eq!(
            commands::vectorstore::canonical_page_id("wiki/Notes/Foo.md").as_deref(),
            Some("Notes/Foo")
        );
        assert_eq!(
            commands::vectorstore::canonical_page_id("wiki//deep/Page.md").as_deref(),
            Some("deep/Page")
        );
        assert_eq!(
            commands::vectorstore::canonical_page_id("raw/sources/a.pdf"),
            None
        );
        assert_eq!(
            commands::vectorstore::canonical_page_id("wiki/nope.txt"),
            None
        );
    }

    // F-33: a query longer than the body (title hit, tiny body) must not
    // underflow the search loop - it simply finds no match.
    #[test]
    fn snippet_handles_query_longer_than_content() {
        let snippet = commands::search::build_snippet(
            "TODO
",
            "coal mine safety regulation",
        );
        assert!(snippet.contains("TODO"));
        // Multi-byte content shorter than the query must be safe too.
        let snippet = commands::search::build_snippet(
            "采
",
            "煤矿安全规程全文检索",
        );
        assert!(snippet.contains("采"));
    }

    #[test]
    fn public_api_paths_exclude_internal_state() {
        assert!(is_public_project_rel("wiki/index.md"));
        assert!(is_public_project_rel("Wiki/index.md"));
        assert!(is_public_project_rel("raw/sources/source.md"));
        assert!(is_public_project_rel("Raw/Sources/source.md"));
        assert!(!is_public_project_rel(".llm-wiki/file-change-queue.json"));
        assert!(!is_public_project_rel("wiki/.draft.md"));
    }

    #[test]
    fn review_query_defaults_to_unresolved_items() {
        let root = test_project_dir();
        let state_dir = root.join(".llm-wiki");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("review.json"),
            json!([
                {
                    "id": "r1",
                    "type": "missing-page",
                    "title": "Missing page: Attention",
                    "description": "Add Attention",
                    "options": [],
                    "resolved": false,
                    "createdAt": 1,
                    "internalSecret": "do-not-expose"
                },
                {
                    "id": "r2",
                    "type": "duplicate",
                    "title": "Duplicate: LLM",
                    "description": "Merge pages",
                    "options": [],
                    "resolved": true,
                    "createdAt": 2
                }
            ])
            .to_string(),
        )
        .unwrap();

        let query = parse_review_query("").unwrap();
        let reviews = load_review_items(root.to_str().unwrap(), &query).unwrap();

        assert_eq!(query.status.as_str(), "unresolved");
        assert_eq!(reviews.len(), 1);
        assert_eq!(
            reviews[0].get("id").and_then(Value::as_str),
            Some(review_id_for_parts("missing-page", "Missing page: Attention").as_str())
        );
        assert!(reviews[0].get("internalSecret").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn review_title_normalization_requires_a_prefix_delimiter() {
        assert_eq!(
            normalize_review_title("Missing page: Attention"),
            "attention"
        );
        assert_eq!(
            normalize_review_title(" Missing page: Attention"),
            "attention"
        );
        assert_eq!(
            normalize_review_title("Missing page Attention"),
            "missing page attention"
        );
        assert_eq!(normalize_review_title("疑似重复 注意力"), "疑似重复 注意力");
        assert_ne!(
            review_id_for_parts("missing-page", "Missing page: Attention"),
            review_id_for_parts("missing-page", "Missing page Attention")
        );
        assert_eq!(
            review_id_for_parts("missing-page", "Missing page: Attention"),
            "review-dbdcf949"
        );
        assert_eq!(
            review_id_for_parts("missing-page", " Missing page: Attention"),
            "review-dbdcf949"
        );
        assert_eq!(
            review_id_for_parts("missing-page", "Missing page Attention"),
            "review-fa5d9960"
        );
        assert_eq!(
            review_id_for_parts("missing-page", "疑似重复 注意力"),
            "review-d2dacda0"
        );
    }

    #[test]
    fn review_query_filters_by_type_status_and_limit() {
        let root = test_project_dir();
        let state_dir = root.join(".llm-wiki");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("review.json"),
            json!([
                { "id": "r1", "type": "missing-page", "title": "r1", "resolved": false, "createdAt": 1 },
                { "id": "r2", "type": "missing-page", "title": "r2", "resolved": false, "createdAt": 2 },
                { "id": "r3", "type": "duplicate", "resolved": false, "createdAt": 3 },
                { "id": "r4", "type": "missing-page", "resolved": true, "createdAt": 4 }
            ])
            .to_string(),
        )
        .unwrap();

        let query = parse_review_query("status=all&type=missing-page&limit=2").unwrap();
        let reviews = load_review_items(root.to_str().unwrap(), &query).unwrap();

        assert_eq!(query.status.as_str(), "all");
        assert_eq!(
            reviews
                .iter()
                .filter_map(|r| r.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect::<Vec<_>>(),
            vec![
                review_id_for_parts("missing-page", "r1"),
                review_id_for_parts("missing-page", "r2"),
            ]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn review_query_collapses_legacy_duplicate_ids_to_stable_id() {
        let root = test_project_dir();
        let state_dir = root.join(".llm-wiki");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("review.json"),
            json!([
                {
                    "id": "review-1",
                    "type": "missing-page",
                    "title": "Attention",
                    "description": "",
                    "affectedPages": ["a.md"],
                    "resolved": false,
                    "createdAt": 5
                },
                {
                    "id": "review-2",
                    "type": "missing-page",
                    "title": "Missing page: Attention",
                    "description": "resolved copy",
                    "affectedPages": ["b.md"],
                    "resolved": true,
                    "resolvedAction": "user-resolved",
                    "createdAt": 2
                }
            ])
            .to_string(),
        )
        .unwrap();

        let query = parse_review_query("status=all").unwrap();
        let reviews = load_review_items(root.to_str().unwrap(), &query).unwrap();

        assert_eq!(reviews.len(), 1);
        assert_eq!(
            reviews[0].get("id").and_then(Value::as_str),
            Some(review_id_for_parts("missing-page", "Attention").as_str())
        );
        assert_eq!(
            reviews[0].get("resolved").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            reviews[0].get("resolvedAction").and_then(Value::as_str),
            Some("user-resolved")
        );
        assert_eq!(
            reviews[0]
                .get("affectedPages")
                .and_then(Value::as_array)
                .map(|pages| pages.iter().filter_map(Value::as_str).collect::<Vec<_>>()),
            Some(vec!["a.md", "b.md"])
        );
        assert_eq!(
            reviews[0].get("createdAt").and_then(Value::as_f64),
            Some(2.0)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn review_query_filters_status_after_stable_id_merge() {
        let root = test_project_dir();
        let state_dir = root.join(".llm-wiki");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("review.json"),
            json!([
                {
                    "id": "review-old-unresolved",
                    "type": "missing-page",
                    "title": "Attention",
                    "resolved": false,
                    "createdAt": 5
                },
                {
                    "id": "review-old-resolved",
                    "type": "missing-page",
                    "title": "Missing page: Attention",
                    "resolved": true,
                    "resolvedAction": "Done",
                    "createdAt": 6
                }
            ])
            .to_string(),
        )
        .unwrap();

        let unresolved = parse_review_query("status=unresolved").unwrap();
        let unresolved_reviews = load_review_items(root.to_str().unwrap(), &unresolved).unwrap();
        assert!(unresolved_reviews.is_empty());

        let all = parse_review_query("status=all").unwrap();
        let all_reviews = load_review_items(root.to_str().unwrap(), &all).unwrap();
        assert_eq!(all_reviews.len(), 1);
        assert_eq!(
            all_reviews[0].get("resolved").and_then(Value::as_bool),
            Some(true)
        );
        let _ = fs::remove_dir_all(root);
    }

    fn write_reviews(root: &Path, value: Value) {
        let state_dir = root.join(".llm-wiki");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("review.json"), value.to_string()).unwrap();
    }

    fn read_reviews(root: &Path) -> Value {
        let raw = fs::read_to_string(root.join(".llm-wiki/review.json")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn patch_review_item_marks_resolved_and_preserves_unsanitized_fields() {
        let root = test_project_dir();
        write_reviews(
            &root,
            json!([
                {
                    "id": "r1",
                    "type": "missing-page",
                    "resolved": false,
                    "createdAt": 1,
                    "internalSecret": "keep-me"
                },
                { "id": "r2", "type": "duplicate", "resolved": false, "createdAt": 2 }
            ]),
        );

        let found = patch_review_item(root.to_str().unwrap(), "r1", true, Some("Skip")).unwrap();
        assert!(found);

        // Re-read the RAW file: r1 must be resolved with the action label,
        // its non-sanitized `internalSecret` preserved (the write path must
        // not go through the sanitizing reader), and r2 left untouched.
        let parsed = read_reviews(&root);
        let items = parsed.as_array().unwrap();
        let r1 = items
            .iter()
            .find(|i| i.get("id").and_then(Value::as_str) == Some("r1"))
            .unwrap();
        assert_eq!(r1.get("resolved").and_then(Value::as_bool), Some(true));
        assert_eq!(
            r1.get("resolvedAction").and_then(Value::as_str),
            Some("Skip")
        );
        assert_eq!(
            r1.get("internalSecret").and_then(Value::as_str),
            Some("keep-me")
        );
        let r2 = items
            .iter()
            .find(|i| i.get("id").and_then(Value::as_str) == Some("r2"))
            .unwrap();
        assert_eq!(r2.get("resolved").and_then(Value::as_bool), Some(false));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn patch_review_item_accepts_stable_id_for_legacy_counter_item() {
        let root = test_project_dir();
        write_reviews(
            &root,
            json!([
                {
                    "id": "review-1",
                    "type": "missing-page",
                    "title": "Missing page: Attention",
                    "resolved": false
                }
            ]),
        );

        let stable_id = review_id_for_parts("missing-page", "Attention");
        let found =
            patch_review_item(root.to_str().unwrap(), &stable_id, true, Some("API")).unwrap();
        assert!(found);

        let parsed = read_reviews(&root);
        let item = &parsed.as_array().unwrap()[0];
        assert_eq!(
            item.get("id").and_then(Value::as_str),
            Some(stable_id.as_str())
        );
        assert_eq!(item.get("resolved").and_then(Value::as_bool), Some(true));
        assert_eq!(
            item.get("resolvedAction").and_then(Value::as_str),
            Some("API")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn patch_review_item_can_reopen_with_resolved_false() {
        let root = test_project_dir();
        write_reviews(
            &root,
            json!([{ "id": "r1", "resolved": true, "resolvedAction": "Skip" }]),
        );

        let found = patch_review_item(root.to_str().unwrap(), "r1", false, None).unwrap();
        assert!(found);

        let parsed = read_reviews(&root);
        let r1 = &parsed.as_array().unwrap()[0];
        assert_eq!(r1.get("resolved").and_then(Value::as_bool), Some(false));
        assert!(r1.get("resolvedAction").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn patch_review_item_returns_false_for_unknown_id() {
        let root = test_project_dir();
        write_reviews(&root, json!([{ "id": "r1", "resolved": false }]));

        let found = patch_review_item(root.to_str().unwrap(), "nope", true, None).unwrap();
        assert!(!found);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn patch_review_item_missing_file_returns_false() {
        let root = test_project_dir();
        let found = patch_review_item(root.to_str().unwrap(), "r1", true, None).unwrap();
        assert!(!found);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delete_review_item_removes_match_and_preserves_other_fields() {
        let root = test_project_dir();
        write_reviews(
            &root,
            json!([
                { "id": "r1", "resolved": false, "internalSecret": "remove" },
                { "id": "r2", "resolved": false, "internalSecret": "keep" }
            ]),
        );

        assert!(delete_review_item(root.to_str().unwrap(), "r1").unwrap());
        let parsed = read_reviews(&root);
        let items = parsed.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].get("id").and_then(Value::as_str), Some("r2"));
        assert_eq!(
            items[0].get("internalSecret").and_then(Value::as_str),
            Some("keep")
        );
        assert!(!delete_review_item(root.to_str().unwrap(), "missing").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bulk_resolve_request_accepts_documented_review_ids_alias() {
        let request: BulkResolveRequest = serde_json::from_value(json!({
            "reviewIds": ["r1", "r2"],
            "action": "API"
        }))
        .unwrap();
        assert_eq!(request.ids, vec!["r1", "r2"]);
        assert_eq!(request.action.as_deref(), Some("API"));
    }

    #[test]
    fn resolve_review_items_bulk_resolves_matching_and_reports_not_found() {
        let root = test_project_dir();
        write_reviews(
            &root,
            json!([
                { "id": "r1", "resolved": false, "internalSecret": "a" },
                { "id": "r2", "resolved": false },
                { "id": "r3", "resolved": false }
            ]),
        );

        let ids = vec!["r1".to_string(), "r3".to_string(), "missing".to_string()];
        let (resolved, not_found) =
            resolve_review_items(root.to_str().unwrap(), &ids, Some("Bulk")).unwrap();

        // Input order preserved; missing id reported, not 404'd.
        assert_eq!(resolved, vec!["r1".to_string(), "r3".to_string()]);
        assert_eq!(not_found, vec!["missing".to_string()]);

        let parsed = read_reviews(&root);
        let items = parsed.as_array().unwrap();
        let by_id = |id: &str| {
            items
                .iter()
                .find(|i| i.get("id").and_then(Value::as_str) == Some(id))
                .unwrap()
        };
        assert_eq!(
            by_id("r1").get("resolved").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            by_id("r1").get("resolvedAction").and_then(Value::as_str),
            Some("Bulk")
        );
        // Unsanitized field survives the bulk write-back too.
        assert_eq!(
            by_id("r1").get("internalSecret").and_then(Value::as_str),
            Some("a")
        );
        assert_eq!(
            by_id("r3").get("resolved").and_then(Value::as_bool),
            Some(true)
        );
        // r2 was not in the request — untouched.
        assert_eq!(
            by_id("r2").get("resolved").and_then(Value::as_bool),
            Some(false)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_review_items_accepts_stable_ids_for_legacy_counter_items() {
        let root = test_project_dir();
        write_reviews(
            &root,
            json!([
                {
                    "id": "review-1",
                    "type": "missing-page",
                    "title": "Missing page: Attention",
                    "resolved": false
                },
                {
                    "id": "review-2",
                    "type": "duplicate",
                    "title": "Duplicate page: Transformer",
                    "resolved": false
                }
            ]),
        );

        let ids = vec![
            review_id_for_parts("missing-page", "Attention"),
            "missing".to_string(),
        ];
        let (resolved, not_found) =
            resolve_review_items(root.to_str().unwrap(), &ids, Some("Bulk")).unwrap();

        assert_eq!(resolved, vec![ids[0].clone()]);
        assert_eq!(not_found, vec!["missing".to_string()]);
        let parsed = read_reviews(&root);
        let items = parsed.as_array().unwrap();
        assert_eq!(
            items[0].get("id").and_then(Value::as_str),
            Some(ids[0].as_str())
        );
        assert_eq!(
            items[0].get("resolved").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(items[1].get("id").and_then(Value::as_str), Some("review-2"));
        assert_eq!(
            items[1].get("resolved").and_then(Value::as_bool),
            Some(false)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_review_items_missing_file_reports_all_not_found() {
        let root = test_project_dir();
        let ids = vec!["r1".to_string(), "r2".to_string()];
        let (resolved, not_found) =
            resolve_review_items(root.to_str().unwrap(), &ids, None).unwrap();
        assert!(resolved.is_empty());
        assert_eq!(not_found, ids);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_path_match_normalizes_separators() {
        assert!(project_path_matches(
            "C:/Users/me/wiki",
            "C:\\Users\\me\\wiki"
        ));
        if cfg!(windows) {
            assert!(project_path_matches("C:/Users/me/wiki", "c:/users/me/wiki"));
        } else {
            assert!(!project_path_matches(
                "C:/Users/me/wiki",
                "c:/users/me/wiki"
            ));
        }
    }

    #[test]
    fn tokenize_keeps_single_cjk_character() {
        assert_eq!(
            crate::commands::search::tokenize_query("图"),
            Vec::<String>::new()
        );
        let tokens = crate::commands::search::tokenize_query("知识图谱");
        assert!(tokens.contains(&"知识".to_string()));
    }

    #[test]
    fn text_content_filter_rejects_binary_extensions() {
        assert!(is_text_content_rel("wiki/index.md"));
        assert!(!is_text_content_rel("wiki/media/image.png"));
        assert!(!is_text_content_rel("raw/sources/book.pdf"));
        assert!(!is_text_content_rel("raw/sources/notes.rtf"));
    }

    #[test]
    fn constant_time_eq_matches_equal_bytes_only() {
        assert!(constant_time_eq(b"token", b"token"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"token", b"tokeN"));
        assert!(!constant_time_eq(b"token", b"token-longer"));
    }

    #[test]
    fn rate_limit_skips_health_and_options_only() {
        assert!(!should_rate_limit(&Method::Get, "/api/v1/health"));
        assert!(!should_rate_limit(&Method::Options, "/api/v1/projects"));
        assert!(should_rate_limit(&Method::Get, "/wp-login"));
        assert!(should_rate_limit(
            &Method::Post,
            "/api/v1/projects/current/search"
        ));
    }

    #[test]
    fn chat_endpoint_allows_larger_multimodal_bodies() {
        assert_eq!(
            body_limit_for_request(&Method::Post, "/api/v1/projects/current/chat"),
            MAX_CHAT_BODY_BYTES
        );
        assert_eq!(
            body_limit_for_request(&Method::Post, "/api/v1/projects/current/search"),
            MAX_BODY_BYTES
        );
        assert_eq!(
            body_limit_for_request(&Method::Post, "/api/v1/projects/current/pages/write"),
            MAX_PAGE_WRITE_BODY_BYTES
        );
    }

    #[test]
    fn paid_or_mutating_agent_routes_always_require_a_token() {
        assert!(is_token_required_request(
            &Method::Post,
            "/api/v1/projects/current/pages/embed"
        ));
        assert!(is_token_required_request(
            &Method::Post,
            "/api/v1/projects/current/pages/write"
        ));
        assert!(is_token_required_request(
            &Method::Post,
            "/api/v1/projects/current/chat"
        ));
        assert!(is_token_required_request(
            &Method::Delete,
            "/api/v1/projects/current/files/content?path=wiki/x.md"
        ));
        assert!(is_token_required_request(&Method::Put, "/api/v1/settings"));
        assert!(is_token_required_request(
            &Method::Post,
            "/api/v1/projects/create"
        ));
        assert!(is_token_required_request(
            &Method::Post,
            "/api/v1/projects/current/history/restore"
        ));
        assert!(!is_token_required_request(
            &Method::Post,
            "/api/v1/projects/current/search"
        ));
        assert!(!is_token_required_request(
            &Method::Get,
            "/api/v1/projects/current/graph"
        ));
        assert!(!is_token_required_request(
            &Method::Get,
            "/api/v1/projects/current/pages/embed"
        ));
    }

    #[test]
    fn chat_streaming_is_enabled_by_body_or_accept_header() {
        let path = "/api/v1/projects/current/chat";
        assert!(wants_streaming_chat(
            &Method::Post,
            path,
            r#"{"message":"hello","stream":true}"#,
            &[],
        ));
        assert!(wants_streaming_chat(
            &Method::Post,
            path,
            r#"{"message":"hello"}"#,
            &[(
                "accept".to_string(),
                "application/json, text/event-stream; q=0.9".to_string(),
            )],
        ));
        assert!(!wants_streaming_chat(
            &Method::Post,
            path,
            r#"{"message":"hello","stream":false}"#,
            &[],
        ));
        assert!(!wants_streaming_chat(
            &Method::Post,
            path,
            r#"{"message":"hello","stream":false}"#,
            &[("accept".to_string(), "text/event-stream".to_string())],
        ));
        assert!(!wants_streaming_chat(
            &Method::Post,
            "/api/v1/projects/current/search",
            r#"{"stream":true}"#,
            &[],
        ));
        assert!(!wants_streaming_chat(
            &Method::Post,
            "/api/v1/projects/current/chat/s1/cancel",
            r#"{"stream":true}"#,
            &[],
        ));
        assert!(!wants_streaming_chat(
            &Method::Post,
            "/projects/current/chat",
            r#"{"stream":true}"#,
            &[],
        ));
        assert!(!wants_streaming_chat(
            &Method::Post,
            "/api/v1/api/v1/projects/current/chat",
            r#"{"stream":true}"#,
            &[],
        ));
    }

    #[test]
    fn sse_reader_preserves_event_frames_until_sender_closes() {
        let (sender, receiver) = mpsc::sync_channel(4);
        sender
            .send(sse_frame("meta", &json!({ "sessionId": "s1" })))
            .unwrap();
        sender
            .send(sse_frame(
                "agent",
                &json!({ "type": "messageDelta", "text": "Hi" }),
            ))
            .unwrap();
        drop(sender);

        let mut body = String::new();
        let mut reader = SseReader::new(receiver);
        assert_eq!(reader.read(&mut []).unwrap(), 0);
        reader.read_to_string(&mut body).unwrap();
        assert!(body.contains("event: meta\n"));
        assert!(body.contains(r#"data: {"sessionId":"s1"}"#));
        assert!(body.contains("event: agent\n"));
        assert!(body.contains(r#""type":"messageDelta""#));
    }

    #[test]
    fn sse_queue_rejects_backpressure_without_blocking_runtime_threads() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        assert!(matches!(
            try_send_sse(&sender, b"first".to_vec()),
            SseSendResult::Sent
        ));
        assert!(matches!(
            try_send_sse(&sender, b"second".to_vec()),
            SseSendResult::Full
        ));
    }

    #[test]
    fn sse_queue_reports_disconnected_clients() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        assert!(matches!(
            try_send_sse(&sender, b"event".to_vec()),
            SseSendResult::Disconnected
        ));
    }

    #[test]
    fn terminal_sse_waits_for_queue_capacity() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(b"agent".to_vec()).unwrap();
        send_terminal_sse(sender, b"done".to_vec());

        assert_eq!(receiver.recv().unwrap(), b"agent");
        assert_eq!(receiver.recv().unwrap(), b"done");
    }

    #[test]
    fn api_config_shape_matches_production_readers() {
        // Exercises the production api_enabled/api_allow_unauthenticated/
        // api_mcp_enabled readers against a temp app-state.json so a
        // production regression fails here instead of surfacing as a 503.
        let dir = std::env::temp_dir().join(format!("llm-wiki-apicfg-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let store = dir.join("app-state.json");
        fs::write(
            &store,
            json!({
                "apiConfig": {
                    "enabled": false,
                    "allowUnauthenticated": true,
                    "allowLanAccess": true,
                    "mcpEnabled": true
                }
            })
            .to_string(),
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = ServerHost::new(
            Some(store.clone()),
            Default::default(),
            Default::default(),
            rt.handle().clone(),
        );
        assert!(!api_enabled(&host));
        assert!(api_allow_unauthenticated(&host));
        assert!(api_mcp_enabled(&host));

        // Defaults with no config file at all.
        let empty = ServerHost::new(
            None,
            Default::default(),
            Default::default(),
            rt.handle().clone(),
        );
        assert!(api_enabled(&empty));
        assert!(!api_allow_unauthenticated(&empty));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_llm_config_uses_profile_with_current_provider_credentials() {
        let state = json!({
            "llmConfig": { "provider": "openai", "apiKey": "global", "model": "gpt-global", "ollamaUrl": "", "customEndpoint": "", "maxContextSize": 1000 },
            "providerConfigs": {
                "deepseek": { "apiKey": "rotated", "model": "provider-model", "baseUrl": "https://new.example/v1", "apiMode": "chat_completions", "streamingEnabled": false, "customHeaders": { "X-Tenant-ID": "team-a" } }
            },
            "projectLlmOverrides": {
                "project-a": {
                    "enabled": true,
                    "presetId": "deepseek",
                    "model": "project-model",
                    "profile": { "provider": "custom", "model": "project-model", "ollamaUrl": "", "customEndpoint": "https://old.example/v1", "maxContextSize": 64000, "apiMode": "chat_completions" }
                }
            }
        });
        let config = project_llm_config(&state, "project-a").expect("project config");
        assert_eq!(config.provider, "custom");
        assert_eq!(config.api_key, "rotated");
        assert_eq!(config.model, "project-model");
        assert_eq!(config.custom_endpoint, "https://new.example/v1");
        assert_eq!(config.streaming_enabled, Some(false));
        assert_eq!(
            config.custom_headers.get("X-Tenant-ID").map(String::as_str),
            Some("team-a")
        );
    }

    #[test]
    fn project_llm_config_falls_back_for_disabled_or_legacy_override() {
        let state = json!({
            "llmConfig": { "provider": "openai", "apiKey": "global", "model": "gpt-global", "ollamaUrl": "", "customEndpoint": "", "maxContextSize": 1000 },
            "projectLlmOverrides": {
                "disabled": { "enabled": false, "presetId": "anthropic", "model": "claude" },
                "legacy": { "enabled": true, "presetId": "anthropic", "model": "claude" }
            }
        });
        assert_eq!(
            project_llm_config(&state, "disabled").unwrap().model,
            "gpt-global"
        );
        assert!(project_llm_config(&state, "legacy").is_none());
    }

    #[test]
    fn project_llm_config_falls_back_after_custom_preset_is_deleted() {
        let state = json!({
            "llmConfig": { "provider": "openai", "apiKey": "global", "model": "gpt-global", "ollamaUrl": "", "customEndpoint": "", "maxContextSize": 1000 },
            "providerConfigs": {
                "custom-deleted": { "apiKey": "stale-secret", "model": "stale-model", "baseUrl": "https://old.example/v1" }
            },
            "customLlmPresets": [],
            "projectLlmOverrides": {
                "project-b": {
                    "enabled": true,
                    "presetId": "custom-deleted",
                    "model": "stale-model",
                    "profile": { "provider": "custom", "model": "stale-model", "ollamaUrl": "", "customEndpoint": "https://old.example/v1", "maxContextSize": 64000 }
                }
            }
        });
        let config = project_llm_config(&state, "project-b").expect("global fallback");
        assert_eq!(config.model, "gpt-global");
        assert_eq!(config.api_key, "global");
    }

    #[test]
    fn project_llm_config_uses_registered_custom_preset() {
        let state = json!({
            "llmConfig": { "provider": "openai", "apiKey": "global", "model": "gpt-global", "ollamaUrl": "", "customEndpoint": "", "maxContextSize": 1000 },
            "customLlmPresets": [{ "id": "custom-live", "label": "Live" }],
            "providerConfigs": {
                "custom-live": { "apiKey": "live-secret", "model": "live-model", "baseUrl": "https://live.example/v1" }
            },
            "projectLlmOverrides": {
                "project-c": {
                    "enabled": true,
                    "presetId": "custom-live",
                    "model": "",
                    "profile": { "provider": "custom", "model": "old-model", "ollamaUrl": "", "customEndpoint": "https://old.example/v1", "maxContextSize": 64000 }
                }
            }
        });
        let config = project_llm_config(&state, "project-c").expect("custom config");
        assert_eq!(config.model, "live-model");
        assert_eq!(config.api_key, "live-secret");
        assert_eq!(config.custom_endpoint, "https://live.example/v1");
    }

    #[test]
    fn chat_session_endpoints_list_and_fetch_messages() {
        let root = test_project_dir();
        let root_str = root.to_string_lossy().to_string();
        let dir = root.join(".llm-wiki").join("agent-sessions");
        fs::create_dir_all(&dir).unwrap();
        let session = agent::session::AgentSession {
            session_id: "sess-1".to_string(),
            project_id: "proj".to_string(),
            messages: vec![
                agent::session::AgentSessionMessage {
                    role: "user".into(),
                    content: "q1".into(),
                    timestamp: 1,
                },
                agent::session::AgentSessionMessage {
                    role: "assistant".into(),
                    content: "a1".into(),
                    timestamp: 2,
                },
                agent::session::AgentSessionMessage {
                    role: "user".into(),
                    content: "q2".into(),
                    timestamp: 3,
                },
            ],
            updated_at: 42,
        };
        fs::write(
            dir.join("sess-1.json"),
            serde_json::to_string(&session).unwrap(),
        )
        .unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let host = ServerHost::new(
            None,
            Arc::new(agent::session::AgentSessionStore::default()),
            Arc::new(agent::cancel::AgentCancellationRegistry::default()),
            runtime.handle().clone(),
        );
        // Register the temp project so resolve_project can find it by path.
        project_registry::set_current_project_path(&root_str);

        let listed = handle_list_chat_sessions(&host, "current", "");
        assert_eq!(listed.status, 200);
        assert_eq!(listed.body["sessions"][0]["sessionId"], "sess-1");
        assert_eq!(listed.body["sessions"][0]["updatedAt"], 42);

        let limited = handle_list_chat_sessions(&host, "current", "limit=1");
        assert_eq!(limited.status, 200);
        assert_eq!(limited.body["sessions"].as_array().unwrap().len(), 1);

        let fetched = handle_get_chat_session(&host, "current", "sess-1", "limit=2");
        assert_eq!(fetched.status, 200);
        let messages = fetched.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);

        let missing = handle_get_chat_session(&host, "current", "", "");
        assert_eq!(missing.status, 400);
        let _ = fs::remove_dir_all(root);
    }
}
