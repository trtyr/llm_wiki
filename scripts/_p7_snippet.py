"""Fix build_snippet: char-boundary-safe case-insensitive matching.

The old code found the match offset in the LOWERCASED string, then tried
to place it into the ORIGINAL string's char positions. When to_lowercase
changes byte length (Turkish dotted I, sharp s, ...), the coordinates
disagree and the snippet window lands dozens of characters off.
"""
import io

P = r"D:\llm_wiki\src-tauri\src\commands\search.rs"
s = io.open(P, encoding="utf-8").read()

old = """pub fn build_snippet(content: &str, query: &str) -> String {
    let lower = content.to_lowercase();
    let q = query.to_lowercase();
    let idx = lower.find(&q).unwrap_or(0);
    let char_positions: Vec<usize> = content.char_indices().map(|(idx, _)| idx).collect();
    if char_positions.is_empty() {
        return String::new();
    }
    let match_char = char_positions
        .iter()
        .position(|byte| *byte >= idx)
        .unwrap_or(char_positions.len().saturating_sub(1));
    let query_chars = query.chars().count().max(1);
    let start_char = match_char.saturating_sub(SNIPPET_CONTEXT);
    let end_char = (match_char + query_chars + SNIPPET_CONTEXT).min(char_positions.len());
    let start = char_positions[start_char];
    let end = if end_char < char_positions.len() {
        char_positions[end_char]
    } else {
        content.len()
    };
    let mut snippet = content[start..end].replace('\\n', " ");
    if start > 0 {
        snippet = format!("...{snippet}");
    }
    if end < content.len() {
        snippet.push_str("...");
    }
    snippet
}"""

new = """pub fn build_snippet(content: &str, query: &str) -> String {
    // Case-insensitive match done over CHARS, not bytes: to_lowercase can
    // change byte lengths (Turkish dotted I, sharp s), so any byte offset
    // taken from the lowercased string would land at the wrong char in the
    // original. Matching char-by-char keeps both coordinates aligned.
    let lower_content: Vec<char> = content.to_lowercase().chars().collect();
    let lower_query: Vec<char> = query.to_lowercase().chars().collect();
    if lower_query.is_empty() || lower_content.is_empty() {
        return String::new();
    }
    let mut match_char = 0;
    for start in 0..=(lower_content.len() - lower_query.len()) {
        if lower_content[start..start + lower_query.len()] == lower_query[..] {
            match_char = start;
            break;
        }
    }
    let char_positions: Vec<usize> = content.char_indices().map(|(idx, _)| idx).collect();
    if char_positions.is_empty() {
        return String::new();
    }
    let query_chars = query.chars().count().max(1);
    let start_char = match_char.saturating_sub(SNIPPET_CONTEXT);
    let end_char = (match_char + query_chars + SNIPPET_CONTEXT).min(char_positions.len());
    let start = char_positions[start_char];
    let end = if end_char < char_positions.len() {
        char_positions[end_char]
    } else {
        content.len()
    };
    let mut snippet = content[start..end].replace('\\n', " ");
    if start > 0 {
        snippet = format!("...{snippet}");
    }
    if end < content.len() {
        snippet.push_str("...");
    }
    snippet
}"""

assert old in s, "build_snippet anchor missing"
s = s.replace(old, new, 1)
io.open(P, "w", encoding="utf-8", newline="\n").write(s)
print("build_snippet char-safe rewrite applied")
