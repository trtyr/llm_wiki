//! Planner: decides when a model-driven tool plan is built and parses it.
//! Extracted from the runtime loop; pure functions + plan types.

use std::collections::BTreeMap;

use crate::agent::types::{AgentMode, AgentToolOptions};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelToolPlan {
    #[serde(default)]
    pub(super) tool_calls: Vec<ModelToolCall>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelToolCall {
    pub(super) tool: String,
    #[serde(default)]
    pub(super) query: Option<String>,
    #[serde(default)]
    pub(super) command: Option<String>,
    #[serde(default)]
    pub(super) timeout_seconds: Option<u64>,
    #[serde(default)]
    pub(super) path: Option<String>,
    #[serde(default)]
    pub(super) content: Option<String>,
    // Overwriting existing wiki pages is destructive. The planner may set this
    // only when the user explicitly asks to update/overwrite an existing page;
    // the tool defaults to create-only when the field is absent.
    #[serde(default)]
    pub(super) allow_overwrite: Option<bool>,
}

pub(super) fn escape_planner_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub(super) fn shell_command_from_call(call: &ModelToolCall) -> Option<&str> {
    call.command
        .as_deref()
        .or(call.query.as_deref())
        .or(call.content.as_deref())
        .map(str::trim)
        .filter(|command| !command.is_empty())
}

pub(super) fn planned_tool_queries(
    plan: &ModelToolPlan,
    fallback_query: &str,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for call in &plan.tool_calls {
        let tool = call.tool.trim();
        if !matches!(
            tool,
            "wiki.search"
                | "source.search"
                | "graph.search"
                | "web.search"
                | "anytxt.search"
                | "wiki.write_page"
        ) {
            continue;
        }
        let query = call
            .query
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .unwrap_or(fallback_query);
        out.entry(tool.to_string())
            .or_insert_with(|| query.to_string());
    }
    out
}

pub(super) fn parse_model_tool_plan(raw: &str) -> Result<ModelToolPlan, String> {
    let json_text = extract_json_object(raw).unwrap_or(raw).trim();
    serde_json::from_str(json_text).map_err(|err| format!("Invalid Agent tool plan JSON: {err}"))
}

pub(super) fn extract_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(&raw[start..=end])
}

pub(super) fn should_plan_tools_with_model(
    message: &str,
    mode: AgentMode,
    tools: &AgentToolOptions,
    skills_enabled: bool,
) -> bool {
    if matches!(mode, AgentMode::Fast) {
        return false;
    }
    let has_available_tool = tools.wiki || tools.web || tools.anytxt || skills_enabled;
    !message.trim().is_empty() && has_available_tool
}
