use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tiny_http::{Header, Method, Response, Server, StatusCode};
use uuid::Uuid;


use crate::api_graph::{invalidate_graph_cache, load_graph, select_graph_page};
use crate::api_sse::{send_terminal_sse, sse_frame, try_send_sse, SseReader, SseSendResult};
#[cfg(test)]
use crate::api_graph::{build_graph, graph_aliases, resolve_link, ApiGraphEdge, ApiGraphNode};
#[cfg(test)]
use std::sync::Arc;
use crate::cors::{local_cors_headers, request_origin};
use crate::server_host::ServerHost;
use crate::{agent, commands, project_registry, server_bind};

mod params;
mod projects;
mod router;

use projects::{
    is_public_project_rel, is_text_content_rel, list_public_roots, list_tree, load_projects,
    normalize_path, read_project_id, resolve_project, safe_join, ReviewQuery, ReviewStatus,
};

mod respond;

use respond::{allow_request, ok, err, respond_error, respond_json, respond_options,
              should_rate_limit, ApiResponse};
use router::{cors_headers, process_request};
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use auth::constant_time_eq;
#[cfg(test)]
use projects::project_path_matches;
#[cfg(test)]
use router::{body_limit_for_request, is_agent_chat_request, is_token_required_request,
             wants_streaming_chat};

use params::{percent_decode, split_url};
pub(crate) use params::parse_query;

mod auth;

use auth::{api_allow_lan_access, api_allow_unauthenticated, api_auth_required, api_enabled,
          api_mcp_enabled, api_token, api_token_source, is_authorized, is_token_authorized};

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
            if should_rate_limit(&method, &url) && !allow_request() {
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
    let history_recorded = commands::file_history::record_file_version(&path, "api", "api.files.delete");
    if let Err(e) = fs::remove_file(&path) {
        return err(500, format!("Failed to delete file: {e}"));
    }

    let mut vectors_cleared = false;
    let lower = normalize_path(rel).to_lowercase();
    if lower.starts_with("wiki/") && lower.ends_with(".md") {
        let page_id = lower[5..lower.len() - 3].to_string();
        vectors_cleared = app
            .block_on(commands::vectorstore::vector_delete_page(
                project.path.clone(),
                page_id,
            ))
            .is_ok();
    }
    invalidate_graph_cache(&project.path);

    let mut body = json!({
        "ok": true,
        "projectId": project.id,
        "deleted": normalize_path(rel),
        "historyRecorded": history_recorded,
        "vectorsCleared": vectors_cleared,
    });
    if !history_recorded {
        body["hint"] = json!("file history is disabled for this project; PUT /history/settings {enabled:true} to make deletions restorable");
    }
    ok(body)
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
        return match list_public_roots(&project.path, recursive, max_files) {
            Ok(files) => ok(json!({
                "ok": true,
                "projectId": project.id,
                "root": "all",
                "files": files,
            })),
            Err(e) => err(if e.contains("exceeds") { 413 } else { 500 }, e),
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
            "truncated": false,
        })),
        Err(e) => err(if e.contains("exceeds") { 413 } else { 500 }, e),
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
        Err(e) => err(500, format!("Failed to read file: {}", e)),
    }
}

fn parse_review_query(query: &str) -> Result<ReviewQuery, String> {
    let params = parse_query(query);
    let status = match params
        .get("status")
        .map(|s| s.as_str())
        .unwrap_or("unresolved")
    {
        "unresolved" | "pending" => ReviewStatus::Unresolved,
        "resolved" => ReviewStatus::Resolved,
        "all" => ReviewStatus::All,
        value => {
            return Err(format!(
                "Invalid review status '{value}'. Expected unresolved, resolved, or all"
            ))
        }
    };
    let item_type = params
        .get("type")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_REVIEWS)
        .clamp(1, HARD_MAX_REVIEWS);

    Ok(ReviewQuery {
        status,
        item_type,
        limit,
    })
}

