//! Importers: herdr, omp sessions, git repos, Obsidian vault, Zen spaces, mnemopi memory.
//! Faithful port of atlas/importers.py — all idempotent, recomputed and upserted.
use crate::cfg;
use crate::db::Db;
use crate::delegate;
use crate::embed;
use crate::util::{cap_chars, iso_to_unix, norm, now, run_out};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

pub const MAX_NOTE_BYTES: usize = 8 * 1024;
pub const MAX_SESSION_TAIL: u64 = 512 * 1024;
pub const TEXT_CAP: usize = 4000;

pub const SEED_LINKS: [(&str, &str, &str); 5] = [
    ("emigate", "lamplife", "strict"),
    ("finance", "lamplife", "strict"),
    ("emigate", "jobseeker", "strict"),
    ("localmix", "zen-tabs", "soft"),
    ("finance", "zen-tabs", "soft"),
];

/// Global sync lock (Python `_sync_lock`): one sync_all at a time.
pub static SYNC_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

// --------------------------------------------------------------------------
// delegation worktrees
// --------------------------------------------------------------------------

/// Root of the delegate worktrees — `delegate.rs::worktree_dir` puts every
/// delegation checkout at `<worktree root>/<project>-<item id>-<4 hex of
/// hash>` (root: config `paths.worktrees`, default `~/.atlas-wt`).
pub fn delegation_root() -> PathBuf {
    cfg::worktree_root()
}

/// The parsed `.atlas-deleg.json` marker of a delegation worktree (written by
/// `delegate.rs` at worktree creation): which item/project it serves. Unknown
/// fields (`branch`) are ignored.
#[derive(Debug, PartialEq, serde::Deserialize)]
pub struct DelegMarker {
    /// The delegated board item.
    pub item: i64,
    /// The project the worktree was cut from.
    pub project: String,
}

/// The marker of `path`, when it exists and parses; `None` when absent or
/// corrupt — both fall through to the legacy name rule.
pub fn delegation_marker(path: &Path) -> Option<DelegMarker> {
    let text = std::fs::read_to_string(path.join(delegate::DELEG_MARKER)).ok()?;
    serde_json::from_str(&text).ok()
}

/// True for a delegation worktree: a dir carrying a valid `.atlas-deleg.json`
/// marker, or — legacy — the delegation root itself and every path below it.
///
/// A worktree is a transient `git worktree` of a real project, so it must never
/// surface as a project of its own: the omp session of a worktree run encodes
/// the worktree path as its slug, which used to mint a phantom project row
/// (`zz-atlas-deleg-19-377e` for a delegation of item 19 of `zz-atlas-deleg`).
///
/// The marker is asked first (FRAGILE §2): recognition no longer hangs on the
/// dir name. The root-anchored test stays component-wise — a directory merely
/// NAMED `.atlas-wt` deeper inside a project is an ordinary path.
pub fn is_delegation_worktree(path: &Path) -> bool {
    is_delegation_worktree_in(path, &delegation_root())
}

/// Injected-root core of [`is_delegation_worktree`], the file's testable style.
fn is_delegation_worktree_in(path: &Path, wt_root: &Path) -> bool {
    if delegation_marker(path).is_some() {
        return true;
    }
    // legacy: ворктри до маркера; удалить, когда старых не останется
    is_under(path, wt_root)
}

/// `path == root` or `root` is a component-wise prefix of `path`.
fn is_under(path: &Path, root: &Path) -> bool {
    let mut rest = path.components();
    for c in root.components() {
        match rest.next() {
            Some(r) if r.as_os_str() == c.as_os_str() => {}
            _ => return false,
        }
    }
    true
}

// --------------------------------------------------------------------------
// omp slug -> project dir resolution
// --------------------------------------------------------------------------

fn join_options(n: usize) -> Vec<Vec<bool>> {
    // Python: sorted(product(("/", "-"), repeat=n-1), key=count("/"), reverse=True)
    // Stable sort ⇒ within equal slash counts the product order is kept
    // (last separator varies fastest).
    if n <= 1 {
        return vec![vec![]];
    }
    let seps = n - 1;
    let mut opts: Vec<Vec<bool>> = Vec::with_capacity(1 << seps); // true = '/'
    for i in 0..(1usize << seps) {
        // product order: last position varies fastest
        let mut v = Vec::with_capacity(seps);
        for pos in 0..seps {
            v.push((i >> (seps - 1 - pos)) & 1 == 1);
        }
        opts.push(v);
    }
    // Stable sort, descending by slash count — ties keep product order,
    // exactly like Python's sorted(..., key=count, reverse=True).
    opts.sort_by(|a, b| {
        let ca = a.iter().filter(|&&s| s).count();
        let cb = b.iter().filter(|&&s| s).count();
        cb.cmp(&ca)
    });
    opts
}

/// Resolve a session slug to a project directory. `home` is the prefix
/// `-`-rooted slugs are rebuilt against (a slug that does not start with `-` is
/// read from the filesystem root), `projects_root` the fallback search root and
/// `wt_root` the delegation root. Every root is injected so the resolver is
/// testable without touching `$HOME`.
///
/// `allow_worktrees` keeps candidates under `wt_root` (only the worktree
/// attribution pass asks for that); every project-discovery caller must see a
/// delegation worktree as non-existent — its session belongs to a real project,
/// never to a new one.
fn resolve_slug_in(
    slug: &str,
    home: &str,
    projects_root: &Path,
    wt_root: &Path,
    allow_worktrees: bool,
) -> Option<String> {
    if slug == "-" || slug == "-tmp" || slug.starts_with("--") {
        return None;
    }
    let body = slug.strip_prefix('-').unwrap_or(slug);
    if body.is_empty() {
        return None;
    }
    let base: &str = if slug.starts_with('-') { home } else { "/" };
    let tokens: Vec<&str> = body.split('-').collect();
    let n = tokens.len();
    for joins in join_options(n) {
        let mut cand = if tokens[0].is_empty() {
            base.to_string()
        } else if base == "/" {
            format!("/{}", tokens[0])
        } else {
            format!("{}/{}", base, tokens[0])
        };
        for (tok, sep) in tokens[1..].iter().zip(joins.iter()) {
            cand.push(if *sep { '/' } else { '-' });
            cand.push_str(tok);
        }
        if !allow_worktrees && is_under(Path::new(&cand), wt_root) {
            continue;
        }
        if Path::new(&cand).is_dir() {
            return Some(cand);
        }
    }
    if n > 10 {
        return None;
    }
    // Fallback: shallow exact-name match under ~/Projects, longest tail first.
    // Python glob "*/"*d + tail with d in 1..=3 puts tail at depth 2..=4.
    for k in (1..=n.min(3)).rev() {
        let tail = tokens[tokens.len() - k..].join("-");
        for depth in 2..=4usize {
            if let Some(hit) = find_at_depth(projects_root, depth, &tail) {
                if !allow_worktrees && is_under(&hit, wt_root) {
                    continue;
                }
                if hit.is_dir() {
                    return Some(hit.to_string_lossy().into_owned());
                }
            }
        }
    }
    None
}

/// Final path component, `""` when there is none.
fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// First directory named `name` exactly `depth` levels below `root` (BFS).
fn find_at_depth(root: &Path, depth: usize, name: &str) -> Option<PathBuf> {
    let mut level: Vec<PathBuf> = vec![root.to_path_buf()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for dir in &level {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut kids: Vec<PathBuf> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            kids.sort();
            next.extend(kids);
        }
        level = next;
    }
    level
        .into_iter()
        .find(|p| p.file_name().map(|f| f == name).unwrap_or(false))
}

