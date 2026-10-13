//! Shared HTTP plumbing: app state, the `ApiError` envelope, and the helpers
//! several domain modules borrow. Domain handlers live in sibling modules
//! (`web/theme.rs`, `web/board.rs`, …); the router assembles here.
//! Contract: endpoint-for-endpoint port of atlas/server.py — route strings,
//! status codes and payload shapes are frozen (see README API table).

pub(crate) mod theme;
pub(crate) mod projects;
pub(crate) mod board;
pub(crate) mod delegate_routes;
pub(crate) mod deps;
pub(crate) mod events;
pub(crate) mod search;
pub(crate) mod orch;
pub(crate) mod settings;
pub(crate) mod notify;
pub(crate) mod open;
pub(crate) mod clipboard;
pub(crate) mod terminal;
pub(crate) mod ui;
pub(crate) mod windows;
pub(crate) mod patrol;
pub(crate) mod focus;
pub(crate) mod sources;
pub(crate) mod session;
pub(crate) mod stt;
pub(crate) mod tasks;
pub(crate) mod assets;

pub use theme::spawn_theme_watcher;
pub use assets::spawn_assets_watcher;

#[cfg(test)]
pub(crate) mod support;

use crate::db;
use crate::orch::Orchestrator;
use crate::stt::SttState;
use crate::util::parse_query;
use axum::body::Body;
use axum::http::{
    header, HeaderValue, Method, Request, Response as HttpResponse, StatusCode, Uri,
};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::Arc;

// Handlers assembled by [`router`] below, one `use` per domain module.
use assets::static_handler;
use board::{api_board, api_board_create, api_board_files_put, api_board_patch, api_board_steps_put};
use clipboard::api_clipboard_image;
use delegate_routes::{
    api_board_accept, api_board_reject, api_delegate_cancel, api_delegate_diff,
    api_delegate_preview, api_delegate_reset, api_delegate_run, api_delegate_start,
};
use deps::api_deps;
use focus::{api_focus, api_focus_targets, api_herdr, api_herdr_focus};
use notify::{api_notify_get, api_notify_post, api_notify_read};
use open::api_open;
use orch::{
    api_orch, api_orch_approve, api_orch_cancel, api_orch_history, api_orch_restart,
    api_orch_status,
};
use patrol::api_patrol;
use projects::{
    api_project_files, api_project_source_create, api_projects, api_projects_archive,
    api_projects_create, api_projects_favorite, api_projects_weburl,
};
use search::api_search;
use session::{api_session, api_session_stream};
use settings::{api_settings_get, api_settings_patch};
use sources::{api_sources, api_sync};
use stt::{api_stt_start, api_stt_status, api_stt_stop};
use tasks::{
    api_project_task_kill, api_project_task_run, api_project_task_status, api_project_tasks,
};
use terminal::{
    api_terminal_kill, api_terminal_open, api_terminal_restart_id, api_terminal_restart_orch,
    api_terminal_sessions, api_terminal_wezterm_dock, api_terminal_wezterm_focus,
    api_terminal_wezterm_hide, api_terminal_wezterm_show, api_terminal_wezterm_state,
    api_terminal_wezterm_tail, api_terminal_ws,
};
use theme::{api_theme, api_theme_stream};
use ui::api_ui;
use windows::{api_windows_focus, api_windows_minimize, api_windows_restore};

#[derive(Clone)]
pub struct AppState {
    pub orch: Arc<Orchestrator>,
    /// Push-to-talk voice input (recorder + local transcription).
    pub stt: Arc<SttState>,
}

// ---------------------------------------------------------------- errors ----

pub struct ApiError {
    status: StatusCode,
    msg: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        ApiError {
            status,
            msg: msg.into(),
        }
    }
    pub(crate) fn bad(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }
    pub(crate) fn unprocessable(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, msg)
    }
    pub(crate) fn not_found(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, msg)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status;
        (status, Json(json!({"detail": self.msg}))).into_response()
    }
}

