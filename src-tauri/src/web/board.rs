//! Board: kanban items («Этап А») — list/create/patch, attached files,
//! checklist steps. Quadrants are frozen ids.

use super::{db_lock, join_result, ok_json, ApiError};
use crate::importers;
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use serde_json::{json, Map, Value};

/// Kanban quadrant ids, frozen («Этап А»).
pub const QUADRANTS: [&str; 4] = ["q1", "q2", "q3", "q4"];


/// Board item row + its attachment files («Этап А»). Files keep the PUT order
/// (`sort`), labels exist in the schema but are always null in v1.
fn item_files(
    conn: &rusqlite::Connection,
    item_id: i64,
) -> Vec<Value> {
    let mut stmt = conn
        .prepare("SELECT path, label FROM item_files WHERE item_id = ?1 ORDER BY sort, rowid")
        .expect("select item files");
    stmt.query_map([item_id], |r| {
        Ok(json!({"path": r.get::<_, String>(0)?, "label": r.get::<_, Option<String>>(1)?}))
    })
    .expect("query item files")
    .filter_map(|r| r.ok())
    .collect()
}

/// Checklist steps of an item in PUT order («Этап B»).
fn item_steps(conn: &rusqlite::Connection, item_id: i64) -> Vec<Value> {
    let mut stmt = conn
        .prepare("SELECT id, text, done FROM item_steps WHERE item_id = ?1 ORDER BY pos, id")
        .expect("select item steps");
    stmt.query_map([item_id], |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "text": r.get::<_, String>(1)?,
            "done": r.get::<_, i64>(2)? != 0,
        }))
    })
    .expect("query item steps")
    .filter_map(|r| r.ok())
    .collect()
}

/// Column list every item query selects, in [`ItemRow::from_row`] order.
const ITEM_COLS: &str = "id, title, quadrant, project, done, description, dedupe_key, \
                         delegation_state, delegation_target, delegation_ref, \
                         delegation_run_id, delegation_started, delegation_ended, delegation_exit";

/// One `items` row, including the «Этап B» delegation columns.
struct ItemRow {
    id: i64,
    title: String,
    quadrant: String,
    project: Option<String>,
    done: i64,
    description: Option<String>,
    dedupe_key: Option<String>,
    delegation_state: String,
    delegation_target: Option<String>,
    delegation_ref: Option<String>,
    delegation_run_id: Option<i64>,
    delegation_started: Option<f64>,
    delegation_ended: Option<f64>,
    delegation_exit: Option<i64>,
}

impl ItemRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRow> {
        Ok(ItemRow {
            id: r.get(0)?,
            title: r.get(1)?,
            quadrant: r.get(2)?,
            project: r.get(3)?,
            done: r.get(4)?,
            description: r.get(5)?,
            dedupe_key: r.get(6)?,
            delegation_state: r.get(7)?,
            delegation_target: r.get(8)?,
            delegation_ref: r.get(9)?,
            delegation_run_id: r.get(10)?,
            delegation_started: r.get(11)?,
            delegation_ended: r.get(12)?,
            delegation_exit: r.get(13)?,
        })
    }
}

/// One item by id (None = unknown), ready for [`item_json`].
fn item_by_id(conn: &rusqlite::Connection, id: i64) -> Option<ItemRow> {
    conn.query_row(
        &format!("SELECT {ITEM_COLS} FROM items WHERE id=?1"),
        [id],
        ItemRow::from_row,
    )
    .ok()
}

fn item_json(conn: &rusqlite::Connection, row: ItemRow) -> Value {
    let mut deps = Vec::new();
    if let Some(p) = &row.project {
        let mut stmt = conn
            .prepare(
                "SELECT l.to_project, l.kind, l.note FROM links l \
                 WHERE l.from_project = ?1 \
                   AND l.to_project IN (SELECT name FROM projects)",
            )
            .expect("select item deps");
        deps = stmt
            .query_map([p], |r| {
                Ok(json!({
                    "to_project": r.get::<_, String>(0)?,
                    "kind": r.get::<_, String>(1)?,
                    "note": r.get::<_, Option<String>>(2)?,
                }))
            })
            .expect("query item deps")
            .filter_map(|r| r.ok())
            .collect();
    }
    // «Этап B»: delegation rides on every item; `null` = never delegated.
    let delegation = if row.delegation_state.is_empty() {
        Value::Null
    } else {
        json!({
            "state": row.delegation_state,
            "target": row.delegation_target,
            "ref": row.delegation_ref,
            "run_id": row.delegation_run_id,
            "started": row.delegation_started,
            "ended": row.delegation_ended,
            "exit": row.delegation_exit,
        })
    };
    json!({
        "id": row.id, "title": row.title, "quadrant": row.quadrant, "project": row.project,
        "done": row.done != 0, "description": row.description, "files": item_files(conn, row.id),
        "steps": item_steps(conn, row.id),
        "dedupe_key": row.dedupe_key,
        "deps": deps,
        "delegation": delegation,
    })
}

