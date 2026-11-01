//! Inert skills module.
//!
//! The built-in skills mechanism (loading instruction folders from
//! `.llm-wiki/skills`, `~/.claude/skills`, ...) was removed by design: the
//! skills layer now belongs to the API consumer, who can inject any
//! instructions into an agent turn via `POST /chat` history and tool
//! options. `load_project_skills` always returns an empty set, which keeps
//! the agent loop's `skills_enabled` gates closed - meaning the planner
//! never offers `skills.load`, `skill.read_file`, `workspace.write_file`,
//! or `shell.exec`.

use serde::{Deserialize, Serialize};

/// Kept for the agent loop's skill-threading signature (always empty).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub base_dir: String,
    pub location: String,
}

/// Always empty: the backend does not bundle a skills mechanism.
pub fn load_project_skills(_project_path: &str, _requested: &[String]) -> Vec<AgentSkill> {
    Vec::new()
}
