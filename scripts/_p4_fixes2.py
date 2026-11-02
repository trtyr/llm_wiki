"""Remaining second-audit fixes: file-read classification, /docs auth, hint."""
import io

MOD = r"D:\llm_wiki\src-tauri\src\api_server\mod.rs"
s = io.open(MOD, encoding="utf-8").read()
applied = []

old_read = '''        Err(_) => err(415, "File is not valid UTF-8 text"),
    }
}'''
new_read = '''        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
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
}'''
if old_read in s:
    s = s.replace(old_read, new_read, 1)
    applied.append("file-read classification")

old_docs = '''        if !is_token_authorized(&app, query, &headers) {'''
new_docs = '''        // Same authorization semantics as every other read endpoint:
        // allowUnauthenticated opens the page, a configured token gates it.
        if !is_authorized(&app, query, &headers) {'''
if old_docs in s:
    s = s.replace(old_docs, new_docs, 1)
    applied.append("/docs auth consistency")

old_hint = '"Unauthorized: append ?token=<your token> to the URL"'
new_hint = '"Unauthorized: append ?token=<your token> to the URL (or enable allowUnauthenticated)"'
if old_hint in s:
    s = s.replace(old_hint, new_hint, 1)
    applied.append("401 hint")

if applied:
    io.open(MOD, "w", encoding="utf-8", newline="\n").write(s)
print("APPLIED:", applied)