fn board_all(conn: &rusqlite::Connection) -> Value {
    let mut board = Map::new();
    for q in QUADRANTS {
        board.insert(q.to_string(), Value::Array(vec![]));
    }
    let mut stmt = conn
        .prepare(&format!("SELECT {ITEM_COLS} FROM items ORDER BY id"))
        .expect("select items");
    let rows: Vec<ItemRow> = stmt
        .query_map([], ItemRow::from_row)
        .expect("query items")
        .filter_map(|r| r.ok())
        .collect();
    for row in rows {
        let q = row.quadrant.clone();
        if let Some(Value::Array(items)) = board.get_mut(&q) {
            items.push(item_json(conn, row));
        }
    }
    Value::Object(board)
}

pub(crate) async fn api_board() -> Result<Response, ApiError> {
    let out = tokio::task::spawn_blocking(|| {
        let conn = db_lock();
        board_all(&conn)
    })
    .await
    .map_err(join_result)?;
    Ok(ok_json(out))
}

fn quadrant_error() -> ApiError {
    ApiError::unprocessable("quadrant must be one of ('q1', 'q2', 'q3', 'q4')")
}

/// Description rule of «Этап А»: string|null, trimmed, ≤10000 chars.
pub const DESCRIPTION_CAP: usize = 10_000;

/// Normalise a client-supplied description. Err = 422 naming the rule.
fn normalize_description(raw: &Value) -> Result<Option<String>, ApiError> {
    match raw {
        Value::Null => Ok(None),
        Value::String(s) => {
            let s = s.trim();
            if s.chars().count() > DESCRIPTION_CAP {
                return Err(ApiError::unprocessable(format!(
                    "description must not exceed {DESCRIPTION_CAP} characters"
                )));
            }
            Ok(Some(s.to_string()))
        }
        _ => Err(ApiError::unprocessable(
            "description must be a string or null",
        )),
    }
}

pub(crate) async fn api_board_create(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let title = v
        .get("title")
        .and_then(|t| t.as_str())
        .ok_or_else(|| ApiError::unprocessable("title must be a string"))?
        .trim()
        .to_string();
    let quadrant = v
        .get("quadrant")
        .and_then(|q| q.as_str())
        .ok_or_else(|| ApiError::unprocessable("quadrant must be a string"))?
        .to_string();
    if !QUADRANTS.contains(&quadrant.as_str()) {
        return Err(quadrant_error());
    }
    if title.is_empty() {
        return Err(ApiError::unprocessable("title must not be empty"));
    }
    let description = match v.get("description") {
        None | Some(Value::Null) => None,
        Some(d) => normalize_description(d)?,
    };
    let project = v.get("project").and_then(|p| p.as_str()).map(String::from);
    // «Этап B»: idempotent-retry key for `atlas board add --dedupe-key`
    let dedupe_key = v
        .get("dedupe_key")
        .and_then(|k| k.as_str())
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty());

    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let conn = db_lock();
        let ts = crate::util::now();
        conn.execute(
            "INSERT INTO items(title,quadrant,project,description,dedupe_key,created_at,updated_at) \
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![title, quadrant, project, description, dedupe_key, ts, ts],
        )
        .map_err(|e| ApiError::bad(format!("{e}")))?;
        let id = conn.last_insert_rowid();
        let out = item_json(
            &conn,
            item_by_id(&conn, id)
                .ok_or_else(|| ApiError::not_found(format!("item {id} not found")))?,
        );
        importers::sync_board_docs(&conn)
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
        Ok(out)
    })
    .await
    .map_err(join_result)??;
    crate::http::web::events::board_changed();
    Ok(ok_json(out))
}

