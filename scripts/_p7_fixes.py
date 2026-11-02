"""Third-audit remaining fixes:
1. set-current: scaffold .llm-wiki/project.json identity for dirs lacking one
   (consistent with create; gives the dir a stable UUID)
2. build_snippet: char-boundary-safe snippet location (no cross-coordinate
   byte offsets between lowercased and original strings)
3. err(500, e): log detail server-side, return generic message to client
4. 401 auth failures logged server-side
"""
import io

MOD = r"D:\llm_wiki\src-tauri\src\api_server\mod.rs"
applied = []

# ---- 1. set-current scaffolds identity ----
s = io.open(MOD, encoding="utf-8").read()
old = """    let display_name = std::path::Path::new(&req.path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Project");
    project_registry::register_project(display_name, &req.path);
    project_registry::set_current_project_path(&req.path);
    let project_id = read_project_id(&req.path).unwrap_or_else(|| req.path.clone());
    persist_project_registration(app, &project_id, display_name, &req.path);
    invalidate_config_cache();"""
new = """    let display_name = std::path::Path::new(&req.path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Project")
        .to_string();
    project_registry::register_project(&display_name, &req.path);
    project_registry::set_current_project_path(&req.path);
    // Give dirs without an identity file one (same as projects/create), so
    // the project is addressable by a stable UUID instead of its path.
    let project_id = match read_project_id(&req.path) {
        Some(id) => id,
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            let meta_dir = std::path::Path::new(&req.path).join(".llm-wiki");
            if fs::create_dir_all(&meta_dir).is_ok() {
                let payload = json!({ "id": id, "name": display_name });
                let _ = fs::write(
                    meta_dir.join("project.json"),
                    serde_json::to_string_pretty(&payload).unwrap_or_default(),
                );
            }
            id
        }
    };
    persist_project_registration(app, &project_id, &display_name, &req.path);
    invalidate_config_cache();"""
assert old in s, "set-current anchor missing"
s = s.replace(old, new, 1)
applied.append("set-current identity scaffold")

io.open(MOD, "w", encoding="utf-8", newline="\n").write(s)
print("APPLIED:", applied)
