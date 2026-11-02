//! HTTP entry pipeline: raw request -> route dispatch.

use tiny_http::Method;

use super::*;

pub(super) fn process_request(app: ServerHost, mut request: tiny_http::Request) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let origin = request_origin(&request);
    if method == Method::Options {
        respond_options(request, origin.as_deref());
        return;
    }

    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|header| {
            (
                header.field.as_str().to_ascii_lowercase().to_string(),
                header.value.as_str().to_string(),
            )
        })
        .collect();

    let body = match read_body(&mut request, body_limit_for_request(&method, &url)) {
        Ok(body) => body,
        Err(err) => {
            respond_error(request, 400, &err, origin.as_deref());
            return;
        }
    };

    let (path, query) = split_url(&url);
    // Access log: mutating requests are always logged so deletes, config
    // writes and history restores can be reconstructed from the console.
    if matches!(
        method,
        Method::Post | Method::Put | Method::Patch | Method::Delete
    ) {
        eprintln!("[api] {} {}", method, url);
    }
    if method == Method::Get && (path == "/docs" || path == "/docs/") {
        // Human-readable API documentation. Same authorization semantics
        // as every other read endpoint: allowUnauthenticated opens it, a
        // configured token gates it. Browsers authenticate via the query
        // token since they cannot set Authorization headers.
        // Honors the api_enabled kill switch like the rest of the API.
        if !api_enabled(&app) {
            respond_error(request, 503, "API server is disabled", origin.as_deref());
            return;
        }
        if !is_authorized(&app, query, &headers) {
            respond_error(
                request,
                401,
                "Unauthorized: append ?token=<your token> to the URL (or enable allowUnauthenticated)",
                origin.as_deref(),
            );
            return;
        }
        let html = crate::api_docs::render(&openapi_document());
        let mut response = tiny_http::Response::from_string(html);
        response.add_header(
            tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap(),
        );
        for header in cors_headers(origin.as_deref()) {
            response.add_header(header);
        }
        let _ = request.respond(response);
        return;
    }
    if wants_streaming_chat(&method, &path, &body, &headers) {
        if !api_enabled(&app) {
            respond_error(
                request,
                503,
                "API server is disabled in Settings → API Server",
                origin.as_deref(),
            );
            return;
        }
        // Agent chat remains token-protected even when read-oriented API
        // endpoints are configured for unauthenticated local access.
        if !is_token_authorized(&app, query, &headers) {
            respond_error(request, 401, "Unauthorized", origin.as_deref());
            return;
        }
        let Some(project_id) = chat_project_id(&method, &path) else {
            respond_error(request, 404, "Not found", origin.as_deref());
            return;
        };
        let Some(stream_slot) = try_acquire_chat_stream_slot() else {
            respond_error(
                request,
                503,
                "Too many concurrent Agent chat streams",
                origin.as_deref(),
            );
            return;
        };
        respond_chat_sse(
            request,
            app,
            project_id,
            &body,
            origin.as_deref(),
            stream_slot,
        );
        return;
    }

    let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_request(&app, &method, &url, &body, &headers)
    }))
    .unwrap_or_else(|payload| {
        eprintln!("[API Server] request panicked: {payload:?}");
        err(500, "Internal API server error")
    });
    respond_json(request, response.status, response.body, origin.as_deref());
}




