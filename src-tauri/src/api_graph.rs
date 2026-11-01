//! Wiki knowledge-graph construction and cache.
//!
//! Extracted from api_server: pure graph engine (no HTTP types). The
//! route handler stays in api_server and calls load_graph/select_graph_page.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use walkdir::WalkDir;

use crate::commands::search::{extract_graph_links, extract_title, relative_to_project};

const GRAPH_CACHE_TTL: Duration = Duration::from_secs(5);
const GRAPH_CACHE_MAX_PROJECTS: usize = 8;

static GRAPH_CACHE: OnceLock<Mutex<HashMap<String, Arc<CachedGraph>>>> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct CachedGraph {
    pub(crate) loaded_at: Instant,
    pub(crate) fingerprint: u64,
    pub(crate) nodes: Vec<ApiGraphNode>,
    pub(crate) edges: Vec<ApiGraphEdge>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiGraphNode {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) node_type: String,
    pub(crate) path: String,
    pub(crate) link_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiGraphEdge {
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) weight: f64,
}

pub(crate) struct GraphPage {
    pub(crate) nodes: Vec<ApiGraphNode>,
    pub(crate) edges: Vec<ApiGraphEdge>,
    pub(crate) total_count: usize,
    pub(crate) has_more: bool,
}


pub(crate) fn select_graph_page(
    nodes: &[ApiGraphNode],
    edges: &[ApiGraphEdge],
    q: Option<&str>,
    node_type: Option<&str>,
    offset: usize,
    limit: usize,
    paged_edges: bool,
) -> GraphPage {
    let matches = |node: &&ApiGraphNode| {
        q.is_none_or(|q| {
            node.id.to_lowercase().contains(q) || node.label.to_lowercase().contains(q)
        }) && node_type.is_none_or(|node_type| node.node_type == node_type)
    };
    let total_count = nodes.iter().filter(matches).count();
    let filtered_ids = paged_edges.then(|| {
        nodes
            .iter()
            .filter(matches)
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>()
    });
    let nodes: Vec<ApiGraphNode> = nodes
        .iter()
        .filter(matches)
        .skip(offset)
        .take(limit)
        .cloned()
        .collect();
    let page_ids: BTreeSet<String> = nodes.iter().map(|node| node.id.clone()).collect();
    let edges = edges
        .iter()
        .filter(|edge| {
            if paged_edges {
                page_ids.contains(&edge.source)
                    && filtered_ids
                        .as_ref()
                        .is_some_and(|ids| ids.contains(&edge.target))
            } else {
                page_ids.contains(&edge.source) && page_ids.contains(&edge.target)
            }
        })
        .cloned()
        .collect();
    let has_more = offset.saturating_add(nodes.len()) < total_count;
    GraphPage {
        nodes,
        edges,
        total_count,
        has_more,
    }
}

pub(crate) fn load_graph(project_path: &str) -> Result<Arc<CachedGraph>, String> {
    let cache = GRAPH_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let cached = cache.lock().ok().and_then(|cache| {
        cache
            .get(project_path)
            .filter(|entry| entry.loaded_at.elapsed() < GRAPH_CACHE_TTL)
            .cloned()
    });
    if let Some(entry) = cached {
        return Ok(entry);
    }

    let (fingerprint, files) = graph_files(project_path)?;
    let cached = cache.lock().ok().and_then(|cache| {
        cache
            .get(project_path)
            .filter(|entry| entry.fingerprint == fingerprint)
            .cloned()
    });
    if let Some(entry) = cached {
        return Ok(entry);
    }

    let (nodes, edges) = build_graph_from_files(project_path, &files)?;
    let graph = Arc::new(CachedGraph {
        loaded_at: Instant::now(),
        fingerprint,
        nodes,
        edges,
    });
    if let Ok(mut cache) = cache.lock() {
        cache.retain(|_, entry| entry.loaded_at.elapsed() < GRAPH_CACHE_TTL);
        if cache.len() >= GRAPH_CACHE_MAX_PROJECTS && !cache.contains_key(project_path) {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.loaded_at)
                .map(|(path, _)| path.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(project_path.to_string(), Arc::clone(&graph));
    }
    Ok(graph)
}

pub(crate) fn graph_files(project_path: &str) -> Result<(u64, Vec<PathBuf>), String> {
    let wiki_root = Path::new(project_path).join("wiki");
    let mut files = Vec::new();
    for entry in WalkDir::new(&wiki_root).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case("md"))
        {
            files.push(entry.into_path());
        }
    }
    files.sort();

    let mut hasher = DefaultHasher::new();
    for path in &files {
        path.hash(&mut hasher);
        if let Ok(metadata) = fs::metadata(path) {
            metadata.len().hash(&mut hasher);
            metadata.modified().ok().hash(&mut hasher);
        }
    }
    Ok((hasher.finish(), files))
}

#[cfg(test)]
pub(crate) fn build_graph(project_path: &str) -> Result<(Vec<ApiGraphNode>, Vec<ApiGraphEdge>), String> {
    let (_, files) = graph_files(project_path)?;
    build_graph_from_files(project_path, &files)
}

