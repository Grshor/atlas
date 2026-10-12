//! Source registry: `GET /api/sources` (importer status) and
//! `POST /api/sync` (re-sync every source into the DB, serialised).

use super::{db_lock, join_result, ok_json, ApiError};
use crate::db;
use crate::importers;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Map, Value};


pub(crate) async fn api_sources() -> Result<Response, ApiError> {
    let out = tokio::task::spawn_blocking(|| {
        let conn = db_lock();
        let mut stmt = conn
            .prepare("SELECT name, count, ok, last_sync, meta FROM sources ORDER BY name")
            .expect("select sources");
        let rows: Vec<(String, i64, i64, f64, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .expect("query sources")
            .filter_map(|r| r.ok())
            .collect();
        let sources: Vec<Value> = rows
            .into_iter()
            .map(|(name, count, ok, last_sync, meta)| {
                let mut row = Map::new();
                row.insert("name".into(), json!(name));
                row.insert("count".into(), json!(count));
                row.insert("ok".into(), json!(ok != 0));
                row.insert("last_sync".into(), json!(last_sync));
                if let Some(meta) = meta {
                    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&meta) {
                        for (k, v) in m {
                            row.insert(k, v);
                        }
                    }
                }
                Value::Object(row)
            })
            .collect();
        json!({"sources": sources})
    })
    .await
    .map_err(join_result)?;
    Ok(ok_json(out))
}

pub(crate) async fn api_sync() -> Result<Response, ApiError> {
    let _guard = importers::SYNC_LOCK.lock().await;
    let summary = importers::sync_all(db::db())
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    let mut out = Map::new();
    out.insert("ok".into(), json!(true));
    if let Value::Object(m) = summary {
        for (k, v) in m {
            out.insert(k, v);
        }
    }
    Ok(ok_json(Value::Object(out)))
}

