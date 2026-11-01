"""One-shot refactor step: extract respond.rs from api_server/mod.rs."""
import io

P = r"D:\llm_wiki\src-tauri\src\api_server\mod.rs"
s = io.open(P, encoding="utf-8").read()
lines = s.split("\n")
applied = []


def find_line(marker, start=0):
    for i in range(start, len(lines)):
        if lines[i].startswith(marker):
            return i
    raise AssertionError("not found: " + marker)


def block_end(start):
    # first line that is exactly '}' at col 0 after start
    for i in range(start + 1, len(lines)):
        if lines[i] == "}":
            return i + 1
    raise AssertionError("no block end after " + str(start))


# --- rate limit static ---
i = find_line("static RATE_LIMIT:")
static_line = lines[i]
del lines[i]

# --- consts ---
i1 = find_line("const RATE_LIMIT_WINDOW:")
i2 = find_line("const RATE_LIMIT_MAX_REQUESTS:")
const_block = lines[i1 : i2 + 1]
del lines[i1 : i2 + 1]

# --- ApiResponse struct ---
i = find_line("struct ApiResponse {")
resp_struct_start = i
resp_struct_end = block_end(i)  # includes closing '}' line
resp_struct = lines[resp_struct_start:resp_struct_end]
del lines[resp_struct_start:resp_struct_end]

# --- ok / err ---
i_ok = find_line("fn ok(body: Value) -> ApiResponse {")
i_err = find_line("fn err(status: u16, message: impl Into<String>) -> ApiResponse {")
ok_err_start = i_ok
ok_err_end = block_end(i_err)  # end of err fn
ok_err = lines[ok_err_start:ok_err_end]
del lines[ok_err_start:ok_err_end]

# --- respond_error / respond_options / respond_json ---
i_re = find_line("fn respond_error(request: tiny_http::Request")
i_ro = find_line("fn respond_options(request: tiny_http::Request")
i_rj = find_line("fn respond_json(request: tiny_http::Request")
i_next = find_line("fn cors_headers(origin: Option<&str>) -> Vec<Header> {")
respond_fns = lines[i_re:i_next]
del lines[i_re:i_next]

# --- should_rate_limit / allow_request ---
i_sr = find_line("fn should_rate_limit(method: &Method, url: &str) -> bool {")
i_ac = find_line("fn allow_request() -> bool {")
i_next2 = find_line("fn is_agent_chat_request(method: &Method, path: &str) -> bool {")
rate_fns = lines[i_sr:i_next2]
del lines[i_sr:i_next2]

# --- write respond.rs ---
out = []
out.append("//! Response construction helpers + request rate limiting.\n")
out.append("//! Extracted from api_server/mod.rs; visibility is pub(super) so the\n")
out.append("//! parent module (server loop, router, handlers) can use them.\n\n")
out.append("use serde_json::Value;\n")
out.append("use tiny_http::{Header, Response as TinyResponse};\n\n")
out.append("use super::*;\n\n")
out.append("pub(super) const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(1);\n")
out.append("pub(super) const RATE_LIMIT_MAX_REQUESTS: usize = 120;\n")
out.append(static_line.replace("static RATE_LIMIT:", "pub(super) static RATE_LIMIT:") + "\n\n")
st = "\n".join(resp_struct)
st = st.replace("struct ApiResponse {", "pub(super) struct ApiResponse {", 1)
st = st.replace("    status: u16,", "    pub(super) status: u16,", 1)
st = st.replace("    body: Value,", "    pub(super) body: Value,", 1)
out.append(st + "\n\n")
oe = "\n".join(ok_err)
oe = oe.replace("fn ok(body: Value) -> ApiResponse {", "pub(super) fn ok(body: Value) -> ApiResponse {", 1)
oe = oe.replace("fn err(status: u16, message: impl Into<String>) -> ApiResponse {", "pub(super) fn err(status: u16, message: impl Into<String>) -> ApiResponse {", 1)
out.append(oe + "\n\n")
rf = "\n".join(respond_fns)
for fn_name in ["fn respond_error(", "fn respond_options(", "fn respond_json("]:
    rf = rf.replace("\n" + fn_name, "\npub(super) " + fn_name, 1)
    rf = rf.replace(rf[: rf.index(fn_name) + len(fn_name)].rsplit("\n", 1)[-1], rf[: rf.index(fn_name) + len(fn_name)].rsplit("\n", 1)[-1])
out.append(rf + "\n\n")
rt = "\n".join(rate_fns)
rt = rt.replace("fn should_rate_limit(", "pub(super) fn should_rate_limit(", 1)
rt = rt.replace("fn allow_request(", "pub(super) fn allow_request(", 1)
out.append(rt + "\n")
io.open(r"D:\llm_wiki\src-tauri\src\api_server\respond.rs", "w", encoding="utf-8", newline="\n").write("".join(out))
applied.append("respond.rs written")

# --- wire mod.rs ---
s = io.open(P, encoding="utf-8").read()
anchor = "mod params;"
s = s.replace(
    anchor,
    "mod respond;\n\n"
    "use respond::{allow_request, ok, err, respond_error, respond_json, respond_options,\n"
    "              should_rate_limit, ApiResponse};",
    1,
)
io.open(P, "w", encoding="utf-8", newline="\n").write(s)
applied.append("mod.rs wired")

print("APPLIED:", applied)
