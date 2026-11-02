"""Third-audit remaining fixes:
A. api_server: enforce token on ALL mutating requests (allowUnauthenticated
   opens reads only); /docs honors the api_enabled kill switch (503)
B. reviews.rs: REVIEW_LOCK serializes read-modify-write; atomic tmp+rename
C. session.rs: corrupt-load warning already applied (verify only)
D. file_history.rs: corrupt store -> Err; missing -> empty list
E. headless.rs: allowUnauthenticated-aware warning (already applied; verify)
F. lib.rs / project_registry.rs: stale doc comments
G. access log for mutating requests
"""
import io

applied = []


def patch(path, old, new, label):
    s = io.open(path, encoding="utf-8").read()
    assert old in s, "anchor missing: " + label
    s = s.replace(old, new, 1)
    io.open(path, "w", encoding="utf-8", newline="\n").write(s)
    applied.append(label)


ROUTER = r"D:\llm_wiki\src-tauri\src\api_server\router.rs"
MOD = r"D:\llm_wiki\src-tauri\src\api_server\mod.rs"
REVIEWS = r"D:\llm_wiki\src-tauri\src\api_server\reviews.rs"

# ---- D/F: token required on all mutating methods ----
patch(
    ROUTER,
    """pub(super) fn is_token_required_request(method: &Method, path: &str) -> bool {
    if is_agent_chat_request(method, path) {
        return true;
    }
    let Some(parts) = api_path_parts(path) else {
        return false;
    };
    method == &Method::Post
        && matches!(
            parts.as_slice(),
            ["projects", _, "pages", "embed"] | ["projects", _, "pages", "write"]
        )
}""",
    """pub(super) fn is_token_required_request(method: &Method, _path: &str) -> bool {
    // allowUnauthenticated opens READS only. Every mutating verb — chat,
    // page write/embed, settings, file delete, project create, history
    // restore — requires a token even when the switch is on, otherwise a
    // LAN peer could delete files or rewrite the config anonymously.
    *method != Method::Get
}""",
    "D token on all mutating methods",
)

# ---- G: /docs honors the kill switch ----
patch(
    ROUTER,
    """    let (path, query) = split_url(&url);
    if method == Method::Get && (path == "/docs" || path == "/docs/") {
        // Human-readable API documentation. Same authorization semantics
        // as every other read endpoint: allowUnauthenticated opens it, a
        // configured token gates it. Browsers authenticate via the query
        // token since they cannot set Authorization headers.
        if !is_authorized(&app, query, &headers) {""",
    """    let (path, query) = split_url(&url);
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
        if !is_authorized(&app, query, &headers) {""",
    "G /docs kill-switch",
)

# ---- H: access log for mutating requests ----
patch(
    ROUTER,
    """    let (path, query) = split_url(&url);
    if method == Method::Get && (path == "/docs" || path == "/docs/") {""",
    """    let (path, query) = split_url(&url);
    // Access log: mutating requests are always logged so deletes, config
    // writes and history restores can be reconstructed from the console.
    if matches!(
        method,
        &Method::Post | &Method::Put | &Method::Patch | &Method::Delete
    ) {
        eprintln!("[api] {} {}", method, url);
    }
    if method == Method::Get && (path == "/docs" || path == "/docs/") {""",
    "H access log",
)

# ---- B: reviews.rs — lock + atomic write ----
s = io.open(REVIEWS, encoding="utf-8").read()
old_lock = "pub(super) fn write_raw_review_array(path: &Path, parsed: &Value) -> Result<(), String> {"
new_lock = """/// Serializes the read-modify-write cycles in patch/delete/bulk-resolve so
/// concurrent requests cannot lose updates or interleave torn writes.
pub(super) static REVIEW_LOCK: Mutex<()> = Mutex::new(());

pub(super) fn write_raw_review_array(path: &Path, parsed: &Value) -> Result<(), String> {"""
assert old_lock in s
s = s.replace(old_lock, new_lock, 1)

old_write = """fn write_raw_review_array(path: &Path, parsed: &Value) -> Result<(), String> {
    let serialized = serde_json::to_string_pretty(parsed)
        .map_err(|err| format!("Failed to serialize review state: {err}"))?;
    fs::write(path, serialized).map_err(|err| format!("Failed to write review state: {err}"))
}"""
new_write = """fn write_raw_review_array(path: &Path, parsed: &Value) -> Result<(), String> {
    let serialized = serde_json::to_string_pretty(parsed)
        .map_err(|err| format!("Failed to serialize review state: {err}"))?;
    // Atomic tmp+rename: a torn review.json previously surfaced as a 500
    // on the next read.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serialized)
        .map_err(|err| format!("Failed to write review state tmp: {err}"))?;
    fs::rename(&tmp, path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        format!("Failed to replace review state: {err}")
    })
}"""
assert old_write in s
s = s.replace(old_write, new_write, 1)

# lock acquisition in the three mutating handlers
for h in ["handle_patch_review(", "handle_delete_review(", "handle_bulk_resolve_reviews("]:
    marker = "pub(super) fn " + h
    idx = s.index(marker)
    # insert guard after the opening brace line
    insert_at = s.index("\n", idx) + 1
    guard = "    let _review_guard = crate::api_server::reviews::REVIEW_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());\n"
    s = s[:insert_at] + guard + s[insert_at:]

# Mutex import
if "use std::sync::Mutex;" not in s:
    s = s.replace("use super::*;", "use std::sync::Mutex;\n\nuse super::*;", 1)
io.open(REVIEWS, "w", encoding="utf-8", newline="\n").write(s)
applied.append("B reviews lock + atomic write")

# ---- F: lib.rs / project_registry doc comments ----
patch(
    r"D:\llm_wiki\src-tauri\src\lib.rs",
    """//! (`api_server`), the browser-extension clip server (`clip_server`),
//! the agent runtime (`agent`), and the document/queue/vector engine""",
    """//! (`api_server`) and the agent runtime (`agent`), with the document and
//! vector engine (`commands`),""",
    "F lib.rs doc",
)

patch(
    r"D:\llm_wiki\src-tauri\src\project_registry.rs",
    """//! Originally the state of the Chrome-extension clip server; since the
//! extension (and the clip server) were removed, this remains as the
//! lightweight way for the headless server (`LLM_WIKI_PROJECT_PATH`) and
//! future API routes to pin which project "current" refers to.""",
    """//! Tracks which project "current" refers to for this server process.
//! Registrations made through the API are persisted into app-state.json
//! `projectRegistry` (see persist_project_registration in api_server);
//! `LLM_WIKI_PROJECT_PATH` pins the startup project for the headless
//! server.""",
    "F project_registry doc",
)

# ---- openapi.json /docs: bearerAuth ----
import json

P = r"D:\llm_wiki\src-tauri\openapi.json"
spec = json.load(open(P, encoding="utf-8"))
docs_op = spec["paths"].get("/docs", {}).get("get", {})
if docs_op.get("security") == []:
    docs_op["security"] = [{"bearerAuth": []}]
    docs_op["description"] = (
        "Rendered Chinese HTML documentation page. Auth: bearer token via "
        "Authorization header or ?token=<token> query parameter. Honors the "
        "api_enabled kill switch (503 when disabled)."
    )
    json.dump(spec, open(P, "w", encoding="utf-8"), indent=2, ensure_ascii=False)
    applied.append("openapi /docs security -> bearerAuth")

print("APPLIED:", applied)