fn normalize_review_title(title: &str) -> String {
    let trimmed = title.trim_start();
    let lower = trimmed.to_lowercase();
    let mut rest = trimmed;
    for prefix in [
        "missing page",
        "missing-page",
        "missingpage",
        "duplicate page",
        "duplicate-page",
        "duplicatepage",
        "possible duplicate",
        "possible-duplicate",
        "possibleduplicate",
        "缺失页面",
        "缺少页面",
        "重复页面",
        "疑似重复",
    ] {
        if !lower.starts_with(prefix) {
            continue;
        }
        let suffix = &trimmed[prefix.len()..];
        let Some(delimiter) = suffix.chars().next() else {
            continue;
        };
        if delimiter == ':' || delimiter == '：' {
            rest = suffix[delimiter.len_utf8()..].trim_start();
            break;
        }
    }
    rest.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn review_id_for_parts(item_type: &str, title: &str) -> String {
    let key = format!("{item_type}::{}", normalize_review_title(title));
    let mut h: u32 = 0x811c9dc5;
    for unit in key.encode_utf16() {
        h ^= u32::from(unit);
        h = h.wrapping_mul(0x01000193);
    }
    format!("review-{h:08x}")
}

fn stable_review_id(item: &Value) -> Option<String> {
    let item_type = item.get("type").and_then(Value::as_str)?;
    let title = item.get("title").and_then(Value::as_str)?;
    Some(review_id_for_parts(item_type, title))
}

fn review_id_matches(item: &Value, requested_id: &str) -> bool {
    item.get("id").and_then(Value::as_str) == Some(requested_id)
        || stable_review_id(item).as_deref() == Some(requested_id)
}

fn load_review_items(project_path: &str, query: &ReviewQuery) -> Result<Vec<Value>, String> {
    let path = Path::new(project_path).join(".llm-wiki/review.json");
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("Failed to read review state: {err}")),
    };
    let parsed: Value =
        serde_json::from_str(&raw).map_err(|err| format!("Invalid review state JSON: {err}"))?;
    let items = parsed
        .as_array()
        .ok_or_else(|| "Invalid review state JSON: expected an array".to_string())?;

    let mut normalized: Vec<Value> = Vec::new();
    let mut index_by_id: BTreeMap<String, usize> = BTreeMap::new();
    for item in items {
        let sanitized = sanitize_review_item(item);
        let id = sanitized
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(id) = id {
            if let Some(existing_idx) = index_by_id.get(&id).copied() {
                merge_sanitized_review(&mut normalized[existing_idx], &sanitized);
                continue;
            }
            index_by_id.insert(id, normalized.len());
        }
        normalized.push(sanitized);
    }

    let mut reviews: Vec<Value> = Vec::new();
    for item in normalized {
        let resolved = item
            .get("resolved")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !query.status.matches(resolved) {
            continue;
        }
        if let Some(item_type) = &query.item_type {
            if item.get("type").and_then(Value::as_str) != Some(item_type.as_str()) {
                continue;
            }
        }
        if reviews.len() >= query.limit {
            break;
        }
        reviews.push(item);
    }

    Ok(reviews)
}

fn sanitize_review_item(item: &Value) -> Value {
    let mut out = Map::new();
    if let Some(id) = stable_review_id(item) {
        out.insert("id".to_string(), Value::String(id));
    } else {
        copy_string_field(item, &mut out, "id");
    }
    copy_string_field(item, &mut out, "type");
    copy_string_field(item, &mut out, "title");
    copy_string_field(item, &mut out, "description");
    copy_string_field(item, &mut out, "sourcePath");
    copy_string_array_field(item, &mut out, "affectedPages");
    copy_string_array_field(item, &mut out, "searchQueries");
    copy_review_options(item, &mut out);
    copy_bool_field(item, &mut out, "resolved");
    copy_string_field(item, &mut out, "resolvedAction");
    copy_number_field(item, &mut out, "createdAt");
    Value::Object(out)
}

