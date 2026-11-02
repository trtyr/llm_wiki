"""Genericize 500 error bodies in api_server handlers: log detail
server-side via err_internal, return generic message to the client."""
import io, re

P = r"D:\llm_wiki\src-tauri\src\api_server\mod.rs"
s = io.open(P, encoding="utf-8").read()

# helper re-export in mod.rs? err_internal lives in respond.rs; mod.rs does
# `use respond::{...}` — add err_internal to that use list and to respond.rs
# export. Handlers reference it bare.

# 1) respond.rs already has err_internal (added earlier). Verify:
assert "pub(super) fn err_internal(" in io.open(
    r"D:\llm_wiki\src-tauri\src\api_server\respond.rs", encoding="utf-8"
).read(), "err_internal missing in respond.rs"

# 2) mod.rs: import err_internal from respond
s = io.open(P := r"D:\llm_wiki\src-tauri\src\api_server\mod.rs", encoding="utf-8").read()
old = """use respond::{allow_request, ok, err, respond_error, respond_json, respond_options,
              should_rate_limit, ApiResponse};"""
new = """use respond::{
    allow_request, err_internal, ok, respond_error, respond_json, respond_options,
    should_rate_limit, ApiResponse,
};"""
assert old in s, "respond use anchor missing"
s = s.replace(old, new, 1)

# 3) replace the plain `err(500, e)` passthrough arms with the logging helper
n = s.count("Err(e) => err(500, e),")
s = s.replace("Err(e) => err(500, e),", 'Err(e) => err_internal("handler error", e),')
applied_count = n

# 4) settings-corrupt rejects: keep actionable hint server-side, generic out
old_a = '''            return err(500, format!("app-state.json is unreadable ({}); refusing to overwrite", e))'''
new_a = '''            return err_internal(
                "settings PUT: app-state.json unreadable, refusing overwrite",
                e,
            )'''
if old_a in s:
    s = s.replace(old_a, new_a, 1)
    applied_count += 1
old_b = '''                return err(500,
                    "app-state.json is corrupt or not a JSON object; refusing to overwrite — fix or move the file first")'''
new_b = '''                return err_internal(
                    "settings PUT: app-state.json corrupt, refusing overwrite",
                    "app-state.json is corrupt or not a JSON object; refusing to overwrite (see server log)",
                )'''
if old_b in s:
    s = s.replace(old_b, new_b, 1)
    applied_count += 1

io.open(P, "w", encoding="utf-8", newline="\n").write(s)
print("APPLIED:", applied_count, "500 sites genericized")
