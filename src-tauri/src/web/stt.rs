//! Push-to-talk voice input: `POST /api/stt/start` (pw-record spawn),
//! `POST /api/stt/stop` (voxtype transcription), `GET /api/stt/status`.

use super::{ok_json, AppState, ApiError};
use crate::stt;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;


/// POST /api/stt/start — push-to-talk: spawn `pw-record` (default source,
/// 16 kHz mono s16) into a temp wav. Second start while one is live → 409.
pub(crate) async fn api_stt_start(State(state): State<AppState>) -> Result<Response, ApiError> {
    match state.stt.start() {
        Ok(v) => Ok(ok_json(v)),
        Err(stt::StartError::Busy) => {
            let mut res = ok_json(json!({"ok": false, "detail": "запись уже идёт"}));
            *res.status_mut() = StatusCode::CONFLICT;
            Ok(res)
        }
        Err(stt::StartError::Failed(e)) => Ok(ok_json(json!({
            "ok": false, "detail": format!("pw-record не запущен: {e}"),
        }))),
    }
}

/// POST /api/stt/stop — SIGTERM the recorder (WAV finalised), transcribe
/// synchronously (fresh voxtype: ~4.5 s cold, ~1–2 s warm — the frontend
/// shows a spinner and keeps a long timeout), delete the wav. The text is
/// transcribed against a throwaway config with `language = "ru"` (the daemon
/// config says "en"; see «Голосовой ввод»). No active recording → 409,
/// unless the 60 s auto-stop just finished and its result is uncollected —
/// then that result is handed over.
pub(crate) async fn api_stt_stop(State(state): State<AppState>) -> Result<Response, ApiError> {
    match state.stt.stop().await {
        Ok(v) => Ok(ok_json(v)),
        Err(stt::NotRecording) => {
            let mut res = ok_json(json!({"ok": false, "detail": "запись не активна"}));
            *res.status_mut() = StatusCode::CONFLICT;
            Ok(res)
        }
    }
}

/// GET /api/stt/status — `{"recording":bool,"transcribing":bool}`; the
/// frontend polls it while recording to survive page switches.
pub(crate) async fn api_stt_status(State(state): State<AppState>) -> Result<Response, ApiError> {
    Ok(ok_json(state.stt.status()))
}