fn merge_sanitized_review(existing: &mut Value, incoming: &Value) {
    let Some(existing) = existing.as_object_mut() else {
        return;
    };
    let Some(incoming) = incoming.as_object() else {
        return;
    };

    let resolved = existing
        .get("resolved")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || incoming
            .get("resolved")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    existing.insert("resolved".to_string(), Value::Bool(resolved));

    if resolved && !existing.contains_key("resolvedAction") {
        if let Some(action) = incoming.get("resolvedAction").and_then(Value::as_str) {
            existing.insert(
                "resolvedAction".to_string(),
                Value::String(action.to_string()),
            );
        }
    }

    for key in ["description", "sourcePath"] {
        let existing_empty = existing
            .get(key)
            .and_then(Value::as_str)
            .map(str::is_empty)
            .unwrap_or(true);
        if existing_empty {
            if let Some(value) = incoming.get(key).and_then(Value::as_str) {
                existing.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
    }

    merge_string_array_field(existing, incoming, "affectedPages");
    merge_string_array_field(existing, incoming, "searchQueries");
    merge_review_options_field(existing, incoming);

    if let Some(incoming_created) = incoming.get("createdAt").and_then(Value::as_f64) {
        let existing_created = existing
            .get("createdAt")
            .and_then(Value::as_f64)
            .unwrap_or(incoming_created);
        existing.insert(
            "createdAt".to_string(),
            json!(existing_created.min(incoming_created)),
        );
    }
}

fn merge_string_array_field(
    existing: &mut Map<String, Value>,
    incoming: &Map<String, Value>,
    key: &str,
) {
    let mut values = existing
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for value in incoming
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !values.iter().any(|existing| existing == value) {
            values.push(value.to_string());
        }
    }
    if !values.is_empty() {
        existing.insert(
            key.to_string(),
            Value::Array(values.into_iter().map(Value::String).collect()),
        );
    }
}

fn merge_review_options_field(existing: &mut Map<String, Value>, incoming: &Map<String, Value>) {
    let mut options = existing
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for option in incoming
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let action = option.get("action").and_then(Value::as_str);
        let already_present = action.is_some_and(|action| {
            options
                .iter()
                .any(|existing| existing.get("action").and_then(Value::as_str) == Some(action))
        });
        if !already_present {
            options.push(option.clone());
        }
    }
    if !options.is_empty() {
        existing.insert("options".to_string(), Value::Array(options));
    }
}

fn copy_string_field(item: &Value, out: &mut Map<String, Value>, key: &str) {
    if let Some(value) = item.get(key).and_then(Value::as_str) {
        out.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn copy_bool_field(item: &Value, out: &mut Map<String, Value>, key: &str) {
    if let Some(value) = item.get(key).and_then(Value::as_bool) {
        out.insert(key.to_string(), Value::Bool(value));
    }
}

fn copy_number_field(item: &Value, out: &mut Map<String, Value>, key: &str) {
    if let Some(value) = item.get(key).and_then(Value::as_f64) {
        if value.is_finite() {
            out.insert(key.to_string(), json!(value));
        }
    }
}

fn copy_string_array_field(item: &Value, out: &mut Map<String, Value>, key: &str) {
    let Some(values) = item.get(key).and_then(Value::as_array) else {
        return;
    };
    let strings = values
        .iter()
        .filter_map(Value::as_str)
        .map(|value| Value::String(value.to_string()))
        .collect::<Vec<_>>();
    out.insert(key.to_string(), Value::Array(strings));
}

fn copy_review_options(item: &Value, out: &mut Map<String, Value>) {
    let Some(values) = item.get("options").and_then(Value::as_array) else {
        return;
    };
    let options = values
        .iter()
        .filter_map(|option| {
            let option = option.as_object()?;
            let mut sanitized = Map::new();
            if let Some(label) = option.get("label").and_then(Value::as_str) {
                sanitized.insert("label".to_string(), Value::String(label.to_string()));
            }
            if let Some(action) = option.get("action").and_then(Value::as_str) {
                sanitized.insert("action".to_string(), Value::String(action.to_string()));
            }
            if sanitized.is_empty() {
                None
            } else {
                Some(Value::Object(sanitized))
            }
        })
        .collect::<Vec<_>>();
    out.insert("options".to_string(), Value::Array(options));
}

fn handle_reviews(app: &ServerHost, project_id: &str, query: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let query = match parse_review_query(query) {
        Ok(query) => query,
        Err(e) => return err(400, e),
    };
    match load_review_items(&project.path, &query) {
        Ok(reviews) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "status": query.status.as_str(),
            "count": reviews.len(),
            "reviews": reviews,
        })),
        Err(e) => { eprintln!("[api] 500: {e}"); err(500, e) },
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PatchReviewRequest {
    /// Target resolved state. Defaults to `true` (the common case:
    /// resolve). Pass `false` to reopen a resolved item.
    resolved: Option<bool>,
    /// Optional human-readable action label stored on the item
    /// (e.g. "Skip", "Created page"). Mark-only — the API never
    /// replicates the WebView's side effects (page creation, etc).
    action: Option<String>,
}

/// `PATCH /projects/{id}/reviews/{reviewId}` — partial update of a
/// single review item's resolved state. Body `{ resolved?, action? }`;
/// an empty body resolves the item (resolved defaults to true).
fn handle_patch_review(
    app: &ServerHost,
    project_id: &str,
    review_id: &str,
    body: &str,
) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let req = if body.trim().is_empty() {
        PatchReviewRequest {
            resolved: None,
            action: None,
        }
    } else {
        match serde_json::from_str::<PatchReviewRequest>(body) {
            Ok(req) => req,
            Err(e) => return err(400, format!("Invalid request body: {e}")),
        }
    };
    let resolved = req.resolved.unwrap_or(true);
    match patch_review_item(&project.path, review_id, resolved, req.action.as_deref()) {
        Ok(true) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "reviewId": review_id,
            "resolved": resolved,
        })),
        Ok(false) => err(404, format!("Review item '{review_id}' not found")),
        Err(e) => { eprintln!("[api] 500: {e}"); err(500, e) },
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BulkResolveRequest {
    /// Review item ids to resolve. Required, non-empty.
    #[serde(alias = "reviewIds")]
    ids: Vec<String>,
    /// Optional label applied to every resolved item.
    action: Option<String>,
}

