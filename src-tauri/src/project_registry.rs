//! Process-wide current-project registry.
//!
//! Originally the state of the Chrome-extension clip server; since the
//! extension (and the clip server) were removed, this remains as the
//! lightweight way for the headless server (`LLM_WIKI_PROJECT_PATH`) and
//! future API routes to pin which project "current" refers to.

use std::sync::Mutex;

static CURRENT_PROJECT: Mutex<String> = Mutex::new(String::new());
static ALL_PROJECTS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new()); // (name, path)

pub fn current_project_path() -> String {
    CURRENT_PROJECT
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// Point project resolution at an explicit project directory. Used by the
/// headless server via `LLM_WIKI_PROJECT_PATH`.
pub fn set_current_project_path(path: &str) {
    if let Ok(mut guard) = CURRENT_PROJECT.lock() {
        *guard = path.replace('\\', "/");
    }
}

#[allow(dead_code)] // reserved: projects listed by future API routes
pub fn all_projects() -> Vec<(String, String)> {
    ALL_PROJECTS
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}