pub(crate) fn ok_json(v: Value) -> Response {
    Json(v).into_response()
}

pub(crate) fn join_result(e: tokio::task::JoinError) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("blocking task: {e}"),
    )
}

pub(crate) fn db_lock() -> std::sync::MutexGuard<'static, rusqlite::Connection> {
    db::db_static().lock()
}


pub(crate) fn query_param(uri: &Uri, key: &str) -> Option<String> {
    parse_query(uri.query().unwrap_or(""))
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

// ------------------------------------------------- shared herdr helpers ----

/// Persist the workspace id a project's herdr artifacts were found under.
/// Used by the focus planner and by `source_create_herdr`.
pub(crate) fn persist_ws(name: &str, ws: &str) -> Result<(), String> {
    let conn = db_lock();
    conn.execute(
        "UPDATE projects SET herdr_workspace_id = ?1 WHERE name = ?2",
        rusqlite::params![ws, name],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// The importer answered with a usable snapshot (`ok:true`).
pub(crate) fn herdr_snapshot_ok(herdr: &Value) -> bool {
    herdr.get("ok").and_then(|o| o.as_bool()).unwrap_or(false)
}

/// True once THIS process successfully bound the HTTP port — `port_owner: self`.
/// A GUI that fell back to a foreign backend keeps it false.
pub static PORT_SELF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// ------------------------------------------------------------------ CORS ---


fn origin_allowed(origin: &str) -> bool {
    // fullmatch: (tauri://localhost|http://(tauri\.localhost|127\.0\.0\.1|localhost))(:\d+)?
    static RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"^(tauri://localhost|http://(tauri\.localhost|127\.0\.0\.1|localhost))(:\d+)?$")
            .expect("cors regex")
    });
    RE.is_match(origin)
}

pub async fn cors_mw(req: Request<Body>, next: Next) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|o| o.to_str().ok())
        .map(String::from);
    let preflight = req.method() == Method::OPTIONS
        && req.headers().contains_key("access-control-request-method");

    let mut res = if preflight {
        HttpResponse::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .unwrap()
    } else {
        next.run(req).await
    };

    if let Some(origin) = origin {
        if origin_allowed(&origin) {
            let headers = res.headers_mut();
            headers.insert(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_str(&origin).expect("origin header"),
            );
            headers.insert(header::VARY, HeaderValue::from_static("Origin"));
            if preflight {
                headers.insert(
                    header::ACCESS_CONTROL_ALLOW_METHODS,
                    HeaderValue::from_static("DELETE, GET, HEAD, OPTIONS, PATCH, POST, PUT"),
                );
                headers.insert(
                    header::ACCESS_CONTROL_ALLOW_HEADERS,
                    HeaderValue::from_static("*"),
                );
                headers.insert(
                    header::ACCESS_CONTROL_MAX_AGE,
                    HeaderValue::from_static("600"),
                );
            }
        }
    }
    res
}