fn handle_delete_review(app: &ServerHost, project_id: &str, review_id: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    match delete_review_item(&project.path, review_id) {
        Ok(true) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "reviewId": review_id,
            "deleted": true,
        })),
        Ok(false) => err(404, format!("Review item '{review_id}' not found")),
        Err(e) => { eprintln!("[api] 500: {e}"); err(500, e) },
    }
}

fn delete_review_item(project_path: &str, review_id: &str) -> Result<bool, String> {
    let path = Path::new(project_path).join(".llm-wiki/review.json");
    let mut parsed = match read_raw_review_array(&path)? {
        Some(parsed) => parsed,
        None => return Ok(false),
    };
    let items = parsed
        .as_array_mut()
        .ok_or_else(|| "Invalid review state JSON: expected an array".to_string())?;
    let original_len = items.len();
    items.retain(|item| !review_id_matches(item, review_id));
    if items.len() == original_len {
        return Ok(false);
    }
    write_raw_review_array(&path, &parsed)?;
    Ok(true)
}

/// `POST /projects/{id}/reviews/resolve` — bulk-resolve many review
/// items in one request. Body `{ ids, action? }`. Partial success is
/// normal, so this returns 200 with `{ resolved, notFound, count }`
/// rather than 404 — 404 is reserved for the single-item PATCH where
/// one unknown id is the entire request.
fn handle_bulk_resolve_reviews(app: &ServerHost, project_id: &str, body: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let req = match serde_json::from_str::<BulkResolveRequest>(body) {
        Ok(req) => req,
        Err(e) => return err(400, format!("Invalid request body: {e}")),
    };
    if req.ids.is_empty() {
        return err(400, "ids must be a non-empty array".to_string());
    }
    match resolve_review_items(&project.path, &req.ids, req.action.as_deref()) {
        Ok((resolved, not_found)) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "resolved": resolved,
            "notFound": not_found,
            "count": resolved.len(),
        })),
        Err(e) => { eprintln!("[api] 500: {e}"); err(500, e) },
    }
}

/// Set one review item's resolved state in `.llm-wiki/review.json`.
///
/// Operates on the RAW parsed array (not `load_review_items`, which
/// sanitizes — reusing it would strip fields like `internalSecret` and
/// silently corrupt the file on write-back). Sets `resolved` and, when
/// provided, `resolvedAction`. Returns Ok(false) if no item with the
/// given id exists (caller maps to 404).
fn patch_review_item(
    project_path: &str,
    review_id: &str,
    resolved: bool,
    action: Option<&str>,
) -> Result<bool, String> {
    let path = Path::new(project_path).join(".llm-wiki/review.json");
    let mut parsed = match read_raw_review_array(&path)? {
        Some(parsed) => parsed,
        None => return Ok(false),
    };
    let items = parsed
        .as_array_mut()
        .ok_or_else(|| "Invalid review state JSON: expected an array".to_string())?;

    let mut found = false;
    for item in items.iter_mut() {
        if !review_id_matches(item, review_id) {
            continue;
        }
        apply_resolution(item, resolved, action);
        if let Some(stable_id) = stable_review_id(item) {
            if let Some(obj) = item.as_object_mut() {
                obj.insert("id".to_string(), Value::String(stable_id));
            }
        }
        found = true;
    }

    if !found {
        return Ok(false);
    }
    write_raw_review_array(&path, &parsed)?;
    Ok(true)
}