fn handle_request(
    app: &ServerHost,
    method: &Method,
    url: &str,
    body: &str,
    headers: &[(String, String)],
) -> ApiResponse {
    let (path, query) = split_url(url);
    if path == "/health" || path == format!("{API_PREFIX}/health") {
        // /health stays reachable even when the user has disabled the
        // API in Settings — the desktop UI uses it to render the
        // "Enabled / disabled / port_conflict" line, and curl-from-
        // terminal users need a way to confirm the server is alive
        // before they go hunting for why other endpoints 503.
        return ok(json!({
            "ok": true,
            "status": get_api_status(),
            "version": env!("CARGO_PKG_VERSION"),
            "authRequired": api_auth_required(app),
            "authConfigured": api_token(app).is_some(),
            "tokenSource": api_token_source(app),
            "enabled": api_enabled(app),
            "mcpEnabled": api_mcp_enabled(app),
            "allowUnauthenticated": api_allow_unauthenticated(app),
            "allowLanAccess": api_allow_lan_access(app),
            "docs": "/docs?token=<your token>",
            "agent": {
                "chat": true,
                "streaming": true,
                "streamProtocol": "sse",
            },
        }));
    }
    if method == &Method::Get
        && (path == "/openapi.json" || path == "/" || path == format!("{API_PREFIX}/openapi.json"))
    {
        return ok(openapi_document());
    }
    if !path.starts_with(API_PREFIX) {
        return err(404, "Not found");
    }
    if !api_enabled(app) {
        // Kill-switch path: token may be configured and valid, but the
        // user toggled the API off in Settings → API Server. 503 is
        // the right code semantically ("temporarily unavailable")
        // and tells well-behaved clients to back off rather than
        // retry instantly the way 401 would.
        return err(503, "API server is disabled in Settings → API Server");
    }
    if is_token_required_request(&method, &path) && !is_token_authorized(app, query, headers) {
        return err(401, "Unauthorized");
    }
    if !is_authorized(app, query, headers) {
        return err(401, "Unauthorized");
    }
    if !matches!(
        method,
        &Method::Get | &Method::Post | &Method::Put | &Method::Patch | &Method::Delete
    ) {
        return err(405, "Method not allowed");
    }

    let Some(parts) = api_path_parts(&path) else {
        return err(404, "Not found");
    };

    match (method, parts.as_slice()) {
        (&Method::Get, ["projects"]) => handle_projects(app),
        (&Method::Post, ["projects", "create"]) => handle_create_project(app, body),
        (&Method::Post, ["projects", "current"]) => handle_set_current_project(app, body),
        (&Method::Get, ["settings"]) => {
            // Config carries every provider key: it always requires a real
            // token, even when allowUnauthenticated opens read routes.
            if !is_token_authorized(app, query, headers) {
                return err(401, "settings require a token (never unauthenticated)");
            }
            handle_get_settings(app)
        }
        (&Method::Put, ["settings"]) => {
            if !is_token_authorized(app, query, headers) {
                return err(401, "settings require a token (never unauthenticated)");
            }
            handle_put_settings(app, body)
        }
        (&Method::Get, ["projects", project_id, "files"]) => handle_files(app, project_id, query),
        (&Method::Get, ["projects", project_id, "files", "content"]) => {
            handle_file_content(app, project_id, query)
        }
        (&Method::Delete, ["projects", project_id, "files", "content"]) => {
            handle_delete_file(app, project_id, query)
        }
        (&Method::Get, ["projects", project_id, "reviews"]) => {
            handle_reviews(app, project_id, query)
        }
        (&Method::Post, ["projects", project_id, "reviews", "resolve"]) => {
            handle_bulk_resolve_reviews(app, project_id, body)
        }
        (&Method::Patch, ["projects", project_id, "reviews", review_id]) => {
            handle_patch_review(app, project_id, review_id, body)
        }
        (&Method::Delete, ["projects", project_id, "reviews", review_id]) => {
            handle_delete_review(app, project_id, review_id)
        }
        (&Method::Post, ["projects", project_id, "search"]) => handle_search(app, project_id, body),
        (&Method::Get, ["projects", project_id, "graph"]) => handle_graph(app, project_id, query),
        (&Method::Post, ["projects", project_id, "sources", "rescan"]) => {
            handle_rescan(app, project_id)
        }
        (&Method::Post, ["projects", project_id, "pages", "embed"]) => {
            handle_embed_page(app, project_id, body)
        }
        (&Method::Post, ["projects", project_id, "pages", "write"]) => {
            handle_write_page(app, project_id, body)
        }
        (&Method::Post, ["projects", project_id, "chat"]) => handle_chat(app, project_id, body),
        (&Method::Get, ["projects", project_id, "history"]) => {
            handle_list_file_history(app, project_id, query)
        }
        (&Method::Post, ["projects", project_id, "history", "restore"]) => {
            handle_restore_file_history(app, project_id, body)
        }
        (&Method::Get, ["projects", project_id, "history", "stats"]) => {
            handle_file_history_stats(app, project_id)
        }
        (&Method::Get, ["projects", project_id, "history", "settings"]) => {
            handle_get_history_settings(app, project_id)
        }
        (&Method::Put, ["projects", project_id, "history", "settings"]) => {
            handle_put_history_settings(app, project_id, body)
        }
        (&Method::Post, ["projects", project_id, "maintenance", "rebuild-index"]) => {
            handle_rebuild_index(app, project_id)
        }
        (&Method::Post, ["projects", project_id, "chat", session_id, "cancel"]) => {
            handle_cancel_chat(app, project_id, session_id)
        }
        (&Method::Get, ["projects", project_id, "chat", "sessions"]) => {
            handle_list_chat_sessions(app, project_id)
        }
        (&Method::Get, ["projects", project_id, "chat", "sessions", session_id]) => {
            handle_get_chat_session(app, project_id, session_id, query)
        }
        _ => err(404, "Not found"),
    }
}



