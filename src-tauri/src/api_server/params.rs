//! URL/request parameter parsing helpers (split, percent-decode).

use super::*;

pub(super) fn split_url(url: &str) -> (String, &str) {
    match url.split_once('?') {
        Some((path, query)) => (path.to_string(), query),
        None => (url.to_string(), ""),
    }
}

pub(crate) fn parse_query(query: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode(k), percent_decode(v));
    }
    out
}

pub(super) fn percent_decode(input: &str) -> String {
    // Byte-wise: slicing the &str at i+1..i+3 could land mid-character and
    // panic for non-ASCII input. Hex decoding works on bytes; anything that
    // is not a valid escape passes through, and invalid UTF-8 output becomes
    // replacement characters instead of panicking.
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        // RFC 3986: `+` is a literal character in path/query strings; only
        // %XX escapes are decoded. Translating `+` to a space corrupted
        // file paths containing plus signs and tokens with base64 `+`.
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Redact credential-bearing query values (`?token=`, keys, secrets) from a
/// URL before it is logged. `?token=` is an officially supported auth
/// channel, so raw URLs in logs would persist live credentials (F-02).
pub(super) fn redact_query_token(url: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, q),
        None => return url.to_string(),
    };
    let redacted: Vec<String> = query
        .split('&')
        .map(|pair| {
            let (k, _) = pair.split_once('=').unwrap_or((pair, ""));
            let lower = k.to_ascii_lowercase();
            let sensitive = lower == "token"
                || lower == "key"
                || lower == "api_key"
                || lower == "api-key"
                || lower.ends_with("apikey")
                || lower.ends_with("secret");
            if sensitive {
                format!("{k}=***")
            } else {
                pair.to_string()
            }
        })
        .collect();
    if redacted.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", redacted.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plus_is_literal_in_query_values() {
        let q = parse_query("path=wiki/c%2B%2B-notes.md");
        assert_eq!(q.get("path").map(String::as_str), Some("wiki/c++-notes.md"));
        let q = parse_query("path=wiki/c++-notes.md");
        assert_eq!(q.get("path").map(String::as_str), Some("wiki/c++-notes.md"));
        let q = parse_query("token=ab+cd");
        assert_eq!(q.get("token").map(String::as_str), Some("ab+cd"));
    }

    #[test]
    fn invalid_escapes_pass_through() {
        let q = parse_query("path=wiki/a%ZZ.md");
        assert_eq!(q.get("path").map(String::as_str), Some("wiki/a%ZZ.md"));
    }

    #[test]
    fn redaction_covers_token_and_aliases() {
        assert_eq!(
            redact_query_token("/api/v1/projects/p/pages/write?token=hunter2&x=1"),
            "/api/v1/projects/p/pages/write?token=***&x=1"
        );
        assert_eq!(
            redact_query_token("/api/v1/x?api_key=abc"),
            "/api/v1/x?api_key=***"
        );
        assert_eq!(redact_query_token("/health"), "/health");
    }
}