/// Bulk version of `patch_review_item`: reads `review.json` ONCE,
/// resolves every matching id in the raw array, writes ONCE. Looping
/// the single-item helper would be N read-parse-write cycles with a
/// race window per write. Returns `(resolved_ids, not_found_ids)`,
/// both in the caller's input order.
fn resolve_review_items(
    project_path: &str,
    ids: &[String],
    action: Option<&str>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let path = Path::new(project_path).join(".llm-wiki/review.json");
    let mut parsed = match read_raw_review_array(&path)? {
        Some(parsed) => parsed,
        // No review file → nothing exists, so every id is "not found".
        None => return Ok((Vec::new(), ids.to_vec())),
    };
    let items = parsed
        .as_array_mut()
        .ok_or_else(|| "Invalid review state JSON: expected an array".to_string())?;

    let mut found: BTreeSet<String> = BTreeSet::new();
    for item in items.iter_mut() {
        let raw_id = item.get("id").and_then(Value::as_str);
        let stable_id = stable_review_id(item);
        let matched_request = ids
            .iter()
            .find(|id| raw_id == Some(id.as_str()) || stable_id.as_deref() == Some(id.as_str()));
        let Some(requested_id) = matched_request else {
            continue;
        };
        apply_resolution(item, true, action);
        if let Some(stable_id) = stable_id {
            if let Some(obj) = item.as_object_mut() {
                obj.insert("id".to_string(), Value::String(stable_id));
            }
        }
        found.insert(requested_id.clone());
    }

    if !found.is_empty() {
        write_raw_review_array(&path, &parsed)?;
    }

    // Preserve the caller's input order; dedupe is implicit via `found`.
    let resolved: Vec<String> = ids
        .iter()
        .filter(|id| found.contains(*id))
        .cloned()
        .collect();
    let not_found: Vec<String> = ids
        .iter()
        .filter(|id| !found.contains(*id))
        .cloned()
        .collect();
    Ok((resolved, not_found))
}

/// Set `resolved` / `resolvedAction` on a raw review item value.
fn apply_resolution(item: &mut Value, resolved: bool, action: Option<&str>) {
    if let Some(obj) = item.as_object_mut() {
        obj.insert("resolved".to_string(), Value::Bool(resolved));
        if !resolved {
            obj.remove("resolvedAction");
        } else if let Some(action) = action {
            obj.insert(
                "resolvedAction".to_string(),
                Value::String(action.to_string()),
            );
        }
    }
}

/// Read `.llm-wiki/review.json` as a raw JSON value. Returns Ok(None)
/// when the file doesn't exist (callers treat that as "no items").
fn read_raw_review_array(path: &Path) -> Result<Option<Value>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("Failed to read review state: {err}")),
    };
    let parsed: Value =
        serde_json::from_str(&raw).map_err(|err| format!("Invalid review state JSON: {err}"))?;
    Ok(Some(parsed))
}

