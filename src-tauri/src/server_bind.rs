use std::fs;
use std::path::Path;

use serde_json::Value;

const DEFAULT_BIND_HOST: &str = "127.0.0.1";
const PUBLIC_BIND_HOST: &str = "0.0.0.0";
const BIND_HOST_ENV: &str = "LLM_WIKI_BIND_HOST";
const API_PORT_ENV: &str = "LLM_WIKI_BIND_PORT";
pub const DEFAULT_API_PORT: u16 = 19828;

/// Resolve the host the HTTP servers should bind to:
/// `LLM_WIKI_BIND_HOST` env wins, then the `allowLanAccess` flag in
/// `app-state.json` (when the host process has one), then loopback.
pub fn configured_bind_host(store_path: Option<&Path>) -> String {
    configured_env_bind_host()
        .or_else(|| configured_store_bind_host(store_path))
        .unwrap_or_else(|| DEFAULT_BIND_HOST.to_string())
}

/// API server port: `LLM_WIKI_BIND_PORT` env override, then 19828.
pub fn configured_api_port() -> u16 {
    configured_env_port(API_PORT_ENV).unwrap_or(DEFAULT_API_PORT)
}

fn configured_env_bind_host() -> Option<String> {
    std::env::var(BIND_HOST_ENV)
        .ok()
        .and_then(|value| sanitize_bind_host(&value))
}

fn configured_store_bind_host(store_path: Option<&Path>) -> Option<String> {
    let path = store_path?;
    let raw = fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    if allow_lan_access_from_state(&parsed) {
        Some(PUBLIC_BIND_HOST.to_string())
    } else {
        None
    }
}

fn configured_env_port(env_key: &str) -> Option<u16> {
    let raw = std::env::var(env_key).ok()?;
    parse_port_value(env_key, &raw)
}

/// Port 0 would silently pick a random port; that's never what a server
/// operator wants from a config value, so reject it loudly.
fn parse_port_value(env_key: &str, raw: &str) -> Option<u16> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.parse::<u16>() {
        Ok(port @ 1..=65535) => Some(port),
        _ => {
            eprintln!(
                "[server-bind] ignoring invalid {env_key} value {raw:?} (expected 1-65535)"
            );
            None
        }
    }
}

pub fn bind_addr(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn sanitize_bind_host(value: &str) -> Option<String> {
    let host = value.trim();
    if host.is_empty() {
        return None;
    }
    let valid = host
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ':' | '[' | ']'));
    if valid {
        Some(host.to_string())
    } else {
        None
    }
}

fn allow_lan_access_from_state(value: &Value) -> bool {
    value
        .get("apiConfig")
        .and_then(|config| config.get("allowLanAccess"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        allow_lan_access_from_state, bind_addr, parse_port_value, sanitize_bind_host,
    };

    #[test]
    fn sanitize_bind_host_accepts_common_lan_hosts() {
        assert_eq!(sanitize_bind_host("0.0.0.0"), Some("0.0.0.0".to_string()));
        assert_eq!(
            sanitize_bind_host("  192.168.1.10  "),
            Some("192.168.1.10".to_string())
        );
        assert_eq!(sanitize_bind_host("::1"), Some("::1".to_string()));
        assert_eq!(sanitize_bind_host("[::]"), Some("[::]".to_string()));
    }

    #[test]
    fn sanitize_bind_host_rejects_empty_or_address_injection() {
        assert_eq!(sanitize_bind_host(""), None);
        assert_eq!(sanitize_bind_host("0.0.0.0:19828/path"), None);
        assert_eq!(sanitize_bind_host("127.0.0.1;rm"), None);
    }

    #[test]
    fn bind_addr_wraps_unbracketed_ipv6_hosts() {
        assert_eq!(bind_addr("127.0.0.1", 19828), "127.0.0.1:19828");
        assert_eq!(bind_addr("0.0.0.0", 19828), "0.0.0.0:19828");
        assert_eq!(bind_addr("::1", 19828), "[::1]:19828");
        assert_eq!(bind_addr("[::]", 19828), "[::]:19828");
    }

    #[test]
    fn allow_lan_access_reads_api_config_flag() {
        assert!(allow_lan_access_from_state(&json!({
            "apiConfig": { "allowLanAccess": true }
        })));
        assert!(!allow_lan_access_from_state(&json!({
            "apiConfig": { "allowLanAccess": false }
        })));
        assert!(!allow_lan_access_from_state(&json!({})));
    }

    #[test]
    fn port_values_accept_valid_and_reject_degenerate_input() {
        assert_eq!(parse_port_value("LLM_WIKI_BIND_PORT", " 9000 "), Some(9000));
        assert_eq!(parse_port_value("LLM_WIKI_BIND_PORT", "1"), Some(1));
        assert_eq!(parse_port_value("LLM_WIKI_BIND_PORT", "65535"), Some(65535));
        assert_eq!(parse_port_value("LLM_WIKI_BIND_PORT", "0"), None);
        assert_eq!(parse_port_value("LLM_WIKI_BIND_PORT", "65536"), None);
        assert_eq!(parse_port_value("LLM_WIKI_BIND_PORT", ""), None);
        assert_eq!(parse_port_value("LLM_WIKI_BIND_PORT", "  "), None);
        assert_eq!(parse_port_value("LLM_WIKI_BIND_PORT", "80abc"), None);
        assert_eq!(parse_port_value("LLM_WIKI_BIND_PORT", "-1"), None);
    }
}