pub(crate) fn build_graph_from_files(
    project_path: &str,
    files: &[PathBuf],
) -> Result<(Vec<ApiGraphNode>, Vec<ApiGraphEdge>), String> {
    let wiki_root = Path::new(project_path).join("wiki");
    let mut stem_counts = HashMap::new();
    for path in files {
        if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
            *stem_counts.entry(stem.to_string()).or_insert(0usize) += 1;
        }
    }
    let mut raw: BTreeMap<String, (String, String, String, Vec<String>)> = BTreeMap::new();
    for path in files {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let relative_id = wiki_page_id(&wiki_root, path);
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let id = if stem_counts.get(stem) == Some(&1) {
            stem.to_string()
        } else {
            relative_id
        };
        if id.is_empty() {
            continue;
        }
        let fallback_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let title = extract_title(&content, fallback_name);
        let node_type = extract_type(&content);
        let relative_path = relative_to_project(project_path, path);
        let links = extract_graph_links(&content);
        raw.insert(id, (title, node_type, relative_path, links));
    }
    // Query pages are intermediate artifacts rather than graph nodes. Remove
    // them before link resolution so the API cannot emit edges whose source
    // or target is absent from the returned node set.
    raw.retain(|_, (_, node_type, _, _)| node_type != "query");
    let aliases = graph_aliases(&raw);
    let mut link_count: BTreeMap<String, usize> = raw.keys().map(|id| (id.clone(), 0)).collect();
    let mut seen = BTreeSet::new();
    let mut edges = Vec::new();
    for (source, (_, _, _, links)) in &raw {
        for link in links {
            let Some(target) = resolve_link(link, &aliases) else {
                continue;
            };
            if &target == source {
                continue;
            }
            let key = if source < &target {
                format!("{source}::{target}")
            } else {
                format!("{target}::{source}")
            };
            if seen.insert(key) {
                *link_count.entry(source.clone()).or_default() += 1;
                *link_count.entry(target.clone()).or_default() += 1;
                edges.push(ApiGraphEdge {
                    source: source.clone(),
                    target,
                    weight: 1.0,
                });
            }
        }
    }
    let nodes = raw
        .into_iter()
        .map(|(id, (label, node_type, path, _))| ApiGraphNode {
            link_count: *link_count.get(&id).unwrap_or(&0),
            id,
            label,
            node_type,
            path,
        })
        .collect();
    Ok((nodes, edges))
}

pub(crate) fn wiki_page_id(wiki_root: &Path, path: &Path) -> String {
    path.strip_prefix(wiki_root)
        .unwrap_or(path)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/")
}

pub(crate) struct GraphAliases {
    pub(crate) exact_ids: HashMap<String, String>,
    pub(crate) ids: HashMap<String, Option<String>>,
    pub(crate) stems: HashMap<String, Option<String>>,
    pub(crate) titles: HashMap<String, Option<String>>,
}

pub(crate) fn graph_aliases(raw: &BTreeMap<String, (String, String, String, Vec<String>)>) -> GraphAliases {
    let mut aliases = GraphAliases {
        exact_ids: HashMap::new(),
        ids: HashMap::new(),
        stems: HashMap::new(),
        titles: HashMap::new(),
    };
    for (id, (title, _, path, _)) in raw {
        aliases.exact_ids.insert(id.clone(), id.clone());
        insert_graph_alias(&mut aliases.ids, name_alias_keys(id), id);
        let canonical_path = path
            .strip_prefix("wiki/")
            .unwrap_or(path)
            .trim_end_matches(".md");
        insert_graph_alias(&mut aliases.ids, name_alias_keys(canonical_path), id);
        if let Some(stem) = id.rsplit('/').next() {
            insert_graph_alias(&mut aliases.stems, name_alias_keys(stem), id);
        }
        insert_graph_alias(&mut aliases.titles, name_alias_keys(title), id);
    }
    aliases
}

pub(crate) fn insert_graph_alias(aliases: &mut HashMap<String, Option<String>>, keys: Vec<String>, id: &str) {
    for key in keys {
        match aliases.get(&key) {
            None => {
                aliases.insert(key, Some(id.to_string()));
            }
            Some(Some(existing)) if existing != id => {
                aliases.insert(key, None);
            }
            _ => {}
        }
    }
}

pub(crate) fn link_alias_keys(raw: &str) -> Vec<String> {
    let target = raw
        .split('|')
        .next()
        .unwrap_or("")
        .split('#')
        .next()
        .unwrap_or("")
        .trim();
    name_alias_keys(target)
}

pub(crate) fn name_alias_keys(raw: &str) -> Vec<String> {
    let normalized = raw.trim().replace('\\', "/").to_lowercase();
    let normalized = normalized.trim_matches('/');
    let normalized = normalized.strip_prefix("./").unwrap_or(normalized);
    let normalized = normalized.strip_prefix("wiki/").unwrap_or(normalized);
    let normalized = if normalized.ends_with(".md") {
        &normalized[..normalized.len() - 3]
    } else {
        normalized
    };
    if normalized.is_empty() {
        return Vec::new();
    }
    let slugged = normalized.replace(' ', "-");
    if slugged == normalized {
        vec![normalized.to_string()]
    } else {
        vec![normalized.to_string(), slugged]
    }
}

pub(crate) fn extract_type(content: &str) -> String {
    for line in content.lines() {
        if let Some(value) = line.trim().strip_prefix("type:") {
            return value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_lowercase();
        }
    }
    "other".to_string()
}

pub(crate) fn resolve_link(raw: &str, aliases: &GraphAliases) -> Option<String> {
    let exact = raw
        .split('|')
        .next()
        .unwrap_or("")
        .split('#')
        .next()
        .unwrap_or("")
        .trim();
    if let Some(id) = aliases.exact_ids.get(exact) {
        return Some(id.clone());
    }
    let keys = link_alias_keys(raw);
    for tier in [&aliases.ids, &aliases.stems, &aliases.titles] {
        for key in &keys {
            if let Some(resolved) = tier.get(key) {
                return resolved.clone();
            }
        }
    }
    None
}

pub(crate) fn invalidate_graph_cache(project_path: &str) {
    if let Some(cache) = GRAPH_CACHE.get() {
        if let Ok(mut cache) = cache.lock() {
            cache.remove(project_path);
        }
    }
}