// --------------------------------------------------------------------------
// herdr
// --------------------------------------------------------------------------

fn empty_herdr() -> Value {
    json!({
        "ok": false,
        "workspaces": [],
        "agents": [],
        "tabs": [],
        "focused_workspace_id": null,
        "focused_tab_id": null,
    })
}

pub async fn import_herdr() -> Value {
    let argv = vec![cfg::HERDR_BIN.clone(), "api".into(), "snapshot".into()];
    let Some(raw) = run_out(&argv, Duration::from_secs(25)).await else {
        return empty_herdr();
    };
    let Some(start) = raw.find('{') else {
        return empty_herdr();
    };
    let Ok(data) = serde_json::from_str::<Value>(&raw[start..]) else {
        return empty_herdr();
    };
    let snap = data
        .get("result")
        .and_then(|r| r.get("snapshot"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "ok": true,
        "workspaces": snap.get("workspaces").cloned().unwrap_or_else(|| json!([])),
        "agents": snap.get("agents").cloned().unwrap_or_else(|| json!([])),
        // tabs + focused ids feed the herdr panel (/api/herdr); herdr_by_project
        // ignores them, so carrying them here keeps ONE snapshot parse.
        "tabs": snap.get("tabs").cloned().unwrap_or_else(|| json!([])),
        "focused_workspace_id": snap.get("focused_workspace_id").cloned().unwrap_or(Value::Null),
        "focused_tab_id": snap.get("focused_tab_id").cloned().unwrap_or(Value::Null),
    })
}

pub struct HerdrEntry {
    pub workspace_id: Option<String>,
    pub agent_status: Option<String>,
    pub agents_count: i64,
    pub agents: Vec<Value>,
}

/// Map project name -> {workspace_id, agent_status, agents_count, agents}.
pub fn herdr_by_project(herdr: &Value) -> Vec<(String, HerdrEntry)> {
    // Preserves Python's insertion order via the ordered serde map.
    let mut out: Map<String, Value> = Map::new();

    let agents = herdr
        .get("agents")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    let mut agents_by_ws: HashMap<String, Vec<Value>> = HashMap::new();
    let mut cwd_agents: HashMap<String, Vec<Value>> = HashMap::new();
    for a in &agents {
        let pane = a.get("pane_id").and_then(|p| p.as_str()).unwrap_or("");
        let ws = pane.split(':').next().unwrap_or("");
        if !ws.is_empty() {
            agents_by_ws
                .entry(ws.to_string())
                .or_default()
                .push(a.clone());
        }
        let cwd = a
            .get("cwd")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim_end_matches('/');
        let base = Path::new(cwd)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !base.is_empty() {
            cwd_agents.entry(base).or_default().push(a.clone());
        }
    }

    let workspaces = herdr
        .get("workspaces")
        .and_then(|w| w.as_array())
        .cloned()
        .unwrap_or_default();
    for w in &workspaces {
        let wid = w
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let label = w.get("label").and_then(|v| v.as_str()).unwrap_or("");
        let key = norm(label);
        let entry = out
            .entry(key.clone())
            .or_insert_with(|| json!({"workspace_id": wid, "agent_status": w.get("agent_status")}))
            .clone();
        let mut obj = entry;
        let ws_agents = wid
            .as_deref()
            .and_then(|id| agents_by_ws.get(id))
            .cloned()
            .unwrap_or_default();
        obj["agents"] = json!(ws_agents);
        obj["agents_count"] = json!(ws_agents.len() as i64);
        if let Some(first) = ws_agents.first() {
            if truthy(first.get("agent_status")) {
                obj["agent_status"] = first.get("agent_status").cloned().unwrap_or(Value::Null);
            }
        }
        out.insert(key, obj);
    }

    for (base, ags) in &cwd_agents {
        let key = norm(base);
        let entry = out
            .entry(key.clone())
            .or_insert_with(|| json!({"workspace_id": Value::Null, "agent_status": Value::Null}))
            .clone();
        let mut obj = entry;
        // Merge by terminal_id, preserving first-seen order.
        let existing = obj
            .get("agents")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        let mut merged_keys: Vec<String> = Vec::new();
        let mut merged: HashMap<String, Value> = HashMap::new();
        for a in existing {
            let k = terminal_key(&a);
            if !merged.contains_key(&k) {
                merged_keys.push(k.clone());
            }
            merged.entry(k).or_insert(a);
        }
        for a in ags {
            let k = terminal_key(a);
            if !merged.contains_key(&k) {
                merged_keys.push(k.clone());
                merged.insert(k, a.clone());
            }
        }
        let merged_list: Vec<Value> = merged_keys
            .iter()
            .filter_map(|k| merged.get(k).cloned())
            .collect();
        obj["agents"] = json!(merged_list);
        obj["agents_count"] = json!(merged_list.len() as i64);
        if !truthy(obj.get("agent_status")) {
            if let Some(first) = ags.first() {
                obj["agent_status"] = first.get("agent_status").cloned().unwrap_or(Value::Null);
            }
        }
        out.insert(key, obj);
    }

    out.into_iter()
        .map(|(k, mut v)| {
            if v.get("agents_count").map(|c| c.is_null()).unwrap_or(true) {
                let n = v
                    .get("agents")
                    .and_then(|a| a.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                v["agents_count"] = json!(n as i64);
            }
            let workspace_id = v
                .get("workspace_id")
                .and_then(|w| w.as_str())
                .map(String::from);
            let agent_status = v
                .get("agent_status")
                .and_then(|s| s.as_str())
                .map(String::from);
            let agents_count = v.get("agents_count").and_then(|c| c.as_i64()).unwrap_or(0);
            let agents = v
                .get("agents")
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default();
            (
                k,
                HerdrEntry {
                    workspace_id,
                    agent_status,
                    agents_count,
                    agents,
                },
            )
        })
        .collect()
}

/// Python truthiness for the JSON values we actually compare (None/""/0/[] falsy).
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::Bool(b)) => *b,
    }
}

