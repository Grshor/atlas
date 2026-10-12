//! Cross-source search: `GET /api/search` — FTS over board items, sessions,
//! sources with Reciprocal-Rank-Fusion merging.

use super::{db_lock, join_result, ok_json, query_param, ApiError};
use crate::embed;
use crate::util::ref_to_file;
use axum::http::Uri;
use axum::response::Response;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::time::Instant;


/// Reciprocal Rank Fusion constant (Cormack et al. 2009); equal list weights.
const RRF_K: f64 = 60.0;
/// Internal retrieval depth per leg: k * DEPTH_FACTOR (capped) — fusion needs
/// deeper candidate lists than the final result size.
const DEPTH_FACTOR: usize = 3;

fn fts_query(tokens: &[String], op: &str) -> String {
    tokens
        .iter()
        .map(|t| format!("\"{t}\"*"))
        .collect::<Vec<_>>()
        .join(&format!(" {op} "))
}

pub(crate) async fn api_search(uri: Uri) -> Result<Response, ApiError> {
    let q = query_param(&uri, "q").ok_or_else(|| ApiError::unprocessable("query q required"))?;
    let k_raw = query_param(&uri, "k");
    let k: usize = match k_raw {
        Some(s) => s
            .parse()
            .map_err(|_| ApiError::unprocessable("k must be an integer"))?,
        None => 20,
    };
    if !(1..=100).contains(&k) {
        return Err(ApiError::unprocessable("k must be between 1 and 100"));
    }

    let token_re = Regex::new(r"\w+").expect("token regex");
    let tokens: Vec<String> = token_re
        .find_iter(&q)
        .map(|m| m.as_str().to_string())
        .collect();
    if tokens.is_empty() {
        let (vec_on, model) = embed::status();
        return Ok(ok_json(json!({
            "hits": [],
            "vector_enabled": vec_on,
            "model": model,
            "took_ms": 0,
        })));
    }

    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let t0 = Instant::now();
        let conn = db_lock();
        let depth = (k * DEPTH_FACTOR).min(300);

        // --- FTS leg: same rank semantics as before, deeper LIMIT, + doc id ---
        let run = |op: &str| -> Vec<(String, String, String, Option<String>, Option<String>, f64)> {
            let mut stmt = conn
                .prepare(
                    "SELECT s.id, s.kind, \
                            snippet(fts_docs, 1, '<b>', '</b>', '\u{2026}', 14) AS snip, \
                            s.title, s.source_ref, rank \
                     FROM fts_docs JOIN search_docs s ON s.rowid = fts_docs.rowid \
                     WHERE fts_docs MATCH ?1 \
                     ORDER BY rank LIMIT ?2",
                )
                .expect("prepare search");
            stmt.query_map(
                rusqlite::params![fts_query(&tokens, op), depth as i64],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, f64>(5)?,
                    ))
                },
            )
            .expect("search")
            .filter_map(|r| r.ok())
            .collect()
        };
        let mut fts_rows = run("AND");
        if fts_rows.len() < depth && tokens.len() > 1 {
            let seen: HashSet<String> = fts_rows.iter().map(|r| r.0.clone()).collect();
            for r in run("OR") {
                if !seen.contains(&r.0) {
                    fts_rows.push(r);
                }
            }
        }

        // --- vector leg: brute-force cosine; failure degrades, never 500s ---
        let mut vector_enabled = false;
        let mut vector_error: Option<String> = None;
        let mut vec_hits: Vec<(String, f64)> = Vec::new();
        match embed::search(&conn, &q, depth) {
            Ok(h) => {
                vector_enabled = true;
                vec_hits = h;
            }
            Err(err) => {
                log::warn!("vector search unavailable, FTS-only: {err}");
                vector_error = Some(err);
            }
        }

        // --- Reciprocal Rank Fusion over the two ranked lists ---
        struct Hit {
            kind: String,
            title: Option<String>,
            snippet: String,
            source_ref: Option<String>,
            bm25: Option<f64>,
            cos: Option<f64>,
            score: f64,
        }
        let rrf = |i: usize| 1.0 / (RRF_K + i as f64 + 1.0);
        let round4 = |v: f64| (v * 10_000.0).round() / 10_000.0;

        let mut hits: HashMap<String, Hit> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for (i, (id, kind, snip, title, sref, rank)) in fts_rows.into_iter().enumerate() {
            order.push(id.clone());
            hits.insert(
                id,
                Hit {
                    kind,
                    snippet: snip,
                    title,
                    source_ref: sref,
                    bm25: Some(round4(-rank)),
                    cos: None,
                    score: rrf(i),
                },
            );
        }
        for (i, (id, cos)) in vec_hits.into_iter().enumerate() {
            match hits.get_mut(&id) {
                Some(h) => {
                    h.score += rrf(i);
                    h.cos = Some(round4(cos));
                }
                None => {
                    // Vector-only hit: no FTS snippet — build a plain teaser.
                    let Some((kind, title, body, sref)) = embed::doc_row(&conn, &id) else {
                        continue; // vanished mid-flight (board mutation); skip
                    };
                    order.push(id.clone());
                    hits.insert(
                        id,
                        Hit {
                            kind,
                            title: Some(title),
                            snippet: embed::plain_snippet(&body),
                            source_ref: Some(sref),
                            bm25: None,
                            cos: Some(round4(cos)),
                            score: rrf(i),
                        },
                    );
                }
            }
        }

        let mut fused: Vec<Hit> = order
            .into_iter()
            .filter_map(|id| hits.remove(&id))
            .collect();
        fused.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let hits_json: Vec<Value> = fused
            .into_iter()
            .take(k)
            .map(|h| {
                let matched = if h.bm25.is_some() && h.cos.is_some() {
                    "both"
                } else if h.bm25.is_some() {
                    "fts"
                } else {
                    "vector"
                };
                json!({
                    "kind": h.kind,
                    "title": h.title,
                    "snippet": h.snippet,
                    "source_ref": h.source_ref,
                    // local file behind the hit (vault notes, session jsonl);
                    // null for board/fact/project refs — /api/open uses this
                    "file": h.source_ref.as_deref().and_then(ref_to_file),
                    "score": (h.score * 1_000_000.0).round() / 1_000_000.0,
                    "match": matched,
                    "bm25": h.bm25,
                    "cos": h.cos,
                })
            })
            .collect();

        let (_, model) = embed::status();
        let mut out = Map::new();
        out.insert("hits".into(), json!(hits_json));
        out.insert("vector_enabled".into(), json!(vector_enabled));
        out.insert("model".into(), json!(model));
        out.insert("took_ms".into(), json!(t0.elapsed().as_millis() as i64));
        if let Some(err) = vector_error {
            out.insert("vector_error".into(), json!(err));
        }
        Ok(Value::Object(out))
    })
    .await
    .map_err(join_result)??;
    Ok(ok_json(out))
}

