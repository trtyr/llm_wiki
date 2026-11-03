//! Review store: review.json read/modify/write, sanitization and the
//! review HTTP handlers. Extracted from api_server/mod.rs.

use std::sync::Mutex;

use super::*;

pub(super) fn parse_review_query(query: &str) -> Result<ReviewQuery, String> {
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

pub(super) fn normalize_review_title(title: &str) -> String {
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

pub(super) fn review_id_for_parts(item_type: &str, title: &str) -> String {
    let key = format!("{item_type}::{}", normalize_review_title(title));
    let mut h: u32 = 0x811c9dc5;
    for unit in key.encode_utf16() {
        h ^= u32::from(unit);
        h = h.wrapping_mul(0x01000193);
    }
    format!("review-{h:08x}")
}

pub(super) fn stable_review_id(item: &Value) -> Option<String> {
    let item_type = item.get("type").and_then(Value::as_str)?;
    let title = item.get("title").and_then(Value::as_str)?;
    Some(review_id_for_parts(item_type, title))
}

pub(super) fn review_id_matches(item: &Value, requested_id: &str) -> bool {
    item.get("id").and_then(Value::as_str) == Some(requested_id)
        || stable_review_id(item).as_deref() == Some(requested_id)
}

pub(super) fn load_review_items(
    project_path: &str,
    query: &ReviewQuery,
) -> Result<Vec<Value>, String> {
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

pub(super) fn sanitize_review_item(item: &Value) -> Value {
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

pub(super) fn merge_sanitized_review(existing: &mut Value, incoming: &Value) {
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

pub(super) fn merge_string_array_field(
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

pub(super) fn merge_review_options_field(
    existing: &mut Map<String, Value>,
    incoming: &Map<String, Value>,
) {
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

pub(super) fn copy_string_field(item: &Value, out: &mut Map<String, Value>, key: &str) {
    if let Some(value) = item.get(key).and_then(Value::as_str) {
        out.insert(key.to_string(), Value::String(value.to_string()));
    }
}

pub(super) fn copy_bool_field(item: &Value, out: &mut Map<String, Value>, key: &str) {
    if let Some(value) = item.get(key).and_then(Value::as_bool) {
        out.insert(key.to_string(), Value::Bool(value));
    }
}

pub(super) fn copy_number_field(item: &Value, out: &mut Map<String, Value>, key: &str) {
    if let Some(value) = item.get(key).and_then(Value::as_f64) {
        if value.is_finite() {
            out.insert(key.to_string(), json!(value));
        }
    }
}

pub(super) fn copy_string_array_field(item: &Value, out: &mut Map<String, Value>, key: &str) {
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

pub(super) fn copy_review_options(item: &Value, out: &mut Map<String, Value>) {
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

pub(super) fn handle_reviews(app: &ServerHost, project_id: &str, query: &str) -> ApiResponse {
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
        Err(e) => {
            eprintln!("[api] 500: {e}");
            err(500, e)
        }
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
pub(super) fn handle_patch_review(
    app: &ServerHost,
    project_id: &str,
    review_id: &str,
    body: &str,
) -> ApiResponse {
    // Serialize the read-modify-write cycle against concurrent
    // PATCH/DELETE/bulk-resolve requests.
    let _review_guard = REVIEW_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        Err(e) => {
            eprintln!("[api] 500: {e}");
            err(500, e)
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BulkResolveRequest {
    /// Review item ids to resolve. Required, non-empty.
    #[serde(alias = "reviewIds")]
    pub(super) ids: Vec<String>,
    /// Optional label applied to every resolved item.
    pub(super) action: Option<String>,
}

pub(super) fn handle_delete_review(
    app: &ServerHost,
    project_id: &str,
    review_id: &str,
) -> ApiResponse {
    let _review_guard = REVIEW_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        Err(e) => {
            eprintln!("[api] 500: {e}");
            err(500, e)
        }
    }
}

pub(super) fn delete_review_item(project_path: &str, review_id: &str) -> Result<bool, String> {
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
pub(super) fn handle_bulk_resolve_reviews(
    app: &ServerHost,
    project_id: &str,
    body: &str,
) -> ApiResponse {
    let _review_guard = REVIEW_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        Err(e) => {
            eprintln!("[api] 500: {e}");
            err(500, e)
        }
    }
}

/// Set one review item's resolved state in `.llm-wiki/review.json`.
///
/// Operates on the RAW parsed array (not `load_review_items`, which
/// sanitizes — reusing it would strip fields like `internalSecret` and
/// silently corrupt the file on write-back). Sets `resolved` and, when
/// provided, `resolvedAction`. Returns Ok(false) if no item with the
/// given id exists (caller maps to 404).
pub(super) fn patch_review_item(
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
pub(super) fn resolve_review_items(
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
pub(super) fn apply_resolution(item: &mut Value, resolved: bool, action: Option<&str>) {
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
pub(super) fn read_raw_review_array(path: &Path) -> Result<Option<Value>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("Failed to read review state: {err}")),
    };
    let parsed: Value =
        serde_json::from_str(&raw).map_err(|err| format!("Invalid review state JSON: {err}"))?;
    Ok(Some(parsed))
}

/// Serializes the read-modify-write cycles in patch/delete/bulk-resolve so
/// concurrent requests cannot lose updates or interleave torn writes.
pub(super) static REVIEW_LOCK: Mutex<()> = Mutex::new(());

pub(super) fn write_raw_review_array(path: &Path, parsed: &Value) -> Result<(), String> {
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
}