// ---------------------------------------------------------------- router ---

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/theme", get(api_theme))
        .route("/api/theme/stream", get(api_theme_stream))
        .route("/api/projects", get(api_projects))
        .route("/api/projects/create", post(api_projects_create))
        .route("/api/projects/archive", post(api_projects_archive))
        .route("/api/projects/favorite", post(api_projects_favorite))
        .route(
            "/api/projects/{name}/sources/{kind}/create",
            post(api_project_source_create),
        )
        .route("/api/session", get(api_session))
        .route("/api/session/stream", get(api_session_stream))
        .route("/api/board", get(api_board))
        .route("/api/events", get(events::api_events))
        .route("/api/board/item", post(api_board_create))
        .route("/api/board/item/{id}", patch(api_board_patch))
        .route("/api/board/item/{id}/files", put(api_board_files_put))
        .route("/api/board/item/{id}/steps", put(api_board_steps_put))
        .route(
            "/api/board/item/{id}/delegate/preview",
            post(api_delegate_preview),
        )
        .route("/api/board/item/{id}/delegate", post(api_delegate_start))
        .route(
            "/api/board/item/{id}/delegate/cancel",
            post(api_delegate_cancel),
        )
        .route(
            "/api/board/item/{id}/delegate/reset",
            post(api_delegate_reset),
        )
        .route("/api/board/item/{id}/run", get(api_delegate_run))
        .route("/api/board/item/{id}/diff", get(api_delegate_diff))
        .route("/api/board/item/{id}/accept", post(api_board_accept))
        .route("/api/board/item/{id}/reject", post(api_board_reject))
        .route("/api/deps", get(api_deps))
        .route("/api/search", get(api_search))
        .route("/api/sources", get(api_sources))
        .route("/api/sync", post(api_sync))
        .route("/api/focus", post(api_focus))
        .route("/api/focus/targets", get(api_focus_targets))
        .route(
            "/api/settings",
            get(api_settings_get).patch(api_settings_patch),
        )
        .route(
            "/api/notify",
            post(api_notify_post).get(api_notify_get),
        )
        .route("/api/notify/read", post(api_notify_read))
        .route("/api/herdr", get(api_herdr))
        .route("/api/herdr/focus", post(api_herdr_focus))
        .route("/api/projects/weburl", post(api_projects_weburl))
        .route("/api/project/{name}/tasks", get(api_project_tasks))
        .route("/api/project/{name}/files", get(api_project_files))
        .route(
            "/api/project/{name}/tasks/run",
            post(api_project_task_run).get(api_project_task_status),
        )
        .route("/api/project/{name}/tasks/kill", post(api_project_task_kill))
        .route("/api/open", post(api_open))
        .route("/api/clipboard/image", post(api_clipboard_image))
        .route("/api/terminal/sessions", get(api_terminal_sessions))
        .route("/api/terminal/ws/{id}", get(api_terminal_ws))
        .route("/api/terminal/orch/restart", post(api_terminal_restart_orch))
        .route("/api/terminal/{id}/kill", post(api_terminal_kill))
        .route("/api/terminal/{id}/restart", post(api_terminal_restart_id))
        .route("/api/terminal/{id}/open", post(api_terminal_open))
        .route("/api/terminal/wezterm/dock", post(api_terminal_wezterm_dock))
        .route("/api/terminal/wezterm/hide", post(api_terminal_wezterm_hide))
        .route("/api/terminal/wezterm/show", post(api_terminal_wezterm_show))
        .route("/api/terminal/wezterm/focus", post(api_terminal_wezterm_focus))
        .route("/api/terminal/wezterm/state", get(api_terminal_wezterm_state))
        .route("/api/terminal/wezterm/tail", get(api_terminal_wezterm_tail))
        .route("/api/ui", get(api_ui))
        .route("/api/windows/minimize", post(api_windows_minimize))
        .route("/api/windows/restore", post(api_windows_restore))
        .route("/api/windows/focus", post(api_windows_focus))
        .route("/api/stt/start", post(api_stt_start))
        .route("/api/stt/stop", post(api_stt_stop))
        .route("/api/stt/status", get(api_stt_status))
        .route("/api/orch", post(api_orch))
        .route("/api/orch/status", get(api_orch_status))
        .route("/api/orch/history", get(api_orch_history))
        .route("/api/orch/approve", post(api_orch_approve))
        .route("/api/orch/cancel", post(api_orch_cancel))
        .route("/api/orch/restart", post(api_orch_restart))
        .route("/api/patrol", get(api_patrol))
        .fallback(static_handler)
        .layer(axum::middleware::from_fn(cors_mw))
        .with_state(state)
}


/// Bind the listener; Err when the port is busy.
pub async fn bind(port: u16) -> Result<tokio::net::TcpListener, String> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    tokio::net::TcpListener::bind(addr)
        .await
        .map(|l| {
            PORT_SELF.store(true, std::sync::atomic::Ordering::Relaxed);
            l
        })
        .map_err(|e| format!("bind {addr}: {e}"))
}