fn write_raw_review_array(path: &Path, parsed: &Value) -> Result<(), String> {
    let serialized = serde_json::to_string_pretty(parsed)
        .map_err(|err| format!("Failed to serialize review state: {err}"))?;
    fs::write(path, serialized).map_err(|err| format!("Failed to write review state: {err}"))
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
            let lower = error.to_ascii_lowercase();
            let status = if lower.contains("without allowoverwrite") {
                409
            } else if lower.contains("path")
                || lower.contains("markdown file")
                || lower.contains("too large")
            {
                400
            } else {
                500
            };
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
    let query_embedding =
        match app.block_on(commands::search::resolve_query_embedding(
            &query,
            req.query_embedding,
            load_embedding_config(app),
        )) {
            Ok(embedding) => embedding,
            Err(e) => return err(400, e),
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
            "results": search.results,
        })),
        Err(e) => { eprintln!("[api] 500: {e}"); err(500, e) },
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
    let cancellation = app
        .cancellations()
        .start(&project.id, &session_id, &run_id);
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
        Err(e) if e == "message is required" => err(400, e),
        Err(e) if e == "Agent turn cancelled" => err(499, e),
        Err(e) => err(502, e),
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
    let _ = try_send_sse(
        &sender,
        sse_frame(
            "meta",
            &json!({
                "projectId": prepared.project.id,
                "sessionId": prepared.session_id,
                "runId": prepared.run_id,
            }),
        ),
    );

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
                    SseSendResult::Sent | SseSendResult::Full => {}
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
                // Intermediate events may be dropped for a temporarily slow
                // client. The terminal `done` frame still carries the complete
                // aggregate response, so correctness is preserved.
                SseSendResult::Sent | SseSendResult::Full => {}
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
        task_app
            .cancellations()
            .finish(&prepared.project.id, &prepared.session_id, &prepared.run_id);
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
    for header in cors_headers(origin) {
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
/// (by `updatedAt`). Powers the web client's session switcher.
fn handle_list_chat_sessions(app: &ServerHost, project_id: &str) -> ApiResponse {
    let project = match resolve_project(app, project_id) {
        Ok(project) => project,
        Err(e) => return err(404, e),
    };
    let mut sessions = app.sessions().list_sessions(&project.path);
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
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
        Err(e) => { eprintln!("[api] 500: {e}"); err(500, e) },
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
        Err(e) => { eprintln!("[api] 500: {e}"); err(500, e) },
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
fn persist_project_registration(app: &ServerHost, id: &str, name: &str, path: &str) {
    let Some(store) = app.app_state_path() else {
        return;
    };
    // Atomic write-then-rename, mirroring handle_put_settings.
    let existing = match fs::read_to_string(&store) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(v) if v.is_object() => v,
            _ => return,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(_) => return,
    };
    let mut merged = existing;
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
    let tmp = store.with_extension("json.tmp");
    if fs::write(&tmp, serde_json::to_string_pretty(&merged).unwrap_or_default()).is_ok() {
        let _ = fs::rename(&tmp, &store);
    }
    invalidate_config_cache();
}

const OPENAPI_JSON: &str = include_str!("../../openapi.json");

fn openapi_document() -> Value {
    static PARSED: OnceLock<Value> = OnceLock::new();
    PARSED
        .get_or_init(|| serde_json::from_str(OPENAPI_JSON).unwrap_or(Value::Null))
        .clone()
}

/// GET /api/v1/settings — returns the merged app-state configuration
/// (llmConfig, embeddingConfig, searchApiConfig, apiConfig, ...). This is
/// the owner's endpoint: values are NOT masked, the bearer token gates it.
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
        Err(e) => err(500, format!("failed to read app-state.json: {e}")),
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
    // A missing file is a legitimate first run (empty base); anything
    // else — IO failure, corrupt JSON — must REFUSE the write instead of
    // silently treating the config as empty and wiping token + keys.
    let existing_raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return err(500, format!("app-state.json is unreadable ({}); refusing to overwrite", e))
        }
    };
    let mut merged: Value = if existing_raw.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str::<Value>(&existing_raw) {
            Ok(v) if v.is_object() => v,
            _ => {
                return err(500,
                    "app-state.json is corrupt or not a JSON object; refusing to overwrite — fix or move the file first")
            }
        }
    };
    deep_merge_masked(&mut merged, &update);
    // Atomic write: a torn app-state.json loses the API token and every
    // provider key on restart, so write-then-rename, never in-place.
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = fs::write(&tmp, serde_json::to_string_pretty(&merged).unwrap_or_default()) {
        let _ = fs::remove_file(&tmp);
        return err(500, format!("Failed to write app-state.json: {e}"));
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return err(500, format!("Failed to replace app-state.json: {e}"));
    }
    invalidate_config_cache();
    ok(mask_secrets(&mut merged).clone())
}

const SECRET_MASK: &str = "********";

