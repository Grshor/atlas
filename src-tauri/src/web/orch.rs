//! Orchestrator chat surface: `POST /api/orch` (SSE turn stream),
//! status/history/approve/cancel/restart.

use super::{ok_json, AppState, ApiError};
use crate::orch::{ApproveError, Orchestrator, TURN_TIMEOUT_SECS};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::Response;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};


/// A turn is already running: one gate, serialised turns, 409 naming the
/// running prompt (the first 80 chars).
fn turn_busy_error(orch: &Orchestrator) -> ApiError {
    let head: String = orch.current_prompt().chars().take(80).collect();
    ApiError::new(
        StatusCode::CONFLICT,
        format!("ход уже идёт: {head}"),
    )
}

/// POST /api/orch — the prompt entry. Body `{"prompt":"…"}`; the legacy
/// `{"message":"…"}` is accepted too. The response streams the turn's canonical
/// events as SSE. A second prompt while a turn runs → 409.
pub(crate) async fn api_orch(State(state): State<AppState>, body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|_| ApiError::unprocessable("body must be {\"prompt\": str}"))?;
    let message = v
        .get("prompt")
        .or_else(|| v.get("message"))
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ApiError::unprocessable("body must be {\"prompt\": str}"))?;

    // One turn at a time: take the gate now (owned guard → movable into the
    // stream); a busy orchestrator is a 409, never a silent queue.
    let gate = Arc::clone(&state.orch.gate);
    let guard = gate
        .clone()
        .try_lock_owned()
        .map_err(|_| turn_busy_error(&state.orch))?;

    let orch = Arc::clone(&state.orch);
    let stream = async_stream::stream! {
        let _guard = guard;
        // Subscribe only once the gate is ours, so `turn_start` is not missed.
        let mut rx = orch.subscribe();
        orch.prompt(&message).await;
        let deadline = Instant::now() + Duration::from_secs(TURN_TIMEOUT_SECS);
        loop {
            match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(ev)) => {
                    let ty = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    let frame = format!("data: {ev}\n\n");
                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(frame));
                    if ty == "done" || ty == "error" {
                        break;
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                Err(_timeout) => {
                    if Instant::now() > deadline {
                        // hung turn: close it out honestly for every subscriber
                        if let Some(ev) = orch.finish(false) {
                            yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(format!("data: {ev}\n\n")));
                        }
                        break;
                    }
                }
            }
        }
    };
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap())
}

/// GET /api/orch/status.
pub(crate) async fn api_orch_status(State(state): State<AppState>) -> Result<Response, ApiError> {
    Ok(ok_json(state.orch.status_json()))
}

/// GET /api/orch/history — the in-memory ring (empty after a server restart).
pub(crate) async fn api_orch_history(State(state): State<AppState>) -> Result<Response, ApiError> {
    Ok(ok_json(json!({"events": state.orch.history()})))
}

/// POST /api/orch/approve — `{"id":"…","allow":true|false}`.
pub(crate) async fn api_orch_approve(
    State(state): State<AppState>,
    body: String,
) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let id = v
        .get("id")
        .and_then(|i| i.as_str())
        .map(str::trim)
        .filter(|i| !i.is_empty())
        .ok_or_else(|| ApiError::unprocessable("id must be a non-empty string"))?;
    let allow = v
        .get("allow")
        .and_then(|a| a.as_bool())
        .ok_or_else(|| ApiError::unprocessable("allow must be a boolean"))?;
    match state.orch.respond_approval(id, allow).await {
        Ok(()) => Ok(ok_json(json!({"ok": true, "id": id, "allow": allow}))),
        Err(ApproveError::Unknown) => Err(ApiError::not_found(format!(
            "подтверждение '{id}' не найдено (неизвестный или истёкший id)"
        ))),
        Err(ApproveError::Channel(e)) => Err(ApiError::unprocessable(format!(
            "rpc-канал оркестратора недоступен: {e}"
        ))),
    }
}

/// POST /api/orch/cancel — interrupt the running turn (rpc `abort`).
pub(crate) async fn api_orch_cancel(State(state): State<AppState>) -> Result<Response, ApiError> {
    Ok(ok_json(state.orch.cancel().await))
}

/// POST /api/orch/restart — kill the rpc child; the next turn respawns it.
pub(crate) async fn api_orch_restart(State(state): State<AppState>) -> Result<Response, ApiError> {
    let had = state.orch.restart().await;
    Ok(ok_json(json!({"ok": true, "restarted": true, "had_process": had})))
}

