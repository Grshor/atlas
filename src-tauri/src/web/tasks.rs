//! Project make-tasks: `GET /api/project/{name}/tasks`, run/status/kill of
//! `make <target>` children (one active run per project).

use super::projects::project_row_or_404;
use super::{join_result, ok_json, ApiError};
use crate::tasks;
use axum::extract::Path as AxumPath;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// GET /api/project/{name}/tasks — the project's makefile targets.
pub(crate) async fn api_project_tasks(AxumPath(name): AxumPath<String>) -> Result<Response, ApiError> {
    let row = project_row_or_404(name).await?;
    let Some(dir) = row
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
    else {
        return Ok(ok_json(json!({"ok": true, "makefile": null, "tasks": []})));
    };
    let out = tokio::task::spawn_blocking(move || {
        let Some(mk) = tasks::find_makefile(&dir) else {
            return json!({"ok": true, "makefile": null, "tasks": []});
        };
        let text = String::from_utf8_lossy(&std::fs::read(&mk).unwrap_or_default()).into_owned();
        let list: Vec<Value> = tasks::parse_makefile(&text)
            .into_iter()
            .map(|t| json!({"name": t.name, "doc": t.doc}))
            .collect();
        json!({"ok": true, "makefile": mk.to_string_lossy(), "tasks": list})
    })
    .await
    .map_err(join_result)?;
    Ok(ok_json(out))
}

/// POST /api/project/{name}/tasks/run — start `make <target>` (409 if busy).
pub(crate) async fn api_project_task_run(
    AxumPath(name): AxumPath<String>,
    body: String,
) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let target = v
        .get("target")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| ApiError::unprocessable("target must be a non-empty string"))?
        .to_string();
    let row = project_row_or_404(name).await?;
    let Some(dir) = row
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
    else {
        return Err(ApiError::unprocessable(format!(
            "у проекта '{}' нет пути — make-задачу запустить нельзя",
            row.name
        )));
    };
    match tasks::start(&row.name, &dir, &target) {
        Ok(id) => Ok(ok_json(json!({"ok": true, "id": id}))),
        Err(tasks::StartError::Active) => Ok((
            StatusCode::CONFLICT,
            Json(json!({"detail": "у проекта уже идёт задача"})),
        )
            .into_response()),
        Err(tasks::StartError::Spawn(e)) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("не удалось запустить make: {e}"),
        )),
    }
}

/// GET /api/project/{name}/tasks/run — status of the (last) make run.
pub(crate) async fn api_project_task_status(AxumPath(name): AxumPath<String>) -> Result<Response, ApiError> {
    let row = project_row_or_404(name).await?;
    let out = tasks::status(&row.name).unwrap_or_else(|| {
        json!({
            "active": false,
            "id": null,
            "exit": null,
            "lines": [],
            "started_at": null,
            "duration_s": null,
        })
    });
    Ok(ok_json(out))
}

/// POST /api/project/{name}/tasks/kill — SIGTERM the active child, if any.
pub(crate) async fn api_project_task_kill(AxumPath(name): AxumPath<String>) -> Result<Response, ApiError> {
    let row = project_row_or_404(name).await?;
    if tasks::kill(&row.name) {
        Ok(ok_json(json!({"ok": true})))
    } else {
        Ok(ok_json(json!({"ok": false, "detail": "нет активной задачи"})))
    }
}