/// Recursively mask values under credential-bearing keys (`apiKey`,
/// `token`, `accessToken`) for GET responses.
fn mask_secrets(value: &mut Value) -> &mut Value {
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                let lower = key.to_lowercase();
                if lower == "apikey" || lower == "token" || lower == "accesstoken" {
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

/// Deep merge for PUT /settings semantics:
/// - object + object merges recursively
/// - `null` deletes the key
/// - the secret mask `"********"` keeps the existing value (round-trip
///   safe: masked GET output can be PUT back without leaking or losing keys)
fn deep_merge_masked(existing: &mut Value, update: &Value) {
    if let (Some(existing_map), Some(update_map)) =
        (existing.as_object_mut(), update.as_object())
    {
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
                // keep whatever already exists; if nothing exists, record
                // the mask so the operator sees it is unset-by-design
                existing_map
                    .entry(key.clone())
                    .or_insert_with(|| Value::String(SECRET_MASK.to_string()));
            } else {
                existing_map.insert(key.clone(), update_value.clone());
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
        return err(400, "invalid name: use a single path component without '..' or leading dots");
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
    project_registry::register_project(&req.name, &project.path);
    project_registry::set_current_project_path(&project.path);
    // Ensure the project has a stable identity BEFORE registering it in
    // app-state — otherwise the registry key falls back to the path and
    // clients can no longer address the project by the returned id.
    let project_id = match read_project_id(&project.path) {
        Some(id) => id,
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            let meta_dir = std::path::Path::new(&project.path).join(".llm-wiki");
            if fs::create_dir_all(&meta_dir).is_ok() {
                let payload = json!({ "id": id, "name": project.name });
                let _ = fs::write(
                    meta_dir.join("project.json"),
                    serde_json::to_string_pretty(&payload).unwrap_or_default(),
                );
            }
            id
        }
    };
    persist_project_registration(app, &project_id, name, &project.path);
    invalidate_graph_cache(&project.path);
    ok(json!({
        "ok": true,
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
    let display_name = std::path::Path::new(&req.path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Project");
    project_registry::register_project(display_name, &req.path);
    project_registry::set_current_project_path(&req.path);
    let project_id = read_project_id(&req.path).unwrap_or_else(|| req.path.clone());
    persist_project_registration(app, &project_id, display_name, &req.path);
    invalidate_config_cache();
    ok(json!({ "ok": true, "current": req.path.replace('\\', "/") }))
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
    let abs_path = match safe_join(&project.path, rel) {
        Ok(abs_path) => abs_path.to_string_lossy().into_owned(),
        Err(e) => return err(400, e),
    };
    if !is_public_project_rel(rel) {
        return err(403, "Path is not exposed by the local API");
    }
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
        Err(e) => { eprintln!("[api] 500: {e}"); err(500, e) },
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
        Err(e) => { eprintln!("[api] 500: {e}"); err(500, e) },
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
        Err(e) => { eprintln!("[api] 500: {e}"); err(500, e) },
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
        let parsed = parse_query("path=wiki%2Fhello+world.md&token=a%2Bb");
        assert_eq!(parsed.get("path").unwrap(), "wiki/hello world.md");
        assert_eq!(parsed.get("token").unwrap(), "a+b");
    }

    #[test]
    fn snippet_handles_unicode_boundaries() {
        let content = "前言。这里是关于知识图谱过滤的中文内容。后续说明。";
        let snippet = commands::search::build_snippet(content, "知识图谱");
        assert!(snippet.contains("知识图谱"));
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
    fn chat_routes_are_recognized_as_agent_requests() {
        assert!(is_agent_chat_request(
            &Method::Post,
            "/api/v1/projects/current/chat"
        ));
        assert!(is_agent_chat_request(
            &Method::Post,
            "/api/v1/projects/current/chat/session-1/cancel"
        ));
        assert!(!is_agent_chat_request(
            &Method::Post,
            "/api/v1/projects/current/search"
        ));
        assert!(!is_agent_chat_request(
            &Method::Get,
            "/api/v1/projects/current/chat"
        ));
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
        assert!(!is_token_required_request(
            &Method::Post,
            "/api/v1/projects/current/search"
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
        let dir = std::env::temp_dir().join(format!(
            "llm-wiki-apicfg-test-{}",
            std::process::id()
        ));
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
        let empty = ServerHost::new(None, Default::default(), Default::default(), rt.handle().clone());
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
                agent::session::AgentSessionMessage { role: "user".into(), content: "q1".into(), timestamp: 1 },
                agent::session::AgentSessionMessage { role: "assistant".into(), content: "a1".into(), timestamp: 2 },
                agent::session::AgentSessionMessage { role: "user".into(), content: "q2".into(), timestamp: 3 },
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

        let listed = handle_list_chat_sessions(&host, "current");
        assert_eq!(listed.status, 200);
        assert_eq!(listed.body["sessions"][0]["sessionId"], "sess-1");
        assert_eq!(listed.body["sessions"][0]["updatedAt"], 42);

        let fetched = handle_get_chat_session(&host, "current", "sess-1", "limit=2");
        assert_eq!(fetched.status, 200);
        let messages = fetched.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);

        let missing = handle_get_chat_session(&host, "current", "", "");
        assert_eq!(missing.status, 400);
        let _ = fs::remove_dir_all(root);
    }
}
