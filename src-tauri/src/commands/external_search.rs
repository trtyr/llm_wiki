/// Frontend-facing search result shape. The Rust Agent uses
/// `AgentReference` internally, but UI/deep-research code historically
/// consumes `{ title, url, snippet, source }`; keep that wire contract
/// stable while moving provider/network logic to Rust.
pub(crate) fn file_url_for_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.is_empty() || normalized.contains("://") {
        return normalized;
    }
    if normalized.starts_with("//") {
        return format!("file:{normalized}");
    }
    if normalized.len() >= 3
        && normalized.as_bytes()[1] == b':'
        && normalized.as_bytes()[2] == b'/'
        && normalized.as_bytes()[0].is_ascii_alphabetic()
    {
        return format!("file:///{}", encode_file_url_path(&normalized));
    }
    if normalized.starts_with('/') {
        return format!("file://{}", encode_file_url_path(&normalized));
    }
    normalized
}

fn encode_file_url_path(path: &str) -> String {
    path.split('/')
        .map(percent_encode_file_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn percent_encode_file_segment(segment: &str) -> String {
    let mut out = String::new();
    for byte in segment.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'.' | b'_' | b'~' | b':') {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::file_url_for_path;

    #[test]
    fn anytxt_paths_are_returned_as_file_urls_for_frontend_results() {
        assert_eq!(
            file_url_for_path(r"C:\docs\煤矿 安全.pdf"),
            "file:///C:/docs/%E7%85%A4%E7%9F%BF%20%E5%AE%89%E5%85%A8.pdf"
        );
        assert_eq!(
            file_url_for_path("/Users/me/docs/a b.txt"),
            "file:///Users/me/docs/a%20b.txt"
        );
        assert_eq!(file_url_for_path("anytxt://99"), "anytxt://99");
    }
}
