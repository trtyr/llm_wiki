"""One-shot refactor: extract planner.rs and dispatch.rs from agent/runtime/mod.rs.

Child modules of agent::runtime see all parent private items, so moved
code needs no visibility cascade. Boundaries are content-verified with
asserts before any write; nothing is written unless all anchors match.
"""
import io

P = r"D:\llm_wiki\src-tauri\src\agent\runtime\mod.rs"
s = io.open(P, encoding="utf-8").read()
lines = s.split("\n")


def find_exact(target, start=0):
    for i in range(start, len(lines)):
        if lines[i] == target:
            return i
    raise AssertionError("not found: " + repr(target))


def find_prefix(prefix, start=0):
    for i in range(start, len(lines)):
        if lines[i].startswith(prefix):
            return i
    raise AssertionError("not found: " + repr(prefix))


# --- planner fn cluster (pure functions) ---
esc = find_exact("fn escape_planner_xml(input: &str) -> String {")
scc = find_prefix("fn shell_command_from_call(call: &ModelToolCall) -> Option<&str> {", esc)
ptq = find_prefix("fn planned_tool_queries(plan: &ModelToolPlan", scc)
pmp = find_prefix("fn parse_model_tool_plan(raw: &str) -> Result<ModelToolPlan, String> {", ptq)
ejo = find_prefix("fn extract_json_object(raw: &str) -> Option<&str> {", pmp)
check_cancel = find_prefix("fn check_cancel(cancellation: Option<&AgentCancellationToken>) -> Result<(), String> {", ejo)

# --- structs ---
st_plan = find_exact("struct ModelToolPlan {")
derive_plan = st_plan - 2
assert lines[derive_plan].startswith("#[derive(Debug, Clone, Default, Deserialize)]"), lines[derive_plan]
assert lines[st_plan - 1].startswith("#[serde("), lines[st_plan - 1]
# end of ModelToolCall block: find struct ModelToolCall, then its closing brace
st_call = find_exact("struct ModelToolCall {", st_plan)
end_call = next(i for i in range(st_call, len(lines)) if lines[i] == "}") + 1

# --- impl methods ---
eat_attr = find_prefix("    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]")
eat = find_exact("    async fn execute_agent_loop_tool(", eat_attr)
assert eat == eat_attr + 1, (eat_attr, eat)
alt = find_exact("    fn agent_loop_tool_input(", eat)
rls = find_exact("    fn record_loop_tool_success(", alt)
tcx = find_prefix("    fn tool_context(&self) -> tools::ToolContext<'_> {", rls)

# --- emit fns ---
em_start = find_exact("fn emit_event(")
em_end = find_prefix("async fn generate_with_cancellation<P: AgentLlmProvider>(", em_start)

print(
    "anchors:",
    dict(esc=esc, scc=scc, ptq=ptq, pmp=pmp, ejo=ejo, check=check_cancel,
         derive=derive_plan, st_call_end=end_call, eat_attr=eat_attr,
         eat=eat, alt=alt, rls=rls, tcx=tcx, em=em_start, em_end=em_end),
)

# sanity: method order within the impl
assert eat_attr < eat < alt < rls < tcx
# planner fns must come after tcx in this file layout
assert tcx < esc < check_cancel
# struct block is near the top
assert st_plan < 100

# ================= extraction =================
planner_fns = lines[esc:check_cancel]
struct_block = lines[derive_plan:end_call]
dispatch_methods = lines[eat_attr:tcx]
emit_fns = lines[em_start:em_end]

# ================= removal (descending) =================
for a, b in sorted(
    [(em_start, em_end), (esc, check_cancel), (eat_attr, tcx), (derive_plan, end_call)],
    key=lambda r: -r[0],
):
    del lines[a:b]

# ================= planner.rs =================
out = []
out.append("//! Planner: decides when a model-driven tool plan is built and parses it.\n")
out.append("//! Extracted from the runtime loop; pure functions + plan types.\n\n")
out.append("use serde::Deserialize;\n")
out.append("use serde_json::Value;\n\n")
out.append("use super::{AgentMode, AgentSkill, AgentSkillMode, AgentToolOptions};\n\n")
structs = "\n".join(struct_block)
structs = structs.replace("struct ModelToolPlan {", "pub(super) struct ModelToolPlan {", 1)
structs = structs.replace("struct ModelToolCall {", "pub(super) struct ModelToolCall {", 1)
out.append(structs + "\n\n")
fns = "\n".join(planner_fns)
fns = fns.replace(
    "fn should_plan_tools_with_model(",
    "pub(super) fn should_plan_tools_with_model(",
    1,
)
fns = fns.replace(
    "fn parse_model_tool_plan(",
    "pub(super) fn parse_model_tool_plan(",
    1,
)
out.append(fns + "\n")
io.open(r"D:\llm_wiki\src-tauri\src\agent\runtime\planner.rs", "w", encoding="utf-8", newline="\n").write(
    "".join(out)
)

# ================= dispatch.rs =================
out = []
out.append("//! Agent loop tool dispatch: input building, execution and success\n")
out.append("//! recording for a single Agent loop tool call.\n\n")
out.append("use super::*;\n\n")
out.append("impl AgentRuntime {\n")
methods = "\n".join(dispatch_methods)
# make the moved methods callable from the parent loop
methods = methods.replace(
    "    async fn execute_agent_loop_tool(",
    "    pub(super) async fn execute_agent_loop_tool(",
    1,
)
out.append(methods)
out.append("}\n\n")
emit = "\n".join(emit_fns)
emit = emit.replace("\nfn emit_event(", "\npub(super) fn emit_event(", 1)
emit = emit.replace("\nfn tool_emit_event(", "\npub(super) fn tool_emit_event(", 1)
out.append(emit + "\n")
io.open(r"D:\llm_wiki\src-tauri\src\agent\runtime\dispatch.rs", "w", encoding="utf-8", newline="\n").write(
    "".join(out)
)

# ================= mod.rs wiring =================
decl = "mod dispatch;\nmod planner;\n\n"
use_line = (
    "use dispatch::{execute_agent_loop_tool, record_loop_tool_success};\n"
    "use planner::{parse_model_tool_plan, planned_tool_queries, should_plan_tools_with_model, ModelToolCall, ModelToolPlan};\n"
)
anchor = "use super::workspace::agent_workspace_display;"
s = "\n".join(lines)
assert anchor in s
s = s.replace(anchor, decl + use_line + anchor, 1)
io.open(P, "w", encoding="utf-8", newline="\n").write(s)
print("extraction complete: planner.rs + dispatch.rs + mod.rs wired")
