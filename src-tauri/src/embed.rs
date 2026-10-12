//! Semantic search vectors: local fastembed(multilingual-e5-base) embeddings
//! stored in the `embeddings` table (float32 LE BLOB per search_docs row) and
//! brute-force cosine at query time (thousands of rows -> sub-ms in Rust).
//!
//! The model is downloaded once into `~/.local/share/atlas/fastembed` and loaded
//! lazily. Init is memoised: success is reused for the process lifetime, a
//! failure is returned as-is for every query (cheap, no retry storm) and only
//! `reset_backend()` (called at the start of each sync) allows a fresh attempt.
//! `warm_up()` pre-loads it on a startup thread so the first sync does not pay
//! the cold cost; reindexing takes the shared DB lock per batch, never across
//! an embed call.
//! Every error surfaces as graceful degradation — search stays FTS-only.
use crate::cfg;
use crate::db::Db;
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const MODEL: EmbeddingModel = EmbeddingModel::MultilingualE5Base;
pub const MODEL_NAME: &str = "intfloat/multilingual-e5-base";
pub const DIM: usize = 768;
/// Long bodies are truncated before embedding; the hash covers the truncated
/// text so a change outside the window does not trigger a re-embed.
pub const MAX_BODY_CHARS: usize = 2_000;
/// Docs per /api/embed-… per fastembed call.
pub const BATCH: usize = 64;

static BACKEND: Mutex<Option<Result<Arc<Mutex<TextEmbedding>>, String>>> = Mutex::new(None);

fn backend() -> Result<Arc<Mutex<TextEmbedding>>, String> {
    let mut guard = BACKEND.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(cached) = guard.as_ref() {
        return cached.clone();
    }
    let cache_dir = cfg::data_dir().join("fastembed");
    let _ = std::fs::create_dir_all(&cache_dir);
    log::info!(
        "vector: loading embedding model {MODEL_NAME} (cached in {})",
        cache_dir.display()
    );
    let t0 = Instant::now();
    let res = TextEmbedding::try_new(
        TextInitOptions::new(MODEL)
            .with_cache_dir(cache_dir)
            .with_show_download_progress(false),
    )
    .map(|m| Arc::new(Mutex::new(m)))
    .map_err(|e| format!("fastembed init: {e}"));
    match &res {
        Ok(_) => log::info!(
            "vector: model {MODEL_NAME} ready ({DIM} dims, {:.1}s)",
            t0.elapsed().as_secs_f64()
        ),
        Err(e) => log::warn!("vector: model init failed: {e}"),
    }
    *guard = Some(res.clone());
    res
}

/// Drop a memoised init failure so the next sync retries loading the model.
pub fn reset_backend() {
    let mut guard = BACKEND.lock().unwrap_or_else(|p| p.into_inner());
    if matches!(guard.as_ref(), Some(Err(_))) {
        *guard = None;
    }
}

/// Pre-load the model off the startup path: the first `backend()` call costs
/// ~1 min cold (file download once + weights + warmup) and, until now, paid it
/// inside the first sync. Meant for a background thread at server startup —
/// never fatal, and a no-op once the memoised handle is present (a memoised
/// failure stays a failure: the run just logs `backend`'s own warn; the next
/// sync's `reset_backend()` retries).
pub fn warm_up() {
    let _ = backend();
}

/// (available, model name) without triggering a load — for quick status only.
pub fn status() -> (bool, Option<String>) {
    let guard = BACKEND.lock().unwrap_or_else(|p| p.into_inner());
    match guard.as_ref() {
        Some(Ok(_)) => (true, Some(MODEL_NAME.to_string())),
        _ => (false, None),
    }
}

// ----------------------------------------------------------------- helpers ----

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Hash of everything that influences the stored vector (model included, so a
/// model switch forces a full re-index).
fn content_hash(kind: &str, title: &str, body_trunc: &str) -> String {
    let mut h = Sha256::new();
    for part in [MODEL_NAME, kind, title, body_trunc] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    format!("{:x}", h.finalize())
}

fn passage_text(title: &str, body_trunc: &str) -> String {
    // multilingual-e5 expects a "passage: " prefix on indexed documents.
    format!("passage: {title}\n{body_trunc}")
}

/// Embedding input for a doc body. memory_fact bodies start with the
/// subject/predicate framing ("fact entity …") from mnemopi's facts table —
/// identical boilerplate across ~2.8k docs that dilutes their vectors, so it is
/// stripped here (stored search_docs stay untouched; the hash covers this text).
fn embed_body<'a>(kind: &str, body: &'a str) -> &'a str {
    if kind == "memory_fact" {
        body.strip_prefix("fact entity ").unwrap_or(body)
    } else {
        body
    }
}

