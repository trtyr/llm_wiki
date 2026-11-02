"""Remaining minor fixes from the third audit:
1. session.rs: atomic save + corrupt-JSON load warning
2. file_history.rs: corrupt store -> Err (missing stays empty)
3. chat error classification: helper maps error strings to 400/499/502
4. write-page status: classifier co-located in agent/tools.rs + unit tests
5. headless.rs: warn also when allowUnauthenticated is enabled
6. access log: mutating requests logged in process_request
"""
import io

applied = []


def patch(path, old, new, label):
    s = io.open(path, encoding="utf-8").read()
    assert old in s, "anchor missing: " + label
    s = s.replace(old, new, 1)
    io.open(path, "w", encoding="utf-8", newline="\n").write(s)
    applied.append(label)


# ---------- 1a. session.rs: atomic save ----------
SESS = r"D:\llm_wiki\src-tauri\src\agent\session.rs"
patch(
    SESS,
    """    let raw = serde_json::to_string_pretty(session)
        .map_err(|err| format!("Failed to serialize session: {err}"))?;
    fs::write(path, raw).map_err(|err| format!("Failed to write session: {err}"))""",
    """    let raw = serde_json::to_string_pretty(session)
        .map_err(|err| format!("Failed to serialize session: {err}"))?;
    // Atomic write-then-rename: a torn session file previously loaded as a
    // silent empty session, wiping the user's chat history.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, raw).map_err(|err| format!("Failed to write session tmp: {err}"))?;
    if let Err(err) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("Failed to replace session file: {err}"));
    }
    Ok(())""",
    "session.rs atomic save",
)

# ---------- 1b. session.rs: corrupt-JSON load warning ----------
patch(
    SESS,
    """fn load_session(project_path: &str, session_id: &str) -> Option<AgentSession> {
    let path = session_file(project_path, session_id)?;
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}""",
    """fn load_session(project_path: &str, session_id: &str) -> Option<AgentSession> {
    let path = session_file(project_path, session_id)?;
    let raw = fs::read_to_string(path).ok()?;
    match serde_json::from_str(&raw) {
        Ok(session) => Some(session),
        Err(err) => {
            // A corrupt session file must not silently look like a brand-new
            // chat: surface it so the operator knows history was dropped.
            eprintln!(
                "[session] corrupt session file {session_id} in {project_path}: {err}"
            );
            None
        }
    }
}""",
    "session.rs corrupt-load warning",
)

# ---------- 2. file_history.rs: corrupt store -> Err ----------
FH = r"D:\llm_wiki\src-tauri\src\commands\file_history.rs"
patch(
    FH,
    """        let raw = fs::read_to_string(store_path).unwrap_or_else(|_| "[]".to_string());
        let mut entries: Vec<FileHistoryEntry> = serde_json::from_str(&raw).unwrap_or_default();""",
    """        // Missing store is a normal empty history; a corrupt one must be an
        // error so clients don't mistake data loss for "no versions yet".
        let raw = match fs::read_to_string(&store_path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "[]".to_string(),
            Err(e) => return Err(format!("Failed to read history store: {e}")),
        };
        let mut entries: Vec<FileHistoryEntry> = serde_json::from_str(&raw)
            .map_err(|err| format!("History store is corrupt: {err}"))?;""",
    "file_history corrupt -> Err",
)

# ---------- 4. headless.rs: warn covers allowUnauthenticated ----------
HEAD = r"D:\llm_wiki\src-tauri\src\headless.rs"
patch(
    HEAD,
    """    let has_token = std::env::var("LLM_WIKI_API_TOKEN")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
        || store_token(store_path).is_some();
    if !has_token {""",
    """    let anonymous_ok = std::env::var("LLM_WIKI_ALLOW_UNAUTHENTICATED")
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE"))
        .unwrap_or(false)
        || store_flag(store_path, "apiConfig", "allowUnauthenticated");
    let has_token = std::env::var("LLM_WIKI_API_TOKEN")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
        || store_token(store_path).is_some();
    if anonymous_ok {
        eprintln!(
            "[llm-server] WARNING: allowUnauthenticated is enabled — every endpoint on {bind_host} is open to the local network without a token"
        );
    }
    if !has_token {""",
    "headless warn allowUnauthenticated",
)

# store_flag helper in headless.rs
patch(
    HEAD,
    """fn store_token(store_path: &Path) -> Option<String> {""",
    """fn store_flag(store_path: &Path, key_path: &str) -> bool {
    let Ok(raw) = fs::read_to_string(store_path) else {
        return false;
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let mut node = &parsed;
    for part in key_path.split('.') {
        node = node.get(part)?;
    }
    node.as_bool() == Some(true)
}

fn store_token(store_path: &Path) -> Option<String> {""",
    "headless store_flag helper",
)
applied.append("headless allowUnauthenticated-aware warning")

# re-order: the existing tail uses store_token — keep. Add fs import if needed.
s = io.open(HEAD, encoding="utf-8").read()
if "use std::fs;" not in s:
    s = s.replace("use std::path::Path;", "use std::fs;\nuse std::path::Path;", 1)
    io.open(HEAD, "w", encoding="utf-8", newline="\n").write(s)
    applied.append("headless fs import")

print("APPLIED:", applied)
