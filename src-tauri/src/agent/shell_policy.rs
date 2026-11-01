//! Shell auto-approval policy for the agent's shell.exec tool.
//!
//! Extracted from the runtime loop so the approval rules live in one
//! auditable place. Everything here is best-effort hardening: an
//! untrusted skill (or a future un-skilling regression) must not be able
//! to widen it.

use super::workspace::{agent_workspace_display, AGENT_WORKSPACE_DIR};

/// Interpreters re-enable everything the scoped-workspace checks try to
/// prevent (`bash -lc "cat /etc/shadow"` tokenizes as one non-path
/// token). They always require explicit user approval.
fn is_interpreter_command(command: &str) -> bool {
    let first = command.split_whitespace().next().unwrap_or("");
    let base = first
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(first)
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "bash"
            | "sh"
            | "zsh"
            | "pwsh"
            | "powershell"
            | "cmd"
            | "python"
            | "python3"
            | "py"
            | "node"
            | "deno"
            | "bun"
            | "ruby"
            | "perl"
            | "php"
            | "lua"
    )
}


pub(crate) fn is_shell_command_approved(command: &str, approved: &[String]) -> bool {
    let command = command.trim();
    !command.is_empty() && approved.iter().any(|item| item.trim() == command)
}

pub(crate) fn is_shell_command_allowed_without_prompt(
    command: &str,
    approved: &[String],
    project_path: &str,
) -> bool {
    is_shell_command_approved(command, approved)
        || is_shell_command_scoped_to_agent_workspace(command, project_path)
}

fn is_shell_command_scoped_to_agent_workspace(command: &str, project_path: &str) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }
    // Interpreters re-enable everything the checks below try to prevent
    // (`bash -lc "cat /etc/shadow"` tokenizes as one non-path token).
    // They always require explicit user approval.
    if is_interpreter_command(command) {
        return false;
    }

    let lower = command.to_ascii_lowercase();
    if lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("ftp://")
        || lower.contains("sftp://")
        || lower.contains("curl ")
        || lower.starts_with("curl ")
        || lower.contains("wget ")
        || lower.starts_with("wget ")
        || lower.contains("scp ")
        || lower.starts_with("scp ")
        || lower.contains("ssh ")
        || lower.starts_with("ssh ")
        || lower.contains("$(")
        || command.contains('`')
    {
        return false;
    }

    let workspace = agent_workspace_display(project_path);
    let workspace_norm = normalize_shell_path_for_compare(&workspace);
    let project_norm = normalize_shell_path_for_compare(project_path);
    for token in shell_command_tokens(command) {
        if token.is_empty() {
            continue;
        }
        if shell_token_mentions_external_location(&token, &workspace_norm, &project_norm) {
            return false;
        }
    }
    true
}

fn shell_token_mentions_external_location(
    token: &str,
    workspace_norm: &str,
    project_norm: &str,
) -> bool {
    let token = token.trim_matches(|ch| matches!(ch, '"' | '\'' | ',' | ';'));
    if token.is_empty() {
        return false;
    }
    let lower = token.to_ascii_lowercase();
    if lower.starts_with('~')
        || lower.contains("$home")
        || lower.contains("${home")
        || lower.contains("%userprofile%")
        || lower.contains("%homepath%")
        || lower.contains("$xdg_")
        || lower.contains("${xdg_")
        || lower.contains("$tmp")
        || lower.contains("${tmp")
        || lower.contains("$temp")
        || lower.contains("${temp")
    {
        return true;
    }
    if token == ".." || token.starts_with("../") || token.contains("/../") || token.ends_with("/..")
    {
        return true;
    }
    let mut candidates = vec![token.to_string()];
    if let Some((_, value)) = token.split_once('=') {
        candidates.push(value.to_string());
    }
    for candidate in candidates {
        let candidate = candidate.trim_matches(|ch| matches!(ch, '"' | '\''));
        if candidate.is_empty() {
            continue;
        }
        if is_shell_absolute_path(candidate) {
            let normalized = normalize_shell_path_for_compare(candidate);
            let in_workspace = normalized == workspace_norm
                || normalized.starts_with(&format!("{workspace_norm}/"));
            let project_workspace_prefix = format!("{project_norm}/{AGENT_WORKSPACE_DIR}");
            let in_project_workspace = normalized == project_workspace_prefix
                || normalized.starts_with(&format!("{project_workspace_prefix}/"));
            if !in_workspace && !in_project_workspace {
                return true;
            }
        }
    }
    false
}

fn shell_command_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in command.chars() {
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            ch if ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '(' | ')' | '<' | '>') => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn is_shell_absolute_path(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("\\\\")
        || value.as_bytes().get(1).is_some_and(|byte| *byte == b':')
}

fn normalize_shell_path_for_compare(value: &str) -> String {
    value
        .trim_matches(|ch| matches!(ch, '"' | '\''))
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}
