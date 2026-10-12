//! Notification ring: `POST /api/notify` (push), `GET /api/notify` (feed),
//! `POST /api/notify/read` (mark one/all read). Storage lives in db.rs.

use super::{db_lock, join_result, ok_json, query_param, ApiError};
use crate::db;
use axum::http::Uri;
use axum::response::Response;
use serde_json::{json, Value};

// (insert + ring trim + unread count live in db.rs, shared with the task runner)

pub(crate) async fn api_notify_post(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let text = v
        .get("text")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if text.is_empty() {
        return Err(ApiError::unprocessable("text must be a non-empty string"));
    }
    let level = v
        .get("level")
        .and_then(|l| l.as_str())
        .unwrap_or("info")
        .to_string();
    if !matches!(level.as_str(), "info" | "warn" | "err") {
        return Err(ApiError::unprocessable("level must be one of info, warn, err"));
    }
    let project = v.get("project").and_then(|p| p.as_str()).map(String::from);
    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let conn = db_lock();
        let id = db::insert_notification(&conn, &level, project.as_deref(), &text)
            .map_err(|e| ApiError::bad(format!("{e}")))?;
        let unread = db::unread_count(&conn).map_err(|e| ApiError::bad(format!("{e}")))?;
        Ok(json!({"ok": true, "id": id, "unread": unread}))
    })
    .await
    .map_err(join_result)??;
    Ok(ok_json(out))
}

pub(crate) async fn api_notify_get(uri: Uri) -> Result<Response, ApiError> {
    let unread_only = query_param(&uri, "unread").as_deref() == Some("1");
    let limit: usize = match query_param(&uri, "limit") {
        Some(s) => s
            .parse()
            .map_err(|_| ApiError::unprocessable("limit must be an integer"))?,
        None => 50,
    };
    if limit == 0 {
        return Err(ApiError::unprocessable("limit must be >= 1"));
    }
    let limit = limit.min(db::NOTIFY_RING);
    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let conn = db_lock();
        let sql = if unread_only {
            "SELECT id, ts, level, project, text, read FROM notifications \
             WHERE read = 0 ORDER BY id DESC LIMIT ?1"
        } else {
            "SELECT id, ts, level, project, text, read FROM notifications \
             ORDER BY id DESC LIMIT ?1"
        };
        let mut stmt = conn.prepare(sql).expect("select notifications");
        let items: Vec<Value> = stmt
            .query_map([limit as i64], |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "ts": r.get::<_, f64>(1)?,
                    "level": r.get::<_, String>(2)?,
                    "project": r.get::<_, Option<String>>(3)?,
                    "text": r.get::<_, String>(4)?,
                    "read": r.get::<_, i64>(5)? != 0,
                }))
            })
            .expect("query notifications")
            .filter_map(|r| r.ok())
            .collect();
        let unread = db::unread_count(&conn).map_err(|e| ApiError::bad(format!("{e}")))?;
        Ok(json!({"items": items, "unread": unread}))
    })
    .await
    .map_err(join_result)??;
    Ok(ok_json(out))
}

pub(crate) async fn api_notify_read(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let all = v.get("all").and_then(|a| a.as_bool()).unwrap_or(false);
    let id = v.get("id").and_then(|i| i.as_i64());
    if !all && id.is_none() {
        return Err(ApiError::unprocessable(
            "body must be {\"id\": N} or {\"all\": true}",
        ));
    }
    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let conn = db_lock();
        if all {
            conn.execute("UPDATE notifications SET read = 1 WHERE read = 0", [])
                .map_err(|e| ApiError::bad(format!("{e}")))?;
        } else {
            let id = id.expect("checked above");
            let changed = conn
                .execute("UPDATE notifications SET read = 1 WHERE id = ?1", [id])
                .map_err(|e| ApiError::bad(format!("{e}")))?;
            if changed == 0 {
                let exists: Option<i64> = conn
                    .query_row("SELECT 1 FROM notifications WHERE id = ?1", [id], |r| r.get(0))
                    .ok();
                if exists.is_none() {
                    return Err(ApiError::not_found(format!("notification {id} not found")));
                }
                // already read → idempotent ok
            }
        }
        let unread = db::unread_count(&conn).map_err(|e| ApiError::bad(format!("{e}")))?;
        Ok(json!({"ok": true, "unread": unread}))
    })
    .await
    .map_err(join_result)??;
    Ok(ok_json(out))
}