pub(crate) async fn api_board_patch(uri: Uri, body: String) -> Result<Response, ApiError> {
    let path = uri.path();
    let Some(id_raw) = path.rsplit('/').next() else {
        return Err(ApiError::unprocessable("item id required"));
    };
    let item_id: i64 = id_raw
        .parse()
        .map_err(|_| ApiError::unprocessable("item id must be an integer"))?;
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    if v.get("quadrant").map(|q| !q.is_string()).unwrap_or(false) {
        return Err(ApiError::unprocessable("quadrant must be a string"));
    }
    if v.get("title").map(|t| !t.is_string()).unwrap_or(false) {
        return Err(ApiError::unprocessable("title must be a string"));
    }
    if v.get("done").map(|d| !d.is_boolean()).unwrap_or(false) {
        return Err(ApiError::unprocessable("done must be a boolean"));
    }
    // description: absent = keep, null = clear, string = replace («Этап А»)
    let patch_description = match v.get("description") {
        None => None,
        Some(d) => Some(normalize_description(d)?),
    };
    let patch_quadrant = v.get("quadrant").and_then(|q| q.as_str()).map(String::from);
    let patch_title = v.get("title").and_then(|t| t.as_str()).map(String::from);
    let patch_done = v.get("done").and_then(|d| d.as_bool());
    if let Some(q) = &patch_quadrant {
        if !QUADRANTS.contains(&q.as_str()) {
            return Err(quadrant_error());
        }
    }

    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let conn = db_lock();
        let row = item_by_id(&conn, item_id)
            .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?;
        let title = match &patch_title {
            Some(t) => {
                let t = t.trim().to_string();
                if t.is_empty() {
                    return Err(ApiError::unprocessable("title must not be empty"));
                }
                t
            }
            None => row.title.clone(),
        };
        let quadrant = patch_quadrant.unwrap_or_else(|| row.quadrant.clone());
        let done = patch_done.unwrap_or(row.done != 0);
        let description = patch_description.unwrap_or_else(|| row.description.clone());
        conn.execute(
            "UPDATE items SET title=?, quadrant=?, done=?, description=?, updated_at=? WHERE id=?",
            rusqlite::params![title, quadrant, done as i64, description, crate::util::now(), item_id],
        )
        .map_err(|e| ApiError::bad(format!("{e}")))?;
        let out = item_json(
            &conn,
            item_by_id(&conn, item_id)
                .ok_or_else(|| ApiError::not_found(format!("item {item_id} not found")))?,
        );
        importers::sync_board_docs(&conn)
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
        Ok(out)
    })
    .await
    .map_err(join_result)??;
    crate::http::web::events::board_changed();
    Ok(ok_json(out))
}

/// PUT /api/board/item/{id}/files — `{"paths":["/abs/file",…]}`: REPLACE the
/// attachment set (v1 semantics). Every path must exist and be a file (422
/// lists the offenders), duplicates are dropped preserving order, at most 20
/// files. Labels are a schema feature, always null in v1.
pub(crate) async fn api_board_files_put(uri: Uri, body: String) -> Result<Response, ApiError> {
    // path shape: /api/board/item/{id}/files — the id is second from the end
    let seg: Vec<&str> = uri.path().rsplit('/').collect();
    let Some(id_raw) = seg.get(1) else {
        return Err(ApiError::unprocessable("item id required"));
    };
    let item_id: i64 = id_raw
        .parse()
        .map_err(|_| ApiError::unprocessable("item id must be an integer"))?;
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let Some(paths_raw) = v.get("paths").and_then(|p| p.as_array()) else {
        return Err(ApiError::unprocessable(
            "paths must be an array of absolute path strings",
        ));
    };
    let mut paths: Vec<String> = Vec::with_capacity(paths_raw.len());
    for p in paths_raw {
        let Some(s) = p.as_str() else {
            return Err(ApiError::unprocessable(
                "paths must be an array of absolute path strings",
            ));
        };
        if !paths.iter().any(|x| x == s) {
            paths.push(s.trim().to_string());
        }
    }
    if paths.len() > 20 {
        return Err(ApiError::unprocessable(format!(
            "не больше 20 файлов на задачу (сейчас {})",
            paths.len()
        )));
    }
    // every path must exist and be a file; 422 lists all offenders
    let missing: Vec<String> = paths
        .iter()
        .filter(|p| {
            !std::path::Path::new(p).is_file()
        })
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(ApiError::unprocessable(format!(
            "файлы не найдены (или это не файлы): {}",
            missing.join(", ")
        )));
    }

    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let conn = db_lock();
        conn.query_row("SELECT 1 FROM items WHERE id=?1", [item_id], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(|_| ApiError::not_found(format!("item {item_id} not found")))?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| ApiError::bad(format!("{e}")))?;
        tx.execute("DELETE FROM item_files WHERE item_id = ?1", [item_id])
            .map_err(|e| ApiError::bad(format!("{e}")))?;
        for (i, p) in paths.iter().enumerate() {
            tx.execute(
                "INSERT INTO item_files(item_id, path, label, sort) VALUES(?1, ?2, NULL, ?3)",
                rusqlite::params![item_id, p, i as i64],
            )
            .map_err(|e| ApiError::bad(format!("{e}")))?;
        }
        tx.commit().map_err(|e| ApiError::bad(format!("{e}")))?;
        // the doc body carries the attachment paths — refresh it now, not on
        // the next unrelated board mutation
        importers::sync_board_docs(&conn)
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
        Ok(json!({
            "ok": true,
            "files": paths
                .iter()
                .map(|p| json!({"path": p, "label": null}))
                .collect::<Vec<_>>(),
        }))
    })
    .await
    .map_err(join_result)??;
    crate::http::web::events::board_changed();
    Ok(ok_json(out))
}

