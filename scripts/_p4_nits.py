"""Second-audit fixes: cancel.rs poisoned-lock recovery, stale doc comments,
config example clip residue."""
import io

applied = []

# 1) cancel.rs: poisoned-lock recovery instead of unwrap
P = r"D:\llm_wiki\src-tauri\src\agent\cancel.rs"
s = io.open(P, encoding="utf-8").read()
n = s.count(".lock()\n            .unwrap()")
n2 = s.count(".lock()\n        .unwrap()")
# generic: replace every `.lock()\n ... .unwrap()` chain with into_inner recovery
old_a = ".lock()\n            .unwrap()"
new_a = ".lock()\n            .unwrap_or_else(|poisoned| poisoned.into_inner())"
old_b = ".lock()\n        .unwrap()"
new_b = ".lock()\n        .unwrap_or_else(|poisoned| poisoned.into_inner())"
cnt = s.count(old_a) + s.count(old_b)
s = s.replace(old_a, new_a).replace(old_b, new_b)
if cnt:
    applied.append(f"cancel.rs: {cnt} lock unwrap -> poisoned recovery")

io.open(P, "w", encoding="utf-8", newline="\n").write(s)

# 2) lib.rs: stale crate doc mentioning removed clip server
P = r"D:\llm_wiki\src-tauri\src\lib.rs"
s = io.open(P, encoding="utf-8").read()
old_doc = "//! (`api_server`), the browser-extension clip server (`clip_server`),"
if old_doc in s:
    s = s.replace(
        old_doc,
        "//! (`api_server`),",
        1,
    )
    applied.append("lib.rs doc comment updated")

# 3) api_server/mod.rs: invalidate_config_cache stale allow(dead_code) + comment
P = r"D:\llm_wiki\src-tauri\src\api_server\mod.rs"
s = io.open(P, encoding="utf-8").read()
old = """/// Kept for the upcoming settings API (PUT /settings) — the app-state
/// cache must be dropped when config changes arrive over HTTP.
#[allow(dead_code)]
pub fn invalidate_config_cache() {"""
new = """/// Drops the app-state config cache so the next request re-reads
/// app-state.json from disk. Called by PUT /settings after every write.
pub fn invalidate_config_cache() {"""
if old in s:
    s = s.replace(old, new, 1)
    applied.append("invalidate_config_cache comment/allow fixed")

# 4) config example: remove clip residue
P = r"D:\llm_wiki\scripts\llm-server.config.example.json"
try:
    s = io.open(P, encoding="utf-8").read()
    s = s.replace('  "ClipPort": 19827,\n', "").replace('  "ClipServer": "1",\n', "")
    io.open(P, "w", encoding="utf-8", newline="\n").write(s)
    applied.append("config example cleaned")
except FileNotFoundError:
    applied.append("config example missing (skipped)")

io.open(r"D:\llm_wiki\scripts\_p4_nits_result.txt", "w", encoding="utf-8").write("\n".join(applied))
print("APPLIED:", applied)