fn terminal_key(a: &Value) -> String {
    match a.get("terminal_id") {
        None | Some(Value::Null) => "null".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

// --------------------------------------------------------------------------
// omp sessions
// --------------------------------------------------------------------------

pub struct SlugInfo {
    pub project: Option<String>,
    pub dir_path: Option<String>,
    pub count: i64,
    pub latest_file: Option<String>,
    pub latest_mtime: f64,
}

/// slug -> info, insertion order = sorted dir names (matches Python).
pub fn import_sessions() -> (Vec<(String, SlugInfo)>, i64) {
    let dir = cfg::sessions_dir();
    if !dir.is_dir() {
        return (Vec::new(), 0);
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut total: i64 = 0;
    // (dir name, jsonl count, newest jsonl, its mtime)
    let mut raw: Vec<(String, i64, Option<String>, f64)> = Vec::with_capacity(names.len());
    for name in names {
        let d = dir.join(&name);
        let mut latest_file: Option<String> = None;
        let mut latest_mtime = 0.0f64;
        let mut count: i64 = 0;
        let mut files: Vec<PathBuf> = std::fs::read_dir(&d)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        for f in files {
            count += 1;
            let m = match f.metadata() {
                Ok(md) => md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64()),
                Err(_) => continue,
            };
            if let Some(m) = m {
                if m > latest_mtime {
                    latest_mtime = m;
                    latest_file = Some(f.to_string_lossy().into_owned());
                }
            }
        }
        total += count;
        raw.push((name, count, latest_file, latest_mtime));
    }
    let home = cfg::home().to_string_lossy().into_owned();
    let per_slug = sessions_from(&raw, &home, &cfg::projects_dir(), &delegation_root());
    (per_slug, total)
}

/// Session dir stats -> SlugInfo (roots injected, so it is testable with a
/// fixture slug list and a temp layout).
///
/// Pass 1 resolves every non-worktree slug and collects the project set of this
/// sync; pass 2 gives a delegation-worktree session to the parent project named
/// by its dir name (`<project>-<id>-<hash>`) — but only when that parent is a
/// known project. A worktree is never a project of its own, and an unknown
/// parent drops the session instead of minting a row.
fn sessions_from(
    raw: &[(String, i64, Option<String>, f64)],
    home: &str,
    projects_root: &Path,
    wt_root: &Path,
) -> Vec<(String, SlugInfo)> {
    let mut resolved: Vec<Option<String>> = Vec::with_capacity(raw.len());
    let mut known: HashMap<String, Option<String>> = HashMap::new();
    for (name, ..) in raw {
        let target = resolve_slug_in(name, home, projects_root, wt_root, false);
        if let Some(p) = &target {
            known.entry(file_name(p)).or_insert_with(|| Some(p.clone()));
        }
        resolved.push(target);
    }
    raw.iter()
        .zip(resolved)
        .map(|((name, count, latest_file, latest_mtime), target)| {
            let (project, dir_path) = match target {
                Some(p) => (Some(file_name(&p)), Some(p)),
                None => match worktree_session_parent(name, home, projects_root, wt_root, &known) {
                    Some((parent, dir)) => (Some(parent), dir),
                    None => (None, None),
                },
            };
            (
                name.clone(),
                SlugInfo {
                    project,
                    dir_path,
                    count: *count,
                    latest_file: latest_file.clone(),
                    latest_mtime: *latest_mtime,
                },
            )
        })
        .collect()
}

/// A delegation-worktree session and the project it belongs to: the worktree
/// dir is `<project>-<id>-<4 hex>` under the delegation root, and the session
/// counts towards `<project>` when that is a project we know — one resolved in
/// this sync (`known`, carrying its dir) or an existing `~/Projects/<project>`.
/// Anything else returns `None`: better to drop a session than to invent a
/// project.
fn worktree_session_parent(
    slug: &str,
    home: &str,
    projects_root: &Path,
    wt_root: &Path,
    known: &HashMap<String, Option<String>>,
) -> Option<(String, Option<String>)> {
    let wt = resolve_slug_in(slug, home, projects_root, wt_root, true)?;
    if !is_under(Path::new(&wt), wt_root) {
        return None;
    }
    let parent = delegation_parent(&file_name(&wt))?;
    if let Some(dir) = known.get(&parent) {
        return Some((parent, dir.clone()));
    }
    let disk = projects_root.join(&parent);
    disk.is_dir()
        .then(|| (parent, Some(disk.to_string_lossy().into_owned())))
}

/// `<project>-<item id>-<4 hex of the title hash>` (see `delegate.rs::slug`) ->
/// `<project>`. `None` unless the tail is exactly that shape — a dir named
/// `<x>-<digits>-<4 hex>` is a delegation worktree, nothing else is.
fn delegation_parent(dir_name: &str) -> Option<String> {
    let mut tail = dir_name.rsplitn(3, '-');
    let hash = tail.next()?;
    let id = tail.next()?;
    let parent = tail.next()?;
    let shaped = hash.len() == 4
        && hash.chars().all(|c| c.is_ascii_hexdigit())
        && !id.is_empty()
        && id.chars().all(|c| c.is_ascii_digit());
    (shaped && !parent.is_empty()).then(|| parent.to_string())
}

// --------------------------------------------------------------------------
// git repos
// --------------------------------------------------------------------------

fn git_origin(git_dir: &Path) -> Option<String> {
    let cfgp = git_dir.join("config");
    let text = std::fs::read_to_string(&cfgp).ok()?;
    let mut in_origin = false;
    for line in text.lines() {
        let s = line.trim();
        if s.starts_with('[') {
            in_origin = s.starts_with("[remote") && s.contains("\"origin\"");
        } else if in_origin && s.starts_with("url") {
            return s.splitn(2, '=').nth(1).map(|u| u.trim().to_string());
        }
    }
    None
}

/// [{name, path, origin}] for every repo under ~/Projects up to depth 4.
pub fn import_git() -> Vec<Value> {
    let root = cfg::projects_dir();
    let mut repos: Vec<Value> = Vec::new(); // shallowest occurrence wins
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    walk_git(&root, 0, &delegation_root(), &mut repos, &mut seen);
    repos
}

fn walk_git(
    dir: &Path,
    depth: usize,
    wt_root: &Path,
    repos: &mut Vec<Value>,
    seen: &mut std::collections::HashSet<String>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for p in &entries {
        let name = p
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        if p.is_dir() {
            if name == ".git" {
                let git_dir = dir.join(".git");
                // A linked worktree carries a `.git` FILE, but a delegation dir
                // can hold an ordinary clone of its own — still not a project.
                if git_dir.is_dir() && !is_under(dir, wt_root) {
                    let dn = dir
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if seen.insert(dn.clone()) {
                        repos.push(json!({
                            "name": dn,
                            "path": dir.to_string_lossy(),
                            "origin": git_origin(&git_dir),
                        }));
                    }
                }
                continue; // Python removes ".git" from dirnames
            }
            if matches!(name.as_str(), "node_modules" | ".venv" | "target" | "dist") {
                continue;
            }
            // never walk into (or report) a delegation worktree
            if is_under(p, wt_root) {
                continue;
            }
            subdirs.push(p.clone());
        }
    }
    if depth >= 4 {
        return;
    }
    for sd in subdirs {
        walk_git(&sd, depth + 1, wt_root, repos, seen);
    }
}

// --------------------------------------------------------------------------
// vault
// --------------------------------------------------------------------------

fn count_md(path: &Path) -> usize {
    if !path.is_dir() {
        return 0;
    }
    let mut n = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            let ft = match e.file_type() {
                Ok(f) => f,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if e.file_type().map(|f| f.is_symlink()).unwrap_or(false) {
                    continue; // pathlib rglob does not descend symlinked dirs
                }
                stack.push(p);
            } else if p.extension().map(|x| x == "md").unwrap_or(false) {
                n += 1;
            }
        }
    }
    n
}

pub struct VaultImport {
    pub projects: Vec<(String, String)>, // (name, path), sorted by name
    pub areas: usize,
    pub inbox: usize,
    pub total: usize,
}

pub fn import_vault() -> VaultImport {
    let projects_root = cfg::vault_projects_dir();
    let mut projects = Vec::new();
    if projects_root.is_dir() {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&projects_root)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        for d in dirs {
            let name = d
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            projects.push((name, d.to_string_lossy().into_owned()));
        }
    }
    let vault = cfg::vault_dir();
    VaultImport {
        projects,
        areas: count_md(&cfg::vault_areas_dir()),
        inbox: count_md(&cfg::vault_inbox_dir()),
        total: count_md(&vault),
    }
}