fn normalize(v: &mut [f32]) {
    let norm = v
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x = (*x as f64 / norm) as f32;
        }
    }
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for f in v {
        b.extend_from_slice(&f.to_le_bytes());
    }
    b
}

fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn embed_texts(
    be: &Arc<Mutex<TextEmbedding>>,
    texts: Vec<String>,
) -> Result<Vec<Vec<f32>>, String> {
    let mut m = be.lock().unwrap_or_else(|p| p.into_inner());
    m.embed(texts, Some(BATCH))
        .map_err(|e| format!("fastembed embed: {e}"))
}

/// Embed one query with the e5 "query: " prefix, L2-normalised.
pub fn embed_query(be: &Arc<Mutex<TextEmbedding>>, q: &str) -> Result<Vec<f32>, String> {
    let mut vecs = embed_texts(be, vec![format!("query: {q}")])?;
    let mut v = vecs.pop().ok_or_else(|| "empty embedding".to_string())?;
    if v.len() != DIM {
        return Err(format!("unexpected embedding dim {} != {DIM}", v.len()));
    }
    normalize(&mut v);
    Ok(v)
}

// ---------------------------------------------------------------- indexing ----

pub struct IndexStats {
    pub total: usize,
    pub embedded: usize,
    pub unchanged: usize,
    pub deleted: usize,
    pub secs: f64,
}

/// Bring `embeddings` in line with `search_docs`: embed new/changed docs in
/// batches of BATCH, drop rows for vanished docs, record model+dim in `meta`.
///
/// Takes the shared `Db` handle instead of a caller-held `&Connection`: the
/// one shared connection is locked per batch and RELEASED around every embed
/// call, so a cold model load (~1 min: init + warmup) or a long embedding run
/// no longer pins the single DB door — endpoints keep being served during a
/// sync. (FTS writes are unaffected: they stay under their own lock, in
/// `sync_all_blocking`.)
pub fn reindex(db: &Db) -> Result<IndexStats, String> {
    let t0 = Instant::now();
    // Lazy model load. No lock is held here — this function takes none until
    // the snapshot pass below — and the loaded handle is memoised for the rest
    // of the process (see `warm_up` for the startup pre-load).
    let be = backend()?;

    // One short locked pass: snapshot + purge. Deliberately no CPU work (no
    // hashing, no embedding) under the lock — classification and the batches
    // happen with the guard dropped.
    let (docs, existing, deleted) = {
        let conn = db.lock();

        let mut stmt = conn
            .prepare("SELECT id, kind, COALESCE(title,''), COALESCE(body,'') FROM search_docs")
            .map_err(e)?;
        let docs: Vec<(String, String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(e)?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);

        let mut stmt = conn
            .prepare("SELECT doc_id, hash FROM embeddings")
            .map_err(e)?;
        let existing: HashMap<String, String> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(e)?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);

        // Safety net: rows whose doc vanished outside the importers' cascades,
        // plus rows left over from a previous model.
        let mut deleted = conn
            .execute(
                "DELETE FROM embeddings WHERE doc_id NOT IN (SELECT id FROM search_docs)",
                [],
            )
            .map_err(e)? as usize;
        deleted += conn
            .execute(
                "DELETE FROM embeddings WHERE model != ?1",
                params![MODEL_NAME],
            )
            .map_err(e)? as usize;

        (docs, existing, deleted)
    };

    let mut work: Vec<(String, String, String)> = Vec::new(); // (doc_id, text, hash)
    let mut unchanged = 0usize;
    for (id, kind, title, body) in &docs {
        let body_trunc = truncate_chars(embed_body(kind, body), MAX_BODY_CHARS);
        let hash = content_hash(kind, title, &body_trunc);
        if existing.get(id) == Some(&hash) {
            unchanged += 1;
            continue;
        }
        work.push((id.clone(), passage_text(title, &body_trunc), hash));
    }

    let mut embedded = 0usize;
    for (done, chunk) in work.chunks(BATCH).enumerate() {
        let texts: Vec<String> = chunk.iter().map(|(_, t, _)| t.clone()).collect();
        // Batch of <= BATCH docs, embedded with NO lock held: this is where a
        // cold model load and the fastembed call run, so the DB stays reachable.
        let vecs = embed_texts(&be, texts)?;
        if vecs.len() != chunk.len() {
            return Err(format!(
                "embed returned {} vectors for {} docs",
                vecs.len(),
                chunk.len()
            ));
        }
        // Lock only for this batch's write; released before the next embed.
        {
            let conn = db.lock();
            let tx = conn.unchecked_transaction().map_err(e)?;
            {
                let mut ins = tx
                    .prepare(
                        "INSERT INTO embeddings(doc_id,model,dim,vec,hash) VALUES(?,?,?,?,?) \
                         ON CONFLICT(doc_id) DO UPDATE SET \
                             model=excluded.model, dim=excluded.dim, vec=excluded.vec, hash=excluded.hash",
                    )
                    .map_err(e)?;
                for (idx, (doc_id, _, hash)) in chunk.iter().enumerate() {
                    let mut v = vecs[idx].clone();
                    normalize(&mut v);
                    ins.execute(params![
                        doc_id,
                        MODEL_NAME,
                        DIM as i64,
                        vec_to_blob(&v),
                        hash
                    ])
                    .map_err(e)?;
                }
            }
            tx.commit().map_err(e)?;
        }
        embedded += chunk.len();
        if (done + 1) % 10 == 0 {
            log::info!("vector index: {}/{} docs embedded", embedded, work.len());
        }
    }

    // Bookkeeping in one more short locked step (same rows, same order).
    {
        let conn = db.lock();
        for (k, v) in [
            ("embed_model", MODEL_NAME.to_string()),
            ("embed_dim", DIM.to_string()),
            ("embed_indexed_at", crate::cfg::util::now_iso_utc()),
        ] {
            conn.execute(
                "INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![k, v],
            )
            .map_err(e)?;
        }
    }

    let total = docs.len();
    let secs = t0.elapsed().as_secs_f64();
    log::info!(
        "vector index: done — {total} docs, {embedded} embedded, {unchanged} unchanged, {deleted} deleted in {secs:.1}s"
    );
    Ok(IndexStats {
        total,
        embedded,
        unchanged,
        deleted,
        secs,
    })
}

