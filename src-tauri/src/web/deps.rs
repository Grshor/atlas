//! Dependency graph: `GET /api/deps` with per-edge provenance and cycle
//! detection.

use super::{db_lock, join_result, ok_json, ApiError};
use crate::cfg;
use crate::deporigin;
use axum::response::Response;
use serde_json::json;
use std::collections::{BTreeSet, HashMap};


fn find_cycles(edges: &[(String, String)]) -> Vec<Vec<String>> {
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (a, b) in edges {
        if !adj.contains_key(a) {
            order.push(a.clone());
        }
        adj.entry(a.clone()).or_default().push(b.clone());
    }
    let mut state: HashMap<String, u8> = HashMap::new();
    let mut stack: Vec<String> = Vec::new();
    let mut cycles: BTreeSet<Vec<String>> = BTreeSet::new();
    for v in &order {
        if state.get(v).copied().unwrap_or(0) == 0 {
            dfs(v, &adj, &mut state, &mut stack, &mut cycles);
        }
    }
    cycles.into_iter().collect()
}

fn dfs(
    v: &str,
    adj: &HashMap<String, Vec<String>>,
    state: &mut HashMap<String, u8>,
    stack: &mut Vec<String>,
    cycles: &mut BTreeSet<Vec<String>>,
) {
    state.insert(v.to_string(), 1);
    stack.push(v.to_string());
    if let Some(neigh) = adj.get(v) {
        for w in neigh {
            match state.get(w).copied().unwrap_or(0) {
                0 => dfs(w, adj, state, stack, cycles),
                1 => {
                    let i = stack.iter().position(|x| x == w).expect("grey on stack");
                    let cyc: Vec<String> = stack[i..].to_vec();
                    // canonical rotation for dedupe (first minimal rotation)
                    let n = cyc.len();
                    let mut best = 0usize;
                    for k in 1..n {
                        if cyc[k..] < cyc[best..] {
                            best = k;
                        }
                    }
                    let mut rotated: Vec<String> = cyc[best..].to_vec();
                    rotated.extend_from_slice(&cyc[..best]);
                    cycles.insert(rotated);
                }
                _ => {}
            }
        }
    }
    stack.pop();
    state.insert(v.to_string(), 2);
}

pub(crate) async fn api_deps() -> Result<Response, ApiError> {
    let out = tokio::task::spawn_blocking(|| {
        let conn = db_lock();
        let mut stmt = conn
            .prepare("SELECT from_project, to_project, kind, note FROM links ORDER BY from_project")
            .expect("select links");
        let links: Vec<deporigin::LinkRow> = stmt
            .query_map([], |r| {
                Ok(deporigin::LinkRow {
                    from: r.get(0)?,
                    to: r.get(1)?,
                    kind: r.get(2)?,
                    note: r.get(3)?,
                })
            })
            .expect("query links")
            .filter_map(|r| r.ok())
            .collect();
        let mut paths: HashMap<String, String> = HashMap::new();
        let mut names: Vec<String> = Vec::new();
        {
            let mut stmt = conn
                .prepare("SELECT name, path FROM projects")
                .expect("select projects");
            let rows: Vec<(String, Option<String>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .expect("query projects")
                .filter_map(|r| r.ok())
                .collect();
            for (name, path) in rows {
                names.push(name.clone());
                if let Some(p) = path.filter(|p| !p.is_empty()) {
                    paths.insert(name, p);
                }
            }
        }
        // SEED_LINKS line numbers are computed from the actual source at
        // request time; a moved tuple falls back to a line-less ref.
        let importers_text =
            std::fs::read_to_string(cfg::atlas_dir().join("src-tauri/src/importers.rs")).ok();
        let (edges, warnings) =
            deporigin::build_edges(&links, &paths, &names, importers_text.as_deref());
        let strict = edges.iter().filter(|e| e["kind"] == "strict").count() as i64;
        let soft = edges.iter().filter(|e| e["kind"] == "soft").count() as i64;
        let pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| {
                (
                    e["from"].as_str().unwrap_or("").to_string(),
                    e["to"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        let cycles = find_cycles(&pairs);
        json!({
            "edges": edges,
            "summary": {
                "strict": strict,
                "soft": soft,
                "cycles": cycles,
                "warnings": warnings,
            },
        })
    })
    .await
    .map_err(join_result)?;
    Ok(ok_json(out))
}