// --------------------------------------------------------------------------
// zen spaces
// --------------------------------------------------------------------------

pub fn empty_zen() -> Value {
    json!({"ok": false, "spaces": []})
}

pub async fn import_zen() -> Value {
    let script = cfg::zen_script();
    if !script.is_file() {
        return empty_zen();
    }
    let argv = vec![
        cfg::BUN_BIN.clone(),
        script.to_string_lossy().into_owned(),
        "--json".into(),
    ];
    let Some(raw) = run_out(&argv, Duration::from_secs(45)).await else {
        return empty_zen();
    };
    let Some(start) = raw.find('{') else {
        return empty_zen();
    };
    let Ok(data) = serde_json::from_str::<Value>(&raw[start..]) else {
        return empty_zen();
    };
    let t0 = now();
    let mut spaces = Vec::new();
    if let Some(list) = data.get("spaces").and_then(|s| s.as_array()) {
        for s in list {
            let idle = s.get("idleDays").and_then(|i| i.as_f64());
            let last = idle.map(|idle| t0 - idle * 86_400.0);
            spaces.push(json!({
                "name": s.get("name").cloned().unwrap_or_else(|| json!("")),
                "tabs": s.get("tabs").cloned().unwrap_or_else(|| json!(0)),
                "lastAccessed": last,
                "uuid": s.get("uuid").cloned().unwrap_or(Value::Null),
            }));
        }
    }
    json!({"ok": true, "spaces": spaces})
}

pub fn zen_space_for(name: &str, spaces: &[Value]) -> Option<String> {
    let n = norm(name);
    if n.is_empty() {
        return None;
    }
    for s in spaces {
        let sn = s.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if norm(sn) == n {
            return Some(sn.to_string());
        }
    }
    if n.chars().count() >= 4 {
        for s in spaces {
            let sn = s.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if norm(sn).contains(&n) {
                return Some(sn.to_string());
            }
        }
    }
    None
}

// --------------------------------------------------------------------------
// memory (mnemopi, read-only)
// --------------------------------------------------------------------------

pub fn read_memory_counts() -> Value {
    let conn = Connection::open_with_flags(cfg::memory_db(), OpenFlags::SQLITE_OPEN_READ_ONLY);
    let Ok(conn) = conn else {
        return json!({"ok": false, "error": "open failed", "facts": 0, "memory_embeddings": 0, "graph_edges": 0});
    };
    let q = |table: &str| -> i64 {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or(0)
    };
    json!({
        "ok": true,
        "facts": q("facts"),
        "memory_embeddings": q("memory_embeddings"),
        "graph_edges": q("graph_edges"),
    })
}

// --------------------------------------------------------------------------
// transcript parsing (shared with the HTTP layer)
// --------------------------------------------------------------------------

/// Tail of an omp .jsonl -> user/assistant/tool events.
pub fn parse_session_events(path: &str, max_records: usize) -> Vec<Value> {
    use std::io::{Read, Seek, SeekFrom};
    let p = Path::new(path);
    let Ok(meta) = std::fs::metadata(p) else {
        return vec![];
    };
    let size = meta.len();
    let mut f = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    if size > MAX_SESSION_TAIL {
        if f.seek(SeekFrom::End(-(MAX_SESSION_TAIL as i64))).is_err() {
            return vec![];
        }
    }
    // Drop the (possibly partial) first line.
    let mut byte = [0u8; 1];
    loop {
        match f.read(&mut byte) {
            Ok(0) => return vec![],
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {}
            Err(_) => return vec![],
        }
    }
    let mut rest = Vec::new();
    if f.read_to_end(&mut rest).is_err() {
        return vec![];
    }
    let text = String::from_utf8_lossy(&rest);
    let lines: Vec<&str> = text.split('\n').map(|l| l.trim()).collect();
    let lines = &lines[lines.len().saturating_sub(max_records)..];
    let fallback = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut events = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Ok(d) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        events.extend(parse_transcript_record(&d, fallback));
    }
    events
}

/// One jsonl record -> transcript events [{role,text,ts}].
pub fn parse_transcript_record(d: &Value, fallback_ts: f64) -> Vec<Value> {
    let mut out = Vec::new();
    let t = d.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let ts = iso_to_unix(d.get("timestamp").unwrap_or(&Value::Null)).unwrap_or(fallback_ts);
    if t == "message" {
        let msg = d.get("message").cloned().unwrap_or(Value::Null);
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role != "user" && role != "assistant" {
            return out;
        }
        if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
            for c in content {
                if c.get("type").and_then(|v| v.as_str()) == Some("text") {
                    let raw = c.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
                    if !raw.is_empty() {
                        out.push(json!({
                            "role": role,
                            "text": cap_chars(raw, TEXT_CAP),
                            "ts": ts,
                        }));
                    }
                }
            }
        }
    } else if t == "custom"
        && d.get("customType").and_then(|v| v.as_str()) == Some("tool_execution_start")
    {
        let data = d.get("data").cloned().unwrap_or(Value::Null);
        let name = data
            .get("toolName")
            .and_then(|v| v.as_str())
            .unwrap_or("tool");
        let text = match data.get("intent") {
            Some(Value::String(intent)) if !intent.is_empty() => {
                format!("{name} \u{2014} {intent}")
            }
            _ => name.to_string(),
        };
        out.push(json!({
            "role": "tool",
            "text": cap_chars(&text, 2000),
            "ts": ts,
        }));
    }
    out
}

// --------------------------------------------------------------------------
// search_docs population
// --------------------------------------------------------------------------

/// INSERT OR REPLACE would delete+reinsert the parent row and cascade away its
/// vector; upsert keeps the row (and its embedding) in place.
const SEARCH_DOC_UPSERT: &str =
    "INSERT INTO search_docs(id,kind,title,body,source_ref) VALUES(?,?,?,?,?) \
     ON CONFLICT(id) DO UPDATE SET \
         kind=excluded.kind, title=excluded.title, body=excluded.body, source_ref=excluded.source_ref";

/// Delete only the docs of `kind` that truly vanished (kept ids in `fresh`),
/// so unchanged docs keep their embeddings and FTS rowids.
fn delete_vanished(
    conn: &Connection,
    kind: &str,
    fresh: &HashSet<String>,
) -> rusqlite::Result<usize> {
    let existing = embed::existing_ids(conn, kind)?;
    let mut gone = 0;
    for id in existing.difference(fresh) {
        gone += conn.execute("DELETE FROM search_docs WHERE id = ?1", params![id])?;
    }
    Ok(gone)
}

