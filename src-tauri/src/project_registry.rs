//! Process-wide current-project registry.
//!
//! Tracks which project "current" refers to for this server process.
//! Registrations made through the API are persisted into app-state.json
//! `projectRegistry` (see persist_project_registration in api_server);
//! `LLM_WIKI_PROJECT_PATH` pins the startup project for the headless
//! server.

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

/// Permanently list a project in this server session (so it stays
/// addressable even after `current` moves to another project).
pub fn register_project(name: &str, path: &str) {
    if let Ok(mut guard) = ALL_PROJECTS.lock() {
        let normalized = path.replace('\\', "/");
        if !guard.iter().any(|(_, existing)| existing == &normalized) {
            guard.push((name.to_string(), normalized));
        }
    }
}
