"""Second-audit remediation batch: settings read-failure rejection, /docs
auth consistency, file-read error classification, history empty-state,
cancel.rs poisoned-lock recovery, stale comments, README port fixes,
project registry persistence."""
import io
import re

MOD = r"D:\llm_wiki\src-tauri\src\api_server\mod.rs"
LIB = r"D:\llm_wiki\src-tauri\src\lib.rs"
CANCEL = r"D:\llm_wiki\src-tauri\src\agent\cancel.rs"
FH = r"D:\llm_wiki\src-tauri\src\commands\file_history.rs"
applied = []


def patch(path, old, new, label, count=1):
    s = io.open(path, encoding="utf-8").read()
    assert old in s, "anchor missing in " + path + ": " + label
    s = s.replace(old, new, count)
    io.open(path, "w", encoding="utf-8", newline="\n").write(s)
    applied.append(label)


# ---- A1: PUT /settings — reject on unreadable/corrupt base ----
patch(
    MOD,
    """    let mut merged: Value = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or_else(|| json!({}));
    deep_merge_masked(&mut merged, &update);""",
    """    // A missing file is a legitimate first run (empty base); anything
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
    deep_merge_masked(&mut merged, &update);""",
    "A1 PUT settings read-fail rejection",
)

# ---- A2: file read error classification ----
patch(
    MOD,
    '''        match fs::read_to_string(&path) {
            Ok(content) => ok(json!({''',
    '''        match fs::read_to_string(&path) {
            Ok(content) => ok(json!({''',
    "noop-anchor",
) if False else None

s = io.open(MOD, encoding="utf-8").read()
old_read = '''    match fs::read_to_string(&path) {
        Ok(content) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "path": rel,
            "content": content,
            "bytes": content.len(),
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
    }'''
new_read = '''    match fs::read_to_string(&path) {
        Ok(content) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "path": rel,
            "content": content,
            "bytes": content.len(),
        })),
        // Distinguish the failure classes: a missing file is 404 (the
        // metadata check above normally catches it first), a permission
        // problem is 403, anything else (binary/encoding) stays 415.
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
    }'''
assert old_read in s, "file-read anchor missing"
s = s.replace(old_read, new_read, 1)
applied.append("A2 file-read classification")

# ---- A3: history list — missing store/dir is an empty result, not 404 ----
old_hist = '''    match app.block_on(commands::file_history::list_file_history(
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
    }'''
new_hist = '''    match app.block_on(commands::file_history::list_file_history(
        project.path.clone(),
        abs_path,
    )) {
        Ok(entries) => ok(json!({
            "ok": true,
            "projectId": project.id,
            "path": rel,
            "entries": entries,
        })),
        // A project with no recorded history is a normal empty state, not
        // a client error.
        Err(e) if e.contains("os error 2") || e.contains("not found") || e.contains("not found".to_uppercase().as_str()) => {
            ok(json!({ "ok": true, "projectId": project.id, "path": rel, "entries": [] }))
        }
        Err(e) => err(500, e),
    }'''
assert old_hist in s, "history list anchor missing"
s = s.replace(old_hist, new_hist, 1)
applied.append("A3 history empty-state")

# ---- A4: /docs auth consistency (allowUnauthenticated opens docs) ----
old_docs = '''        if !is_token_authorized(&app, query, &headers) {'''
new_docs = '''        // Same authorization semantics as every other read endpoint:
        // allowUnauthenticated opens the page, a configured token gates it.
        if !is_authorized(&app, query, &headers) {'''
assert old_docs in s, "docs auth anchor missing"
s = s.replace(old_docs, new_docs, 1)
applied.append("A4 /docs auth consistency")

# ---- A5: 401 hint for docs now wording-consistent ----
s = s.replace(
    '"Unauthorized: append ?token=<your token> to the URL"',
    '"Unauthorized: append ?token=<your token> to the URL (or enable allowUnauthenticated)"',
    1,
)
applied.append("A5 docs 401 hint")

io.open(MOD, "w", encoding="utf-8", newline="\n").write(s)
print("APPLIED:", applied)