/// PUT /api/board/item/{id}/steps — checklist steps («Этап B»): REPLACE the
/// set (like files). `{"steps":[{"text":"…","done":false}]}`: texts are
/// trimmed, must be non-empty and ≤50 chars, at most 20 steps, unknown item
/// → 404. `PATCH` semantics are untouched; the completion counter
/// (`done/total`) is the frontend's.
pub(crate) async fn api_board_steps_put(uri: Uri, body: String) -> Result<Response, ApiError> {
    // path shape: /api/board/item/{id}/steps — the id is second from the end
    let seg: Vec<&str> = uri.path().rsplit('/').collect();
    let Some(id_raw) = seg.get(1) else {
        return Err(ApiError::unprocessable("item id required"));
    };
    let item_id: i64 = id_raw
        .parse()
        .map_err(|_| ApiError::unprocessable("item id must be an integer"))?;
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let Some(steps_raw) = v.get("steps").and_then(|p| p.as_array()) else {
        return Err(ApiError::unprocessable(
            "steps must be an array of {text, done?} objects",
        ));
    };
    let mut steps: Vec<(String, bool)> = Vec::with_capacity(steps_raw.len());
    for st in steps_raw {
        let Some(text) = st.get("text").and_then(|t| t.as_str()).map(str::trim) else {
            return Err(ApiError::unprocessable(
                "every step must carry a string \"text\"",
            ));
        };
        if text.is_empty() {
            return Err(ApiError::unprocessable("step text must not be empty"));
        }
        if text.chars().count() > 50 {
            return Err(ApiError::unprocessable(
                "step text не длиннее 50 символов",
            ));
        }
        let done = st.get("done").and_then(|d| d.as_bool()).unwrap_or(false);
        steps.push((text.to_string(), done));
    }
    if steps.len() > 20 {
        return Err(ApiError::unprocessable(format!(
            "не больше 20 шагов на задачу (сейчас {})",
            steps.len()
        )));
    }

    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let conn = db_lock();
        conn.query_row("SELECT 1 FROM items WHERE id=?1", [item_id], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(|_| ApiError::not_found(format!("item {item_id} not found")))?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| ApiError::bad(format!("{e}")))?;
        tx.execute("DELETE FROM item_steps WHERE item_id = ?1", [item_id])
            .map_err(|e| ApiError::bad(format!("{e}")))?;
        for (i, (text, done)) in steps.iter().enumerate() {
            tx.execute(
                "INSERT INTO item_steps(item_id, text, done, pos) VALUES(?1, ?2, ?3, ?4)",
                rusqlite::params![item_id, text, *done as i64, i as i64],
            )
            .map_err(|e| ApiError::bad(format!("{e}")))?;
        }
        tx.commit().map_err(|e| ApiError::bad(format!("{e}")))?;
        // steps are part of the search doc body — refresh it now
        importers::sync_board_docs(&conn)
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
        Ok(json!({
            "ok": true,
            "steps": item_steps(&conn, item_id),
        }))
    })
    .await
    .map_err(join_result)??;
    crate::http::web::events::board_changed();
    Ok(ok_json(out))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::web::support::{call, test_router};
    use axum::http::StatusCode;
    use serde_json::{json, Value};
    /// Checklist steps («Этап B»): PUT replaces the set (trim, ≤50 chars,
    /// cap 20), steps ride on GET /api/board in PUT order, unknown item →
    /// 404, and step texts land in the search doc.
    #[tokio::test]
    async fn board_steps_contract() {
        let r = test_router();
        // a fresh item
        let (st, out) = call(
            &r,
            "POST",
            "/api/board/item",
            Some(r#"{"title":"этап B","quadrant":"q2","project":"lamplife","dedupe_key":"etap-b"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let id = out["id"].as_i64().unwrap();
        assert_eq!(out["dedupe_key"], "etap-b", "{out}");

        // PUT steps: trim + order + done flags
        let body = r#"{"steps":[
            {"text":"  собрать release  ","done":true},
            {"text":"прогнать тесты"}
        ]}"#;
        let (st, out) = call(
            &r,
            "PUT",
            &format!("/api/board/item/{id}/steps"),
            Some(body),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let steps = out["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["text"], "собрать release");
        assert_eq!(steps[0]["done"], true);
        assert_eq!(steps[1]["text"], "прогнать тесты");
        assert_eq!(steps[1]["done"], false);
        assert!(steps[0]["id"].is_number());

        // the board carries them in PUT order
        let (_, board) = call(&r, "GET", "/api/board", None).await;
        let item = board["q2"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == id)
            .expect("item on the board");
        assert_eq!(item["steps"][0]["text"], "собрать release");
        assert_eq!(item["steps"][1]["text"], "прогнать тесты");

        // a second PUT REPLACES the set (no leftovers)
        let (st, out) = call(
            &r,
            "PUT",
            &format!("/api/board/item/{id}/steps"),
            Some(r#"{"steps":[{"text":"единственный шаг"}]}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["steps"].as_array().unwrap().len(), 1);

        // validation: empty text, >50 chars, >20 steps, unknown item, bad bodies
        for (body, want) in [
            (r#"{"steps":[{"text":"   "}]}"#, "empty"),
            (
                r#"{"steps":[{"text":"0123456789012345678901234567890123456789012345678901234567890"}]}"#,
                "50",
            ),
            (
                r#"{"steps":[{"text":"a"},{"text":"b"},{"text":"c"},{"text":"d"},{"text":"e"},{"text":"f"},{"text":"g"},{"text":"h"},{"text":"i"},{"text":"j"},{"text":"k"},{"text":"l"},{"text":"m"},{"text":"n"},{"text":"o"},{"text":"p"},{"text":"q"},{"text":"r"},{"text":"s"},{"text":"t"},{"text":"u"}]}"#,
                "20",
            ),
            (r#"{"steps":[{"done":true}]}"#, "text"),
        ] {
            let (st, out) = call(
                &r,
                "PUT",
                &format!("/api/board/item/{id}/steps"),
                Some(body),
            )
            .await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body} → {out}");
            let detail = out["detail"].as_str().unwrap_or_default();
            assert!(detail.contains(want), "{body} → {detail}");
        }
        let (st, out) = call(
            &r,
            "PUT",
            "/api/board/item/999999/steps",
            Some(r#"{"steps":[{"text":"x"}]}"#),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{out}");

        // step texts are findable: the search doc carries them
        let (st, out) = call(
            &r,
            "PUT",
            &format!("/api/board/item/{id}/steps"),
            Some(r#"{"steps":[{"text":"шуруповёрт калибровка уникальная"}]}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let (st, hits) = call(&r, "GET", "/api/search?q=шуруповёрт+калибровка", None).await;
        assert_eq!(st, StatusCode::OK, "{hits}");
        assert!(
            hits["hits"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["source_ref"] == format!("board:{id}") && h["kind"] == "board_item"),
            "step text must be searchable: {hits}"
        );
    }

    /// Racing double PUT (the frontend's fast-checkbox-toggles shape): two
    /// REPLACEs in flight at once must land as ONE whole set — either the
    /// first or the second, never a mix, never leftovers, no `item_steps`
    /// row anomalies — and the winner must survive a subsequent board read.
    #[tokio::test]
    async fn board_steps_double_put_lands_intact() {
        let r = test_router();
        let (st, out) = call(
            &r,
            "POST",
            "/api/board/item",
            Some(r#"{"title":"двойной тумблер шагов","quadrant":"q3"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let id = out["id"].as_i64().unwrap();

        let body_a = r#"{"steps":[{"text":"шаг А","done":true},{"text":"шаг Б","done":false}]}"#;
        let body_b = r#"{"steps":[{"text":"шаг А","done":false},{"text":"шаг Б","done":true}]}"#;
        let uri_a = format!("/api/board/item/{id}/steps");
        let uri_b = uri_a.clone();
        let ra = call(&r, "PUT", &uri_a, Some(body_a));
        let rb = call(&r, "PUT", &uri_b, Some(body_b));
        let ((sta, outa), (stb, outb)) = tokio::join!(ra, rb);
        assert_eq!(sta, StatusCode::OK, "{outa}");
        assert_eq!(stb, StatusCode::OK, "{outb}");

        // board read after the race: exactly one whole set survives, in order
        let (_, board) = call(&r, "GET", "/api/board", None).await;
        let item = board["q3"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == id)
            .expect("item on the board");
        let got: Vec<(String, bool)> = item["steps"]
            .as_array()
            .expect("steps array")
            .iter()
            .map(|s| {
                (
                    s["text"].as_str().expect("step text").to_string(),
                    s["done"].as_bool().expect("step done"),
                )
            })
            .collect();
        let set_a = vec![("шаг А".to_string(), true), ("шаг Б".to_string(), false)];
        let set_b = vec![("шаг А".to_string(), false), ("шаг Б".to_string(), true)];
        assert!(
            got == set_a || got == set_b,
            "race must leave one whole set, got a mix: {got:?}"
        );
    }

    /// `atlas board …` CLI verbs end to end: the test serves the real router
    /// on an ephemeral port and drives the sync std-TCP client. The server
    /// lives on a DEDICATED OS thread with its own current_thread runtime —
    /// the production shape (main.rs) — so the blocking client on the test
    /// thread cannot starve it under full-suite parallel load. Exit contract:
    /// 0 ok · 1 runtime · 2 usage (parse) · 3 unreachable; `--dedupe-key`
    /// returns the existing id («deduped»).
    #[tokio::test]
    async fn board_cli_verbs() {
        let r = test_router();
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral bind");
        let addr = std_listener.local_addr().expect("addr");
        let r_srv = r.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("cli test runtime");
            std_listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            rt.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(std_listener).expect("async listener");
                let _ = axum::serve(listener, r_srv).await;
            });
        });
        let url = format!("http://{addr}");

        // warm up: under full-suite parallel load the fresh server thread can
        // stay off-CPU past the client's 10 s socket timeout — wait until it
        // answers before driving the verbs (it keeps answering after that)
        let mut up = false;
        for _ in 0..30 {
            match crate::cli::http_get(&url, "/api/ui") {
                Ok((200, _)) => {
                    up = true;
                    break;
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(200)),
            }
        }
        assert!(up, "ephemeral board server never answered");

        // Even served once, the tiny server thread can be starved past the
        // client's 10 s socket timeout by the parallel storm — and a sibling
        // test (`created_row_survives_sync`) holds the SHARED DB lock for about
        // a minute while it loads the local fastembed model inside sync, which
        // stalls every DB endpoint for that whole window. Every verb driven
        // below is idempotent (GETs, dedupe-keyed adds), so a read-timeout
        // Unreachable is retried until a generous deadline instead of a fixed
        // count (the stall is longer than the fixed count survived).
        fn retry_unreach<F: FnMut() -> crate::cli::CliExit>(
            mut f: F,
        ) -> crate::cli::CliExit {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
            loop {
                let out = f();
                let retry = matches!(
                    &out,
                    crate::cli::CliExit::Unreachable(e) if e.contains("os error 11")
                );
                if !retry || std::time::Instant::now() > deadline {
                    return out;
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }

        // usage errors never touch the network (exit 2)
        for bad in [
            vec!["add".to_string()],
            vec![
                "add".to_string(),
                "т".to_string(),
                "--quadrant".to_string(),
                "q9".to_string(),
            ],
            vec!["add".to_string(), "т".to_string(), "--dedupe-key".to_string()],
            vec!["frobnicate".to_string()],
            vec!["list".to_string(), "--quadrant".to_string(), "q5".to_string()],
        ] {
            assert!(
                crate::cli::parse_board_args(bad.clone()).is_err(),
                "must be a usage error: {bad:?}"
            );
        }

        // add → id=N (exit 0)
        let out = retry_unreach(|| {
            crate::cli::board_add_main(crate::cli::BoardAddArgs {
                title: "CLI задача".into(),
                project: Some("atlas".into()),
                quadrant: "q1".into(),
                description: None,
                dedupe_key: Some("cli-uniq-1".into()),
                url: url.clone(),
            })
        });
        assert_eq!(out.code(), 0, "{out:?}");
        let crate::cli::CliExit::Ok(line) = out else {
            panic!("expected Ok");
        };
        // a starved first attempt may have landed server-side and been
        // answered with «deduped» on the retry — the id is the same either way
        let id1: i64 = line
            .trim_start_matches("id=")
            .trim_end_matches(" deduped")
            .parse()
            .expect("id=N[ deduped] printed");

        // the same --dedupe-key again → the existing id, «deduped», exit 0
        let out = retry_unreach(|| {
            crate::cli::board_add_main(crate::cli::BoardAddArgs {
            title: "CLI задача повтор".into(),
            project: Some("atlas".into()),
            quadrant: "q3".into(),
            description: None,
            dedupe_key: Some("cli-uniq-1".into()),
                url: url.clone(),
            })
        });
        assert_eq!(out.code(), 0, "{out:?}");
        let crate::cli::CliExit::Ok(line) = out else {
            panic!("expected Ok");
        };
        assert_eq!(line, format!("id={id1} deduped"));

        // a second distinct item lands in q2
        let out = retry_unreach(|| {
            crate::cli::board_add_main(crate::cli::BoardAddArgs {
                title: "CLI вторая".into(),
                project: None,
                quadrant: "q2".into(),
                description: None,
                dedupe_key: Some("cli-uniq-2".into()),
                url: url.clone(),
            })
        });
        assert_eq!(out.code(), 0);
        let id2: i64 = match out {
            crate::cli::CliExit::Ok(l) => l.trim_start_matches("id=").parse().unwrap(),
            other => panic!("expected Ok: {other:?}"),
        };

        // unreachable → exit 3 (closed port, named refusal)
        let out = crate::cli::board_status_main(crate::cli::BoardStatusArgs {
            json: false,
            url: "http://127.0.0.1:1".into(),
        });
        assert_eq!(out.code(), 3, "{out:?}");

        // list --json carries the items with steps/files counts fields present
        let out = retry_unreach(|| {
            crate::cli::board_list_main(crate::cli::BoardListArgs {
                quadrant: Some("q2".into()),
                json: true,
                url: url.clone(),
            })
        });
        assert_eq!(out.code(), 0);
        let crate::cli::CliExit::Ok(json_line) = out else {
            panic!("expected Ok");
        };
        let arr: serde_json::Value = serde_json::from_str(&json_line).expect("json array");
        let arr = arr.as_array().unwrap();
        assert!(arr.iter().any(|i| i["id"] == id2 && i["quadrant"] == "q2"));
        assert!(arr.iter().all(|i| i["steps"].is_array() && i["files"].is_array()));
        // the deduped add must NOT have created a q3 twin
        assert!(!arr.iter().any(|i| i["quadrant"] == "q3"));

        // two open steps on the q2 item
        let (st, out) = call(
            &r,
            "PUT",
            &format!("/api/board/item/{id2}/steps"),
            Some(r#"{"steps":[{"text":"шаг раз"},{"text":"шаг два"}]}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let after = match retry_unreach(|| {
            crate::cli::board_status_main(crate::cli::BoardStatusArgs {
                json: true,
                url: url.clone(),
            })
        }) {
            crate::cli::CliExit::Ok(l) => serde_json::from_str::<serde_json::Value>(&l).unwrap(),
            other => panic!("expected Ok: {other:?}"),
        };
        // the shared test DB serves parallel sibling tests — exact deltas are
        // racy, so assert the shape and that MY items/steps are visible
        for key in ["q1", "q2", "q3", "q4", "done", "open_steps"] {
            assert!(after[key].is_i64(), "status must carry {key}: {after}");
        }
        assert!(after["q1"].as_i64().unwrap() >= 1, "{after}");
        assert!(after["q2"].as_i64().unwrap() >= 1, "{after}");
        assert!(after["open_steps"].as_i64().unwrap() >= 2, "{after}");
        // human status is one line naming the counts
        let out = retry_unreach(|| {
            crate::cli::board_status_main(crate::cli::BoardStatusArgs {
                json: false,
                url: url.clone(),
            })
        });
        assert_eq!(out.code(), 0);
        let crate::cli::CliExit::Ok(line) = out else {
            panic!("expected Ok");
        };
        assert!(line.starts_with("q1=") && line.contains("open_steps="), "{line}");
    }

    /// «Этап А» board contract: description on create/patch (trim, cap, null
    /// clears, absent keeps), attachments REPLACE-set via PUT (existence,
    /// dedupe, cap 20, order), /api/board carries both, unknown item → 404.
    #[tokio::test]
    async fn board_description_and_files_contract() {
        let r = test_router();
        let dir = std::env::temp_dir().join(format!("atlas-board-a-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let f1 = dir.join("a.md");
        let f2 = dir.join("b.md");
        std::fs::write(&f1, "1").unwrap();
        std::fs::write(&f2, "2").unwrap();
        let f1s = f1.to_string_lossy().into_owned();
        let f2s = f2.to_string_lossy().into_owned();

        // create with a description
        let (st, out) = call(
            &r,
            "POST",
            "/api/board/item",
            Some(r#"{"title":"починить сборку","quadrant":"q1","project":"lamplife","description":"  сломался release после обновления  "}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let id = out["id"].as_i64().unwrap();
        assert_eq!(
            out["description"],
            "сломался release после обновления",
            "description is trimmed on create"
        );
        assert_eq!(out["files"], json!([]));

        // PUT files: dedupe preserving order, replace-set semantics
        let body = serde_json::json!({"paths": [f2s.clone(), f1s.clone(), f2s.clone()]}).to_string();
        let (st, out) = call(&r, "PUT", &format!("/api/board/item/{id}/files"), Some(&body)).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(
            out["files"],
            json!([{"path": f2s, "label": null}, {"path": f1s, "label": null}]),
            "order preserved, duplicates dropped"
        );
        // a different PUT REPLACES the set
        let (st, out) = call(
            &r,
            "PUT",
            &format!("/api/board/item/{id}/files"),
            Some(&serde_json::json!({"paths": [f1s.clone()]}).to_string()),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["files"], json!([{"path": f1s, "label": null}]));

        // board carries description + files
        let (st, board) = call(&r, "GET", "/api/board", None).await;
        assert_eq!(st, StatusCode::OK);
        let item = board["q1"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == id)
            .expect("item on the board");
        assert_eq!(item["description"], "сломался release после обновления");
        assert_eq!(item["files"][0]["path"], f1.to_string_lossy().to_string());

        // PATCH: set, absent keeps, null clears
        let (st, out) = call(
            &r,
            "PATCH",
            &format!("/api/board/item/{id}"),
            Some(r#"{"description":"новый бриф"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["description"], "новый бриф");
        let (st, out) = call(
            &r,
            "PATCH",
            &format!("/api/board/item/{id}"),
            Some(r#"{"title":"починить сборку"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["description"], "новый бриф", "absent description is kept");
        let (st, out) = call(
            &r,
            "PATCH",
            &format!("/api/board/item/{id}"),
            Some(r#"{"description":null}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["description"], Value::Null);

        // PATCH validation: too long → 422 naming the cap; wrong type → 422
        let long = "x".repeat(DESCRIPTION_CAP + 1);
        let (st, out) = call(
            &r,
            "PATCH",
            &format!("/api/board/item/{id}"),
            Some(&serde_json::json!({"description": long}).to_string()),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        assert!(out["detail"].as_str().unwrap_or_default().contains("10000"));
        let (st, out) = call(
            &r,
            "PATCH",
            &format!("/api/board/item/{id}"),
            Some(r#"{"description":42}"#),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");

        // PUT validation: missing file listed; cap 20; unknown item → 404
        let ghost = dir.join("ghost.md").to_string_lossy().into_owned();
        let (st, out) = call(
            &r,
            "PUT",
            &format!("/api/board/item/{id}/files"),
            Some(&serde_json::json!({"paths": [f1s, ghost.clone()]}).to_string()),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        assert!(
            out["detail"].as_str().unwrap_or_default().contains(&ghost),
            "422 lists the missing file: {out}"
        );
        let many: Vec<String> = (0..21)
            .map(|i| dir.join(format!("f{i}.txt")).to_string_lossy().into_owned())
            .collect();
        let (st, out) = call(
            &r,
            "PUT",
            &format!("/api/board/item/{id}/files"),
            Some(&serde_json::json!({"paths": many}).to_string()),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        assert!(out["detail"].as_str().unwrap_or_default().contains("20"));
        let (st, out) = call(
            &r,
            "PUT",
            "/api/board/item/999999/files",
            Some(r#"{"paths":[]}"#),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{out}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