/// Refresh only board_item docs (used by board mutations). Same connection.
/// Closed («done») items leave the index: a finished deal is not a search hit.
/// Body: title first, then project, the description, the checklist steps and
/// the attachment paths («Этап А»/«B») — tasks become findable by their
/// brief, their steps and their attached files.
pub fn sync_board_docs(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt =
        conn.prepare("SELECT id,title,project,description FROM items WHERE done = 0")?;
    let rows: Vec<(i64, String, Option<String>, Option<String>)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);
    let fresh: HashSet<String> = rows
        .iter()
        .map(|(id, _, _, _)| format!("board:{id}"))
        .collect();
    delete_vanished(conn, "board_item", &fresh)?;
    let mut files = conn.prepare(
        "SELECT path FROM item_files WHERE item_id = ?1 ORDER BY sort, rowid",
    )?;
    let mut steps = conn.prepare(
        "SELECT text FROM item_steps WHERE item_id = ?1 ORDER BY pos, id",
    )?;
    let mut ins = conn.prepare(SEARCH_DOC_UPSERT)?;
    for (id, title, project, description) in rows {
        let mut body = match &project {
            Some(p) => format!("{title}\nproject: {p}"),
            None => title.clone(),
        };
        if let Some(d) = description.as_deref().filter(|d| !d.is_empty()) {
            body.push('\n');
            body.push_str(d);
        }
        let step_texts: Vec<String> = steps
            .query_map([id], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        if !step_texts.is_empty() {
            body.push_str("\nsteps:\n");
            body.push_str(&step_texts.join("\n"));
        }
        let paths: Vec<String> = files
            .query_map([id], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        if !paths.is_empty() {
            body.push_str("\nfiles:\n");
            body.push_str(&paths.join("\n"));
        }
        ins.execute(params![
            format!("board:{id}"),
            "board_item",
            title,
            body,
            format!("board:{id}")
        ])?;
    }
    Ok(())
}

fn walk_vault_notes() -> Vec<(String, String, String, String, String)> {
    let mut rows = Vec::new();
    let mut stack = vec![cfg::vault_dir()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let name = p
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            if p.is_dir() {
                if !name.starts_with('.') {
                    stack.push(p);
                }
                continue;
            }
            if !name.ends_with(".md") {
                continue;
            }
            let Ok(meta) = p.metadata() else { continue };
            if meta.len() > 2 * 1024 * 1024 {
                continue;
            }
            let body = match std::fs::read(&p) {
                Ok(bytes) => cap_chars(&String::from_utf8_lossy(&bytes), MAX_NOTE_BYTES),
                Err(_) => continue,
            };
            let sp = p.to_string_lossy().into_owned();
            let stem = p
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            rows.push((format!("vault:{sp}"), "vault_note".into(), stem, body, sp));
        }
    }
    rows
}

/// Rebuild all search_docs from sources (Python `sync_search_docs`).
pub fn sync_search_docs(
    conn: &Connection,
    sessions: &[(String, SlugInfo)],
    memory: &Value,
) -> rusqlite::Result<()> {
    let was_empty = match conn.query_row("SELECT 1 FROM search_docs LIMIT 1", [], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(_) => false,
        Err(rusqlite::Error::QueryReturnedNoRows) => true,
        Err(e) => return Err(e),
    };

    let tx = conn.unchecked_transaction()?;

    // vault notes
    {
        let fresh_rows = walk_vault_notes();
        let fresh: HashSet<String> = fresh_rows.iter().map(|r| r.0.clone()).collect();
        delete_vanished(&tx, "vault_note", &fresh)?;
        let mut ins = tx.prepare(SEARCH_DOC_UPSERT)?;
        for row in fresh_rows {
            ins.execute(params![row.0, row.1, row.2, row.3, row.4])?;
        }
    }

    // omp session transcripts: latest session per slug dir only
    {
        let mut fresh_rows: Vec<(String, String, String, String, String)> = Vec::new();
        for (slug, info) in sessions {
            let Some(lf) = &info.latest_file else {
                continue;
            };
            let events = parse_session_events(lf, 400);
            if events.is_empty() {
                continue;
            }
            let msgs: Vec<&Value> = events
                .iter()
                .filter(|e| {
                    matches!(
                        e.get("role").and_then(|r| r.as_str()),
                        Some("user") | Some("assistant")
                    )
                })
                .collect();
            let msgs = &msgs[msgs.len().saturating_sub(20)..];
            let body_raw: String = msgs
                .iter()
                .map(|e| {
                    format!(
                        "{}: {}",
                        e.get("role").and_then(|r| r.as_str()).unwrap_or(""),
                        e.get("text").and_then(|t| t.as_str()).unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let body = cap_chars(&body_raw, MAX_NOTE_BYTES);
            let project = info.project.clone().unwrap_or_else(|| slug.clone());
            let parent = std::path::Path::new(lf)
                .parent()
                .and_then(|p| p.file_name())
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            fresh_rows.push((
                format!("session:{lf}"),
                "omp_session".into(),
                format!("{project} \u{2014} {parent}"),
                body,
                lf.clone(),
            ));
        }
        let fresh: HashSet<String> = fresh_rows.iter().map(|r| r.0.clone()).collect();
        delete_vanished(&tx, "omp_session", &fresh)?;
        let mut ins = tx.prepare(SEARCH_DOC_UPSERT)?;
        for row in fresh_rows {
            ins.execute(params![row.0, row.1, row.2, row.3, row.4])?;
        }
    }

    // board items
    sync_board_docs(&tx)?;

    // memory facts (mnemopi, read-only)
    if memory.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
        if let Ok(mconn) =
            Connection::open_with_flags(cfg::memory_db(), OpenFlags::SQLITE_OPEN_READ_ONLY)
        {
            let mut ins = tx.prepare(SEARCH_DOC_UPSERT)?;
            let mut fresh: HashSet<String> = HashSet::new();
            if let Ok(mut cur) =
                mconn.prepare("SELECT fact_id, subject, predicate, object FROM facts")
            {
                if let Ok(mut rows) = cur.query([]) {
                    loop {
                        let mut batch: Vec<(String, String, String, String)> =
                            Vec::with_capacity(500);
                        for _ in 0..500 {
                            match rows.next() {
                                Ok(Some(r)) => {
                                    let fid: String = r.get(0).unwrap_or_default();
                                    let s: String = r.get(1).unwrap_or_default();
                                    let p: String = r.get(2).unwrap_or_default();
                                    let o: String = r.get(3).unwrap_or_default();
                                    batch.push((fid, s, p, o));
                                }
                                _ => break,
                            }
                        }
                        if batch.is_empty() {
                            break;
                        }
                        for (fid, s, p, o) in batch {
                            let id = format!("fact:{fid}");
                            fresh.insert(id.clone());
                            ins.execute(params![
                                id,
                                "memory_fact",
                                s,
                                format!("{s} {p} {o}"),
                                format!("mnemopi:facts/{fid}")
                            ])?;
                        }
                    }
                }
            }
            drop(ins);
            delete_vanished(&tx, "memory_fact", &fresh)?;
        }
    }

    if was_empty {
        tx.execute("INSERT INTO fts_docs(fts_docs) VALUES('rebuild')", [])?;
    }
    tx.commit()?;
    Ok(())
}

// --------------------------------------------------------------------------
// projects upsert + full sync
// --------------------------------------------------------------------------

#[derive(Clone)]
struct Proj {
    name: String,
    path: Option<String>,
    vault_path: Option<String>,
    zen_space: Option<String>,
    memory_bank: Option<String>,
    herdr_workspace_id: Option<String>,
    agent_status: Option<String>,
    agents_count: i64,
    sessions_count: i64,
    last_activity: f64,
}

impl Proj {
    fn new(name: &str) -> Proj {
        Proj {
            name: name.to_string(),
            path: None,
            vault_path: None,
            zen_space: None,
            memory_bank: if cfg::memory_db().exists() {
                Some("mnemopi".into())
            } else {
                None
            },
            herdr_workspace_id: None,
            agent_status: None,
            agents_count: 0,
            sessions_count: 0,
            last_activity: 0.0,
        }
    }
}

fn max_md_mtime(path: &Path) -> f64 {
    let mut best = 0.0f64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x == "md").unwrap_or(false) {
                if let Ok(md) = p.metadata() {
                    if let Ok(t) = md.modified() {
                        if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                            best = best.max(d.as_secs_f64());
                        }
                    }
                }
            }
        }
    }
    best
}

