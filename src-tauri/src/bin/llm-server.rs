//! Headless LLM Wiki server binary: same Rust core as the desktop app,
//! no GUI. See `src-tauri/src/headless.rs` for configuration.

fn main() {
    let code = llm_wiki_lib::headless::run_headless_server();
    std::process::exit(code);
}