/// Serve until process exit.
pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> Result<(), String> {
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    log::info!("atlas server listening on http://{addr}");
    axum::serve(listener, router(state))
        .await
        .map_err(|e| format!("serve: {e}"))?;
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::support::{call, test_router};
    use axum::http::StatusCode;
    use serde_json::{json, Value};
    /// POST /api/windows/focus contract — only the label validation is
    /// exercised over the router: an unknown label is 404 and a bad body is
    /// 422, both BEFORE any hyprctl call (a valid label would really
    /// dispatch focuswindow — that is the user button's job, never a
    /// machine check; the matcher itself is covered by gui.rs fixtures).
    /// Plus the stt surface: status honest, stop with nothing → 409.
    #[tokio::test]
    async fn windows_focus_and_stt_contract() {
        let r = test_router();

        // bad bodies are rejected before anything runs
        let (st, out) = call(&r, "POST", "/api/windows/focus", Some(r#"{}"#)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        let (st, out) = call(&r, "POST", "/api/windows/focus", Some("not json")).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        // unknown labels keep the windows-endpoint 404 contract
        for label in ["settings", "nope"] {
            let body = format!(r#"{{"win":"{label}"}}"#);
            let (st, out) = call(&r, "POST", "/api/windows/focus", Some(&body)).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{label} → {out}");
            assert!(
                out["detail"].as_str().unwrap_or_default().contains(label),
                "{out}"
            );
        }

        // stt: honest status, and a stop with no recording is a 409 (never a
        // spawn — start is not exercised over the router: it would record
        // from the real mic; the recorder is faked in stt.rs unit tests)
        let (st, body) = call(&r, "GET", "/api/stt/status", None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"recording": false, "transcribing": false}));
        let (st, out) = call(&r, "POST", "/api/stt/stop", None).await;
        assert_eq!(st, StatusCode::CONFLICT, "{out}");
        assert_eq!(out["detail"], "запись не активна");
    }

    /// Focus/W contract: three targets, the removed ones rejected by name, the
    /// terminal plan answered from the session registry, and the terminal
    /// routes that replaced the old orchestrator pair.
    #[tokio::test]
    async fn focus_and_terminal_contract() {
        let r = test_router();
        let (st, body) = call(&r, "GET", "/api/focus/targets", None).await;
        assert_eq!(st, StatusCode::OK);
        let letters: Vec<&str> = body["targets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["letter"].as_str().unwrap())
            .collect();
        assert_eq!(letters, vec!["H", "T", "W"]);
        let targets: Vec<&str> = body["targets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["target"].as_str().unwrap())
            .collect();
        assert_eq!(targets, vec!["herdr_workspace", "herdr_tab", "terminal"]);
        assert_eq!(body["targets"][2]["label"], "терминал atlas");

        // the removed targets are rejected before anything else happens, and
        // the error names the valid ones (the offending target is echoed too)
        for gone in ["wezterm", "orchestrator"] {
            let body = format!(r#"{{"project":"atlas","target":"{gone}"}}"#);
            let (st, out) = call(&r, "POST", "/api/focus", Some(&body)).await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
            let detail = out["detail"].as_str().unwrap_or_default();
            assert!(detail.contains(gone), "{detail}");
            let supported = detail
                .split("supported: ")
                .nth(1)
                .unwrap_or_default()
                .trim();
            assert_eq!(
                supported, "herdr_workspace, herdr_tab, terminal",
                "the valid target list must not contain the removed one"
            );
        }

        // a project without a path cannot host a shell
        let (st, out) = call(
            &r,
            "POST",
            "/api/focus",
            Some(r#"{"project":"phone-access","target":"terminal","dry_run":true}"#),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");

        // sessions: project shells only — the orchestrator has no PTY here,
        // and listing never spawns anything
        let (st, body) = call(&r, "GET", "/api/terminal/sessions", None).await;
        assert_eq!(st, StatusCode::OK);
        let sessions = body["sessions"].as_array().unwrap();
        assert!(
            !sessions.iter().any(|s| s["id"] == "orch"),
            "orch is not a terminal session: {body}"
        );
        assert!(
            !sessions.iter().any(|s| s["id"] == "proj-lamplife"),
            "nothing may be spawned by a listing: {body}"
        );

        // dry run answers plan create + session id, without spawning
        let (st, body) = call(
            &r,
            "POST",
            "/api/focus",
            Some(r#"{"project":"lamplife","target":"terminal","dry_run":true}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["plan"], "create");
        assert_eq!(body["reused"], false);
        assert_eq!(body["session"], "proj-lamplife");
        assert_eq!(body["dry_run"], true);
        let (_, body) = call(&r, "GET", "/api/terminal/sessions", None).await;
        assert!(
            !body["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["id"] == "proj-lamplife"),
            "dry run must not spawn: {body}"
        );

        // targeting `orch` names reality (the Orchestrator panel owns it)
        let (st, out) = call(&r, "POST", "/api/terminal/orch/kill", Some("{}")).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        assert!(
            out["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("Оркестратор"),
            "{out}"
        );
        let (st, _) = call(&r, "GET", "/api/terminal/orch/status", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        // the legacy terminal alias still restarts the rpc orchestrator child
        let (st, out) = call(&r, "POST", "/api/terminal/orch/restart", Some("{}")).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["id"], "orch");
        let (st, out) = call(&r, "POST", "/api/terminal/proj-nope/restart", Some("{}")).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{out}");
        let (st, out) = call(&r, "POST", "/api/terminal/proj-nope/kill", Some("{}")).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{out}");
    }

    /// The rpc chat surface: status/history/approve/cancel/restart and the
    /// patrol status, over a fresh (never-spawned) orchestrator.
    #[tokio::test]
    async fn orch_and_patrol_endpoints() {
        let r = test_router();

        let (st, body) = call(&r, "GET", "/api/orch/status", None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["alive"], false);
        assert_eq!(body["pid"], Value::Null);
        assert_eq!(body["turn_active"], false);
        assert_eq!(body["pending"], json!([]));
        assert_eq!(body["model"], Value::Null);
        let cmd = body["cmd"].as_array().expect("cmd array");
        assert_eq!(cmd[1], "--mode", "cmd names the rpc child: {body}");
        assert_eq!(cmd[2], "rpc");
        assert!(
            !cmd.iter().any(|c| c == "always-ask" || c == "--approval-mode"),
            "the child must not be pinned to always-ask any more: {body}"
        );
        assert!(
            cmd.iter().any(|c| c == "--config")
                && cmd.iter().any(|c| c.as_str().unwrap_or_default().ends_with("orchestrator-approval.yml")),
            "the approval policy overlay must be in the argv: {body}"
        );
        assert!(
            cmd.iter().any(|c| c == "--append-system-prompt"),
            "the persona flag is preserved: {body}"
        );

        let (st, body) = call(&r, "GET", "/api/orch/history", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["events"], json!([]));

        // unknown/expired approval id → 404 with a clear detail
        let (st, out) = call(
            &r,
            "POST",
            "/api/orch/approve",
            Some(r#"{"id":"nope","allow":true}"#),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{out}");
        assert!(out["detail"].as_str().unwrap_or_default().contains("nope"));
        // malformed approval body → 422
        let (st, _) = call(&r, "POST", "/api/orch/approve", Some(r#"{"allow":true}"#)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

        // cancel with no live child is honest, never a panic
        let (st, out) = call(&r, "POST", "/api/orch/cancel", Some("{}")).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["abort_sent"], false);
        assert_eq!(out["cancelled"], false);

        // a prompt with no rpc child still answers 422 (bad body), and the
        // empty body is rejected before any turn is attempted
        let (st, _) = call(&r, "POST", "/api/orch", Some(r#"{"prompt":""}"#)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

        let (st, body) = call(&r, "GET", "/api/patrol", None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["enabled"], true);
        assert_eq!(body["interval_min"], 360);
        assert_eq!(body["last"], Value::Null);
    }
}