fn get_proj<'a>(projects: &'a mut Vec<(String, Proj)>, name: &str) -> &'a mut Proj {
    let key = norm(name);
    if !projects.iter().any(|(k, _)| *k == key) {
        projects.push((key.clone(), Proj::new(name)));
    }
    &mut projects.iter_mut().find(|(k, _)| *k == key).unwrap().1
}

/// Full recompute + upsert. Async parts (subprocesses) awaited here; the
/// blocking filesystem/DB tail runs on the blocking pool.
pub async fn sync_all(db: std::sync::Arc<Db>) -> Result<Value, rusqlite::Error> {
    let t0 = now();
    let herdr = import_herdr().await;
    let zen = import_zen().await;
    let (sessions, git, vault, memory) = tokio::task::spawn_blocking(|| {
        (
            import_sessions(),
            import_git(),
            import_vault(),
            read_memory_counts(),
        )
    })
    .await
    .expect("importers panicked");

    let db2 = std::sync::Arc::clone(&db);
    let n_herdr_ws = herdr
        .get("workspaces")
        .and_then(|w| w.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let n_zen = zen
        .get("spaces")
        .and_then(|w| w.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let herdr_ok = herdr.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
    let zen_ok = zen.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
    let total_sessions = sessions.1;
    let n_slugs = sessions.0.len();
    let n_git = git.len();
    let n_vault_projects = vault.projects.len();
    let n_vault_notes = vault.total;
    let mem_ok = memory.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
    let mem_facts = memory.get("facts").and_then(|f| f.as_i64()).unwrap_or(0);
    let mem_emb = memory
        .get("memory_embeddings")
        .and_then(|f| f.as_i64())
        .unwrap_or(0);
    let mem_edges = memory
        .get("graph_edges")
        .and_then(|f| f.as_i64())
        .unwrap_or(0);

    let summary = tokio::task::spawn_blocking(move || {
        sync_all_blocking(
            db2,
            herdr,
            sessions,
            git,
            vault,
            zen,
            memory,
            n_herdr_ws,
            total_sessions,
            n_slugs,
            n_git,
            n_vault_projects,
            n_vault_notes,
            herdr_ok,
            n_zen,
            zen_ok,
            mem_ok,
            mem_facts,
            mem_emb,
            mem_edges,
        )
    })
    .await
    .expect("sync panicked");

    let duration = ((now() - t0) * 1000.0).round() / 1000.0;
    let _ = summary?;
    Ok(json!({
        "herdr": {"ok": herdr_ok, "workspaces": n_herdr_ws},
        "omp": {"ok": true, "slugs": n_slugs, "sessions": total_sessions},
        "git": {"ok": true, "repos": n_git},
        "vault": {"ok": true, "projects": n_vault_projects, "notes": n_vault_notes},
        "zen": {"ok": zen_ok, "spaces": n_zen},
        "memory": {"ok": mem_ok, "facts": mem_facts},
        "duration": duration,
    }))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn sync_all_blocking(
    db: std::sync::Arc<Db>,
    herdr: Value,
    sessions: (Vec<(String, SlugInfo)>, i64),
    git: Vec<Value>,
    vault: VaultImport,
    zen: Value,
    memory: Value,
    n_herdr_ws: usize,
    total_sessions: i64,
    n_slugs: usize,
    n_git: usize,
    _n_vault_projects: usize,
    n_vault_notes: usize,
    herdr_ok: bool,
    n_zen: usize,
    zen_ok: bool,
    mem_ok: bool,
    mem_facts: i64,
    mem_emb: i64,
    mem_edges: i64,
) -> rusqlite::Result<()> {
    let hb = herdr_by_project(&herdr);
    let mut projects: Vec<(String, Proj)> = Vec::new();

    for (key, e) in &hb {
        let p = get_proj(&mut projects, key);
        if e.workspace_id.is_some() {
            p.herdr_workspace_id = e.workspace_id.clone();
        }
        if e.agent_status.is_some() {
            p.agent_status = e.agent_status.clone();
        }
        p.agents_count = p.agents_count.max(e.agents_count);
    }

    for r in &git {
        let name = r.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let path = r.get("path").and_then(|n| n.as_str()).unwrap_or("");
        let p = get_proj(&mut projects, name);
        let replace = match &p.path {
            None => true,
            Some(cur) => !Path::new(cur).is_dir(),
        };
        if replace {
            p.path = Some(path.to_string());
        }
    }

    for (_slug, info) in &sessions.0 {
        let Some(name) = &info.project else { continue };
        let p = get_proj(&mut projects, name);
        p.sessions_count += info.count;
        p.last_activity = p.last_activity.max(info.latest_mtime);
        if p.path.is_none() {
            if let Some(dp) = &info.dir_path {
                p.path = Some(dp.clone());
            }
        }
    }

    for (name, path) in &vault.projects {
        let p = get_proj(&mut projects, name);
        p.vault_path = Some(path.clone());
    }

    let zen_spaces: Vec<Value> = zen
        .get("spaces")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    for (key, p) in projects.iter_mut() {
        if let Some(vp) = &p.vault_path {
            p.last_activity = p.last_activity.max(max_md_mtime(Path::new(vp)));
        }
        if p.path.is_some() && p.herdr_workspace_id.is_none() {
            if let Some((_, e)) = hb.iter().find(|(k, _)| *k == *key) {
                p.herdr_workspace_id = e.workspace_id.clone();
            }
        }
        p.zen_space = zen_space_for(&p.name, &zen_spaces);
    }

    // agent cwd pins the real path (most precise source)
    for (key, e) in &hb {
        for a in &e.agents {
            let Some(cwd) = a.get("cwd").and_then(|c| c.as_str()) else {
                continue;
            };
            if !Path::new(cwd).is_dir() {
                continue;
            }
            // a delegation worktree is transient — never pin a project's path to it
            if is_delegation_worktree(Path::new(cwd)) {
                continue;
            }
            let p = get_proj(&mut projects, key);
            let replace = match &p.path {
                None => true,
                Some(cur) => cwd.len() > cur.len(),
            };
            if replace {
                p.path = Some(cwd.to_string());
            }
        }
    }

    // ---- write ----
    let conn = db.lock();
    let tx = conn.unchecked_transaction()?;
    for (_, p) in &projects {
        tx.execute(
            "INSERT INTO projects(name,path,vault_path,zen_space,memory_bank, \
                 herdr_workspace_id,agent_status,agents_count,sessions_count,last_activity) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) \
             ON CONFLICT(name) DO UPDATE SET \
                 path=COALESCE(excluded.path, projects.path), \
                 vault_path=COALESCE(excluded.vault_path, projects.vault_path), \
                 zen_space=COALESCE(excluded.zen_space, projects.zen_space), \
                 memory_bank=COALESCE(excluded.memory_bank, projects.memory_bank), \
                 herdr_workspace_id=COALESCE(excluded.herdr_workspace_id, projects.herdr_workspace_id), \
                 agent_status=COALESCE(excluded.agent_status, projects.agent_status), \
                 agents_count=excluded.agents_count, \
                 sessions_count=excluded.sessions_count, \
                 last_activity=excluded.last_activity",
            params![
                p.name,
                p.path,
                p.vault_path,
                p.zen_space,
                p.memory_bank,
                p.herdr_workspace_id,
                p.agent_status,
                p.agents_count,
                p.sessions_count,
                p.last_activity,
            ],
        )?;
    }

    let links_empty = match tx.query_row("SELECT 1 FROM links LIMIT 1", [], |r| r.get::<_, i64>(0))
    {
        Ok(_) => false,
        Err(rusqlite::Error::QueryReturnedNoRows) => true,
        Err(e) => return Err(e),
    };
    if links_empty {
        for (fp, tp, kind) in SEED_LINKS {
            tx.execute(
                "INSERT OR IGNORE INTO links(from_project,to_project,kind,note) VALUES(?,?,?,?)",
                params![fp, tp, kind, Option::<String>::None],
            )?;
        }
    }

    let ts = now();
    let vault_meta = format!("{{\"areas\": {}, \"inbox\": {}}}", vault.areas, vault.inbox);
    let mem_meta = format!(
        "{{\"memory_embeddings\": {}, \"graph_edges\": {}}}",
        mem_emb, mem_edges
    );
    let sources_rows: [(&str, i64, bool, Option<String>); 6] = [
        ("herdr", n_herdr_ws as i64, herdr_ok, None),
        ("omp", total_sessions, n_slugs > 0, None),
        ("git", n_git as i64, true, None),
        ("vault", n_vault_notes as i64, true, Some(vault_meta)),
        ("zen", n_zen as i64, zen_ok, None),
        ("memory", mem_facts, mem_ok, Some(mem_meta)),
    ];
    for (name, count, ok, meta) in sources_rows {
        tx.execute(
            "INSERT INTO sources(name,count,ok,last_sync,meta) VALUES(?,?,?,?,?) \
             ON CONFLICT(name) DO UPDATE SET count=excluded.count, ok=excluded.ok, \
                 last_sync=excluded.last_sync, meta=excluded.meta",
            params![name, count, ok as i64, ts, meta],
        )?;
    }

    tx.execute("DELETE FROM sessions", [])?;
    for (slug, info) in &sessions.0 {
        let Some(project) = &info.project else {
            continue;
        };
        tx.execute(
            "INSERT INTO sessions(slug,project,dir_path,count,latest_file,latest_mtime) VALUES(?,?,?,?,?,?)",
            params![slug, project, info.dir_path, info.count, info.latest_file, info.latest_mtime],
        )?;
    }
    tx.commit()?;

    sync_search_docs(&conn, &sessions.0, &memory)?;

    // The FTS write above needs the lock; the vector phase below must NOT hold
    // it — `reindex` takes the shared connection per batch, so a cold model
    // load (~1 min) or a long embed run cannot pin the single DB door (every
    // endpoint would queue, then fail with «database is locked» after the 10 s
    // busy timeout). Release before reset_backend/reindex.
    drop(conn);

    // Vector index: additive enhancement of search_docs — an unavailable
    // embedding backend or embed failure degrades to FTS-only, never fails sync.
    embed::reset_backend();
    match embed::reindex(db.as_ref()) {
        Ok(s) => log::info!(
            "vector index: {} embedded, {} unchanged, {} deleted ({} docs) in {:.1}s",
            s.embedded,
            s.unchanged,
            s.deleted,
            s.total,
            s.secs
        ),
        Err(err) => log::warn!("vector index unavailable, search stays FTS-only: {err}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Throwaway root under $TMPDIR, removed by the caller.
    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atlas-imp-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The rule: the delegation root (config `paths.worktrees`, default
    /// `$HOME/.atlas-wt`) and everything below it, matched component-wise —
    /// NOT any dir that merely bears that name.
    #[test]
    fn worktree_rule_is_anchored_at_the_delegation_root() {
        let root = Path::new("/home/u/.atlas-wt");
        assert!(is_under(Path::new("/home/u/.atlas-wt"), root));
        assert!(is_under(Path::new("/home/u/.atlas-wt/proj-1-abcd"), root));
        assert!(is_under(Path::new("/home/u/.atlas-wt/proj-1-abcd/"), root));
        assert!(is_under(Path::new("/home/u/.atlas-wt/a/b/c"), root));
        assert!(!is_under(Path::new("/home/u/.atlas-wtx/proj-1-abcd"), root));
        assert!(!is_under(Path::new("/home/v/.atlas-wt/proj-1-abcd"), root));
        assert!(!is_under(Path::new("/home/u/Projects/x"), root));
        // ONLY the root matters: `.atlas-wt` nested inside a project is an
        // ordinary directory, not a delegation worktree
        assert!(!is_under(Path::new("/home/u/Projects/x/.atlas-wt/y-1-abcd"), root));
        // the public helper applies the same rule to the real worktree root
        let root = delegation_root();
        assert!(is_delegation_worktree(&root.join("proj-1-abcd")));
        assert!(is_delegation_worktree(&root));
        assert!(!is_delegation_worktree(
            &cfg::home().join("Projects").join("x").join(".atlas-wt").join("y-1-abcd")
        ));
        assert!(!is_delegation_worktree(&cfg::home().join("Projects").join("x")));
    }

    /// `<project>-<item id>-<4 hex>` -> `<project>`, nothing else.
    #[test]
    fn delegation_parent_needs_the_id_hash_shape() {
        assert_eq!(
            delegation_parent("zz-atlas-deleg-19-377e").as_deref(),
            Some("zz-atlas-deleg")
        );
        assert_eq!(delegation_parent("p-1-abcd").as_deref(), Some("p"));
        assert_eq!(delegation_parent("a-b-7-00ff").as_deref(), Some("a-b"));
        assert_eq!(delegation_parent("lamplife"), None);
        assert_eq!(delegation_parent("proj-1"), None); // no hash
        assert_eq!(delegation_parent("proj-x-ab-abcd"), None); // id not numeric
        assert_eq!(delegation_parent("proj-12-zzzz"), None); // hash not hex
        assert_eq!(delegation_parent("proj-12-abc"), None); // hash too short
        assert_eq!(delegation_parent("-1-abcd"), None); // no project part
    }

    /// Throwaway worktree layout: `<root>/<name>`, removed by the caller.
    fn wt_dir(home: &Path, root_name: &str, name: &str) -> PathBuf {
        let d = home.join(root_name).join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The marker decides: a valid `.atlas-deleg.json` recognizes the dir as a
    /// delegation worktree of exactly the item/project it names.
    #[test]
    fn marker_makes_the_dir_a_delegation_worktree() {
        let home = tmp("marker");
        let wt = wt_dir(&home, ".atlas-wt", "lamplife-19-377e");
        std::fs::write(
            wt.join(delegate::DELEG_MARKER),
            r#"{"item":19,"project":"lamplife","branch":"task/19-377e"}"#,
        )
        .unwrap();
        assert!(is_delegation_worktree_in(&wt, &home.join(".atlas-wt")));
        assert_eq!(
            delegation_marker(&wt),
            Some(DelegMarker {
                item: 19,
                project: "lamplife".into()
            })
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// legacy: ворктри до маркера — a template-named dir under the root
    /// without a marker is still recognized, as before the marker existed.
    #[test]
    fn template_name_without_marker_stays_a_worktree() {
        let home = tmp("legacy");
        let wt = wt_dir(&home, ".atlas-wt", "lamplife-19-377e");
        assert!(is_delegation_worktree_in(&wt, &home.join(".atlas-wt")));
        assert_eq!(delegation_marker(&wt), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A corrupt marker reads as absent: the legacy name rule decides, exactly
    /// like today's behavior for unparsable data.
    #[test]
    fn corrupt_marker_falls_back_to_the_name_template() {
        let home = tmp("corrupt");
        let wt = wt_dir(&home, ".atlas-wt", "lamplife-19-377e");
        std::fs::write(wt.join(delegate::DELEG_MARKER), "{not json").unwrap();
        assert!(is_delegation_worktree_in(&wt, &home.join(".atlas-wt")));
        assert_eq!(delegation_marker(&wt), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The phantom case: a template-shaped dir OUTSIDE the delegation root is
    /// an ordinary directory — no marker, no recognition, no fake project.
    #[test]
    fn template_name_outside_the_root_is_no_worktree() {
        let home = tmp("phantom");
        let proj = wt_dir(&home, "Projects", "lamplife-19-377e");
        assert!(!is_delegation_worktree_in(&proj, &home.join(".atlas-wt")));
        assert_eq!(delegation_marker(&proj), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A worktree dir DOES exist on disk and the raw resolver reaches it (the
    /// leak), but project discovery skips it; normal slugs are unaffected.
    #[test]
    fn slug_resolution_excludes_delegation_worktrees() {
        let home = tmp("resolve");
        let projects = home.join("Projects");
        let wt_root = home.join(".atlas-wt");
        std::fs::create_dir_all(wt_root.join("demo-1-abcd")).unwrap();
        std::fs::create_dir_all(projects.join("demo")).unwrap();
        let home_s = home.to_string_lossy().into_owned();
        let wt = wt_root.join("demo-1-abcd").to_string_lossy().into_owned();
        let demo = projects.join("demo").to_string_lossy().into_owned();

        assert_eq!(
            resolve_slug_in("-.atlas-wt-demo-1-abcd", &home_s, &projects, &wt_root, true).as_deref(),
            Some(wt.as_str())
        );
        assert_eq!(
            resolve_slug_in("-.atlas-wt-demo-1-abcd", &home_s, &projects, &wt_root, false),
            None
        );
        assert_eq!(resolve_slug_in("-.atlas-wt", &home_s, &projects, &wt_root, false), None);
        assert_eq!(
            resolve_slug_in("-Projects-demo", &home_s, &projects, &wt_root, false).as_deref(),
            Some(demo.as_str())
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Fixture slug list: the worktree session produces NO project of its own
    /// (so `sync_all_blocking` writes no such row), lands on its parent instead,
    /// and leaves ordinary projects alone.
    #[test]
    fn worktree_session_never_becomes_a_project() {
        let home = tmp("wt");
        let projects = home.join("Projects");
        let wt_root = home.join(".atlas-wt");
        std::fs::create_dir_all(wt_root.join("zz-atlas-deleg-19-377e")).unwrap();
        std::fs::create_dir_all(wt_root.join("diskproj-2-abcd")).unwrap();
        std::fs::create_dir_all(wt_root.join("ghost-3-abcd")).unwrap();
        std::fs::create_dir_all(projects.join("zz-atlas-deleg")).unwrap();
        std::fs::create_dir_all(projects.join("diskproj")).unwrap();
        std::fs::create_dir_all(projects.join("plain")).unwrap();
        std::fs::create_dir_all(projects.join("x").join(".atlas-wt").join("y-1-abcd")).unwrap();
        let home_s = home.to_string_lossy().into_owned();
        let f = |s: &str| Some(format!("/sessions/{s}.jsonl"));
        let raw = vec![
            ("-.atlas-wt-zz-atlas-deleg-19-377e".into(), 3i64, f("a"), 5.0f64),
            ("-.atlas-wt-diskproj-2-abcd".into(), 1, f("b"), 4.0),
            ("-.atlas-wt-ghost-3-abcd".into(), 1, f("c"), 3.0),
            ("-Projects-zz-atlas-deleg".into(), 2, f("d"), 2.0),
            ("-Projects-plain".into(), 7, f("e"), 1.0),
            ("-Projects-x-.atlas-wt-y-1-abcd".into(), 1, f("f"), 0.5),
        ];
        let out = sessions_from(&raw, &home_s, &projects, &wt_root);
        let by: HashMap<&str, &SlugInfo> = out.iter().map(|(s, i)| (s.as_str(), i)).collect();
        let p = |r: &str| projects.join(r).to_string_lossy().into_owned();

        // attributed to the parent project, named by another slug AND by disk
        assert_eq!(by["-.atlas-wt-zz-atlas-deleg-19-377e"].project.as_deref(), Some("zz-atlas-deleg"));
        assert_eq!(by["-.atlas-wt-zz-atlas-deleg-19-377e"].dir_path.as_deref(), Some(p("zz-atlas-deleg").as_str()));
        assert_eq!(by["-.atlas-wt-diskproj-2-abcd"].project.as_deref(), Some("diskproj"));
        assert_eq!(by["-.atlas-wt-diskproj-2-abcd"].dir_path.as_deref(), Some(p("diskproj").as_str()));
        // unknown parent: dropped, never minted
        assert_eq!(by["-.atlas-wt-ghost-3-abcd"].project, None);
        assert_eq!(by["-.atlas-wt-ghost-3-abcd"].dir_path, None);
        // stats survive attribution
        assert_eq!(by["-.atlas-wt-zz-atlas-deleg-19-377e"].count, 3);
        assert_eq!(by["-.atlas-wt-zz-atlas-deleg-19-377e"].latest_mtime, 5.0);
        // ordinary projects unchanged
        assert_eq!(by["-Projects-plain"].project.as_deref(), Some("plain"));
        assert_eq!(by["-Projects-plain"].dir_path.as_deref(), Some(p("plain").as_str()));
        // a `.atlas-wt` dir inside a project stays the ordinary dir it is
        assert_eq!(by["-Projects-x-.atlas-wt-y-1-abcd"].project.as_deref(), Some("y-1-abcd"));
        assert_eq!(
            by["-Projects-x-.atlas-wt-y-1-abcd"].dir_path.as_deref(),
            Some(projects.join("x").join(".atlas-wt").join("y-1-abcd").to_string_lossy().as_ref())
        );
        // no row can name the worktree: every project is a resolved dir name
        for (slug, info) in &out {
            assert_ne!(info.project.as_deref(), Some("zz-atlas-deleg-19-377e"), "{slug}");
            assert_ne!(info.project.as_deref(), Some("diskproj-2-abcd"), "{slug}");
            assert_ne!(info.project.as_deref(), Some("ghost-3-abcd"), "{slug}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The git walk reports repos, never a delegation worktree.
    #[test]
    fn git_walk_skips_delegation_worktrees() {
        let root = tmp("git");
        let wt_root = root.join(".atlas-wt");
        for d in [
            root.join("real").join(".git"),
            wt_root.join("clone-1-abcd").join(".git"),
            root.join("real").join("nested").join(".git"),
        ] {
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("config"), "[remote \"origin\"]\n\turl = x\n").unwrap();
        }
        let mut repos = Vec::new();
        let mut seen = HashSet::new();
        walk_git(&root, 0, &wt_root, &mut repos, &mut seen);
        let names: Vec<&str> = repos.iter().filter_map(|r| r["name"].as_str()).collect();
        assert_eq!(names, vec!["real", "nested"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
