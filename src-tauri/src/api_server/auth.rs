//! API authentication/authorization: token extraction, comparison and
//! the allowUnauthenticated / enabled / mcp switches.
//!
//! Child module of api_server: reaches the parent's app-state cache and
//! helpers through `super::`.

use super::*;

pub(super) fn is_authorized(app: &ServerHost, query: &str, headers: &[(String, String)]) -> bool {
    if !api_auth_required(app) {
        return true;
    }
    is_token_authorized(app, query, headers)
}

pub(crate) fn is_token_authorized(
    app: &ServerHost,
    query: &str,
    headers: &[(String, String)],
) -> bool {
    let Some(token) = api_token(app) else {
        return false;
    };
    let params = parse_query(query);
    if params
        .get("token")
        .map(|v| constant_time_eq(v.as_bytes(), token.as_bytes()))
        .unwrap_or(false)
    {
        return true;
    }
    headers.iter().any(|(key, value)| {
        if key == "x-llm-wiki-token" {
            return constant_time_eq(value.as_bytes(), token.as_bytes());
        }
        if key == "authorization" {
            return value
                .strip_prefix("Bearer ")
                .map(|v| constant_time_eq(v.as_bytes(), token.as_bytes()))
                .unwrap_or(false);
        }
        false
    })
}

pub(super) fn api_token(app: &ServerHost) -> Option<String> {
    if let Ok(token) = std::env::var("LLM_WIKI_API_TOKEN") {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let parsed = load_app_state(app)?;
    parsed
        .get("apiConfig")
        .and_then(|v| v.get("token"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn api_token_source(app: &ServerHost) -> &'static str {
    if let Ok(token) = std::env::var("LLM_WIKI_API_TOKEN") {
        if !token.trim().is_empty() {
            return "env";
        }
    }
    if load_app_state(app)
        .and_then(|parsed| {
            parsed
                .get("apiConfig")
                .and_then(|v| v.get("token"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|_| ())
        })
        .is_some()
    {
        return "store";
    }
    "none"
}

pub(super) fn api_auth_required(app: &ServerHost) -> bool {
    !api_allow_unauthenticated(app)
}

pub(super) fn api_allow_unauthenticated(app: &ServerHost) -> bool {
    let Some(parsed) = load_app_state(app) else {
        return false;
    };
    parsed
        .get("apiConfig")
        .and_then(|v| v.get("allowUnauthenticated"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(super) fn api_allow_lan_access(app: &ServerHost) -> bool {
    let Some(parsed) = load_app_state(app) else {
        return false;
    };
    parsed
        .get("apiConfig")
        .and_then(|v| v.get("allowLanAccess"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether the API server should accept non-/health requests.
///
/// Defaults to `true` when no config has been written yet — keeps
/// existing setups (env-token-only, hand-edited app-state.json) working
/// after the kill-switch was introduced. New users still land in
/// "enabled + no token = 401" which is fail-closed by virtue of the
/// missing token, not the enable flag.
pub(super) fn api_enabled(app: &ServerHost) -> bool {
    let Some(parsed) = load_app_state(app) else {
        return true;
    };
    parsed
        .get("apiConfig")
        .and_then(|v| v.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

pub(super) fn api_mcp_enabled(app: &ServerHost) -> bool {
    let Some(parsed) = load_app_state(app) else {
        return false;
    };
    parsed
        .get("apiConfig")
        .and_then(|v| v.get("mcpEnabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(super) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();
    for i in 0..max_len {
        let a = left.get(i).copied().unwrap_or(0);
        let b = right.get(i).copied().unwrap_or(0);
        diff |= (a ^ b) as usize;
    }
    diff == 0
}

