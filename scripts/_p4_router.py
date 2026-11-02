"""Second-audit fixes in router.rs: /docs auth consistency + 401 hint."""
import io

P = r"D:\llm_wiki\src-tauri\src\api_server\router.rs"
s = io.open(P, encoding="utf-8").read()
applied = []

old_gate = """        // Human-readable API documentation; browsers authenticate via the
        // query token since they cannot set Authorization headers.
        if !is_token_authorized(&app, query, &headers) {"""
new_gate = """        // Human-readable API documentation. Same authorization semantics
        // as every other read endpoint: allowUnauthenticated opens it, a
        // configured token gates it. Browsers authenticate via the query
        // token since they cannot set Authorization headers.
        if !is_authorized(&app, query, &headers) {"""
if old_gate in s:
    s = s.replace(old_gate, new_gate, 1)
    applied.append("/docs gate -> is_authorized")

old_hint = '"Unauthorized: append ?token=<your token> to the URL"'
new_hint = '"Unauthorized: append ?token=<your token> to the URL (or enable allowUnauthenticated)"'
if old_hint in s:
    s = s.replace(old_hint, new_hint, 1)
    applied.append("401 hint extended")

io.open(P, "w", encoding="utf-8", newline="\n").write(s)
print("APPLIED:", applied)