/// Chat (agent) routes always require a token: they execute tools and can
/// invoke shell commands, so they are never anonymous.
fn is_agent_chat_request(method: &Method, path: &str) -> bool {
    if method != &Method::Post {
        return false;
    }
    let Some(parts) = api_path_parts(path) else {
        return false;
    };
    matches!(
        parts.as_slice(),
        ["projects", _, "chat"] | ["projects", _, "chat", _, "cancel"]
    )
}

pub(super) fn is_token_required_request(method: &Method, path: &str) -> bool {
    if is_agent_chat_request(method, path) {
        return true;
    }
    let Some(parts) = api_path_parts(path) else {
        return false;
    };
    // allowUnauthenticated opens READS only — search/graph/files listing
    // and file content are governed by is_authorized instead. Every
    // mutating verb requires a token even when the switch is on, otherwise
    // a LAN peer could delete files or rewrite the config anonymously.
    !matches!(
        (method, parts.as_slice()),
        (&Method::Get, _)
            | (&Method::Post, ["projects", _, "search"])
    )
}

fn chat_project_id<'a>(method: &Method, path: &'a str) -> Option<&'a str> {
    if method != &Method::Post {
        return None;
    }
    let parts = api_path_parts(path)?;
    match parts.as_slice() {
        ["projects", project_id, "chat"] => Some(*project_id),
        _ => None,
    }
}

fn api_path_parts(path: &str) -> Option<Vec<&str>> {
    let remainder = path.strip_prefix(API_PREFIX)?;
    if !remainder.is_empty() && !remainder.starts_with('/') {
        return None;
    }
    Some(
        remainder
            .trim_start_matches('/')
            .split('/')
            .filter(|part| !part.is_empty())
            .collect(),
    )
}

pub(super) fn wants_streaming_chat(
    method: &Method,
    path: &str,
    body: &str,
    headers: &[(String, String)],
) -> bool {
    if chat_project_id(method, path).is_none() {
        return false;
    }
    let accepts_sse = headers.iter().any(|(key, value)| {
        key == "accept"
            && value
                .split(',')
                .filter_map(|part| part.trim().split(';').next())
                .any(|media_type| media_type.trim().eq_ignore_ascii_case("text/event-stream"))
    });
    match serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("stream").and_then(Value::as_bool))
    {
        Some(stream) => stream,
        None => accepts_sse,
    }
}

pub(super) fn body_limit_for_request(method: &Method, url: &str) -> usize {
    let (path, _) = split_url(url);
    let parts = api_path_parts(&path);
    if method != &Method::Post {
        return MAX_BODY_BYTES;
    }
    match parts.as_deref() {
        Some(["projects", _, "chat"]) => MAX_CHAT_BODY_BYTES,
        Some(["projects", _, "pages", "write"]) => MAX_PAGE_WRITE_BODY_BYTES,
        _ => MAX_BODY_BYTES,
    }
}

fn read_body(request: &mut tiny_http::Request, max_body_bytes: usize) -> Result<String, String> {
    let mut limited = request.as_reader().take(max_body_bytes as u64 + 1);
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Failed to read body: {e}"))?;
    if bytes.len() > max_body_bytes {
        return Err("Request body too large".to_string());
    }
    String::from_utf8(bytes).map_err(|_| "Request body must be UTF-8".to_string())
}




pub(super) fn cors_headers(origin: Option<&str>) -> Vec<Header> {
    local_cors_headers(origin, "Content-Type, Authorization, X-LLM-Wiki-Token")
}