fn e(err: rusqlite::Error) -> String {
    format!("sqlite: {err}")
}

// ----------------------------------------------------------------- search ----

/// Top-`depth` (doc_id, cosine) pairs for the query; cosine on L2-normalised
/// vectors (dot product, guarded by a defensive norm check).
pub fn search(conn: &Connection, q: &str, depth: usize) -> Result<Vec<(String, f64)>, String> {
    let be = backend()?;
    let qv = embed_query(&be, q)?;

    let mut stmt = conn
        .prepare("SELECT doc_id, vec FROM embeddings WHERE model = ?1 AND dim = ?2")
        .map_err(e)?;
    let rows: Vec<(String, Vec<u8>)> = stmt
        .query_map(params![MODEL_NAME, DIM as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(e)?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    let qn = qv
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    let mut scored: Vec<(String, f64)> = Vec::with_capacity(rows.len());
    for (doc_id, blob) in rows {
        let v = blob_to_vec(&blob);
        if v.len() != DIM {
            continue;
        }
        let mut dot = 0.0f64;
        let mut vn = 0.0f64;
        for (a, b) in qv.iter().zip(v.iter()) {
            dot += (*a as f64) * (*b as f64);
            vn += (*b as f64) * (*b as f64);
        }
        let cos = if qn > 0.0 && vn > 0.0 {
            dot / (qn * vn.sqrt())
        } else {
            0.0
        };
        scored.push((doc_id, cos));
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(depth);
    Ok(scored)
}

/// kind/title/body/source_ref for vector-only hits (no FTS snippet available).
pub fn doc_row(conn: &Connection, doc_id: &str) -> Option<(String, String, String, String)> {
    conn.query_row(
        "SELECT kind, COALESCE(title,''), COALESCE(body,''), COALESCE(source_ref,'') \
         FROM search_docs WHERE id = ?1",
        params![doc_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .optional()
    .unwrap_or(None)
}

/// Plain-text teaser for hits that have no FTS snippet.
pub fn plain_snippet(body: &str) -> String {
    let one_line = body.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&one_line, 180)
}

/// Ids of existing docs of one kind — importer helper for exact deletes.
pub fn existing_ids(conn: &Connection, kind: &str) -> Result<HashSet<String>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT id FROM search_docs WHERE kind = ?1")?;
    let set = stmt
        .query_map(params![kind], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .collect();
    Ok(set)
}
