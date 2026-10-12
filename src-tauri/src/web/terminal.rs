//! Terminal panel: PTY sessions (`proj-<имя>`, `herdr`) — list, lazy WS
//! bridge, kill, restart. The orchestrator is a panel, not a PTY (`orch`
//! ids → 422).

use super::windows::{self, DockPreset};
use super::{db_lock, join_result, ok_json, query_param, AppState, ApiError};
use crate::cfg;
use crate::gui;
use crate::term;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as AxumPath, State};
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{LazyLock, Mutex};


/// The id the removed orchestrator PTY used; the terminal panel now carries
/// project shells only (the orchestrator is the Оркестратор panel).
const TERMINAL_ORCH_ID: &str = "orch";

/// 422 naming reality: the orchestrator is not a terminal session any more.
fn orch_is_a_panel() -> ApiError {
    ApiError::unprocessable(
        "оркестратор живёт в панели Оркестратор; терминал — только оболочки проектов (proj-<имя>)",
    )
}

/// GET /api/terminal/sessions — the registry: `proj-*` once created. Listing
/// never spawns anything.
pub(crate) async fn api_terminal_sessions() -> Result<Response, ApiError> {
    let rows: Vec<Value> = term::list().iter().map(|s| s.json()).collect();
    Ok(ok_json(json!({"sessions": rows})))
}

/// Spawn spec for a session id: the project behind `proj-<slug>`, or the
/// herdr client session (`herdr`). The project's path is required (a shell
/// needs a cwd) → 422 without one.
async fn terminal_spec(id: String) -> Result<term::Spec, ApiError> {
    if id == term::HERDR_SESSION_ID {
        return Ok(term::herdr_spec());
    }
    if term::proj_slug(&id).is_none() {
        return Err(ApiError::not_found(format!("unknown session '{id}'")));
    }
    let want = id.clone();
    let found = tokio::task::spawn_blocking(move || {
        let conn = db_lock();
        let mut stmt = conn
            .prepare("SELECT name, path FROM projects")
            .expect("select projects");
        let rows: Vec<(String, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query projects")
            .filter_map(|r| r.ok())
            .collect();
        rows.into_iter()
            .find(|(name, _)| term::proj_id(name) == want)
    })
    .await
    .map_err(join_result)?;
    let Some((name, path)) = found else {
        return Err(ApiError::not_found(format!("unknown session '{id}'")));
    };
    let Some(path) = path.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) else {
        return Err(ApiError::unprocessable(format!(
            "у проекта '{name}' нет пути — сессию терминала открыть нельзя"
        )));
    };
    Ok(term::proj_spec(&name, std::path::Path::new(&path)))
}

/// GET /api/terminal/ws/{session_id} — websocket bridge to that session's PTY
/// (spawned lazily on this first connect).
pub(crate) async fn api_terminal_ws(
    AxumPath(id): AxumPath<String>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if id == TERMINAL_ORCH_ID {
        return Err(orch_is_a_panel());
    }
    let spec = terminal_spec(id).await?;
    Ok(ws.on_upgrade(move |socket| term::serve_ws(spec, socket)))
}

/// POST /api/terminal/{id}/kill — kill and forget a project session. Anything
/// targeting `orch` → 422 (the orchestrator is the Оркестратор panel).
pub(crate) async fn api_terminal_kill(AxumPath(id): AxumPath<String>) -> Result<Response, ApiError> {
    if id == TERMINAL_ORCH_ID {
        return Err(orch_is_a_panel());
    }
    match term::kill(&id) {
        Ok(()) => Ok(ok_json(json!({"ok": true, "id": id, "killed": true}))),
        Err(term::KillError::Unknown) => {
            Err(ApiError::not_found(format!("unknown session '{id}'")))
        }
    }
}

/// POST /api/terminal/{id}/restart — drop the session; the next attach spawns
/// a fresh child. Project sessions only (`orch` → 422).
pub(crate) async fn api_terminal_restart_id(AxumPath(id): AxumPath<String>) -> Result<Response, ApiError> {
    if id == TERMINAL_ORCH_ID {
        return Err(orch_is_a_panel());
    }
    if term::restart(&id) {
        Ok(ok_json(json!({"ok": true, "id": id})))
    } else {
        Err(ApiError::not_found(format!("unknown session '{id}'")))
    }
}

/// POST /api/terminal/orch/restart — legacy alias: restarts the rpc
/// orchestrator child, exactly like `POST /api/orch/restart`.
pub(crate) async fn api_terminal_restart_orch(State(state): State<AppState>) -> Result<Response, ApiError> {
    let had = state.orch.restart().await;
    Ok(ok_json(json!({"ok": true, "id": TERMINAL_ORCH_ID, "had_process": had})))
}

/// Env var names that leak a herdr-nested context into wezterm children.
/// Today's incident: a GUI launched from a herdr pane passes HERDR_PANE_ID
/// (et al.) down; the herdr client spawned inside the wezterm window then
/// self-terminates — «nested herdr is disabled by default» — and the fresh
/// window dies before its cli socket ever answers. Pure env hygiene, applied
/// to both wezterm children (window launch and tab spawn): plain shells are
/// unaffected.
fn herdr_nested_env_names() -> &'static [&'static str] {
    &[
        "HERDR_ENV",
        "HERDR_PANE_ID",
        "HERDR_TAB_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_SOCKET_PATH",
        "HERDR_BIN_PATH",
        "TERM_PROGRAM",
    ]
}

/// POST /api/terminal/{id}/open — open the session in the atlas-OWNED wezterm
/// window (панель — пульт, FRAGILE.md §1 этап 1): atlas drives its own
/// dedicated instance, never the user's personal wezterm. Discovery probes
/// the per-instance cli sockets for the `atlas` workspace marker
/// (`term::atlas_wezterm_instance`); a live instance gets the session as a
/// new tab in its window (`wezterm cli spawn --window-id …`), otherwise this
/// open LAUNCHES the instance (`wezterm start --always-new-process
/// --workspace atlas`) and the session is its initial window. The cwd/argv
/// are exactly the ones the embedded PTY spawner would use (`terminal_spec`:
/// `$SHELL -l` in the project path, bare `herdr` in home). The embedded xterm
/// bridge is untouched and stays the default viewer. A wezterm binary that
/// cannot run → 400 naming config `binaries.term`.
pub(crate) async fn api_terminal_open(AxumPath(id): AxumPath<String>) -> Result<Response, ApiError> {
    if id == TERMINAL_ORCH_ID {
        return Err(orch_is_a_panel());
    }
    let spec = terminal_spec(id.clone()).await?;
    let bin = cfg::TERM_BIN.clone();
    let _seat = term::OPEN_LOCK.lock().await;
    let inst = {
        let probe_bin = bin.clone();
        tokio::task::spawn_blocking(move || term::atlas_wezterm_instance(&probe_bin))
            .await
            .map_err(join_result)?
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    ApiError::bad(term::wezterm_missing_error(&bin))
                } else {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("wezterm cli list: {e}"),
                    )
                }
            })?
    };
    let (argv, mode) = match &inst {
        Some(i) => (
            term::wezterm_cli_spawn_argv(&bin, i.window_id, &spec.cwd, &spec.argv),
            "tab",
        ),
        None => (term::wezterm_start_argv(&bin, &spec.cwd, &spec.argv), "window"),
    };
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for name in herdr_nested_env_names() {
        cmd.env_remove(name);
    }
    if let Some(i) = &inst {
        cmd.env(term::WEZTERM_SOCK_ENV, &i.sock);
    }
    // Detached child: the window/tab lives its own life, the server does not
    // wait. Dropping the tokio Child hands reaping to the runtime's orphan
    // reaper — no zombie, no kill-on-drop.
    let child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ApiError::bad(term::wezterm_missing_error(&bin))
        } else {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("wezterm spawn: {e}"),
            )
        }
    })?;
    if mode == "window" {
        if let Some(pid) = child.id() {
            wait_atlas_instance_up(&bin, pid).await;
        }
    }
    log::info!("terminal: opened '{id}' in wezterm ({mode}, pid {:?})", child.id());
    Ok(ok_json(json!({"ok": true, "id": id, "mode": mode})))
}

/// After launching the atlas instance, wait until its cli socket answers so
/// the NEXT open becomes a tab in it instead of racing a second launch.
/// Best effort: the window itself is already up — on timeout the next open
/// simply re-discovers (or relaunches) by the same workspace marker.
async fn wait_atlas_instance_up(bin: &str, pid: u32) {
    let wait_bin = bin.to_owned();
    let up = tokio::task::spawn_blocking(move || {
        let Some(sock) = term::wezterm_gui_socket(pid) else { return false };
        for _ in 0..40 {
            if matches!(term::socket_is_atlas(&wait_bin, &sock), Ok(true)) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        false
    })
    .await
    .unwrap_or(false);
    if up {
        log::info!("terminal: atlas wezterm instance is up (pid {pid})");
    } else {
        log::warn!("terminal: atlas wezterm socket silent after 8 s (pid {pid}) — the next open may start a fresh instance");
    }
}

// ------------------------------------------- wezterm window control (dock) ---

/// Special workspace the dock hides the atlas-wezterm window into
/// (WEZTERM-EMBED §B3: Hyprland has no minimize dispatcher — the window is
/// moved away silently and stays alive, sessions intact).
const DOCK_SPECIAL_WS: &str = "atlas-dock";

/// Window selector of the atlas-wezterm window for `hyprctl dispatch`
/// arguments — bare class regex, no leading spaces after the preceding
/// parameter's comma (Hyprland#7999).
fn wz_sel() -> String {
    format!("class:^({})$", term::ATLAS_CLASS)
}

/// The rect the dock applied last (`docked_rect` of the state endpoint).
/// Process-local, best-effort: it answers "what did atlas dock last", not a
/// source of truth — the live truth is always `hyprctl clients -j`.
static DOCKED: LazyLock<Mutex<Option<(i32, i32, i32, i32)>>> =
    LazyLock::new(|| Mutex::new(None));

fn docked_get() -> Option<(i32, i32, i32, i32)> {
    DOCKED.lock().map(|d| *d).unwrap_or(None)
}

fn docked_set(r: (i32, i32, i32, i32)) {
    if let Ok(mut d) = DOCKED.lock() {
        *d = Some(r);
    }
}

/// The atlas-wezterm client in a `hyprctl clients -j` payload (class exact).
fn wz_client(clients: &Value) -> Option<&Value> {
    clients
        .as_array()?
        .iter()
        .find(|c| c["class"] == term::ATLAS_CLASS)
}

/// The atlas window the dock anchors to: the terminal panel toplevel when it
/// is open (`atlas — Терминал` — the panel driving the dock), else any atlas
/// toplevel (the panel may live as a host tab; then the host represents
/// atlas). Same matcher as windows/focus (`gui::atlas_clients`).
fn atlas_host<'a>(clients: &'a Value) -> Option<&'a Value> {
    let arr = clients.as_array()?;
    let title = gui::def_of("terminal").map(|d| d.title)?;
    arr.iter()
        .find(|c| c["class"] == "atlas" && c["title"] == title.as_str())
        .or_else(|| arr.iter().find(|c| c["class"] == "atlas"))
}

/// `([x, y], [w, h])` of a client entry; `None` on a malformed payload.
fn rect_of(c: &Value) -> Option<([i32; 2], [i32; 2])> {
    let at = c["at"].as_array()?;
    let size = c["size"].as_array()?;
    let px = |a: &Vec<Value>, i: usize| a.get(i).and_then(Value::as_i64).map(|v| v as i32);
    Some((
        [px(at, 0)?, px(at, 1)?],
        [px(size, 0)?, px(size, 1)?],
    ))
}

/// The monitor carrying the atlas host: by the client's own `monitor` id,
/// falling back to the monitor whose bounds hold the host center (a window
/// spanning monitors belongs to the monitor of its center).
fn monitor_of_host<'a>(monitors: &'a Value, host: &Value) -> Option<&'a Value> {
    let arr = monitors.as_array()?;
    if let Some(id) = host["monitor"].as_i64() {
        if let Some(m) = arr.iter().find(|m| m["id"].as_i64() == Some(id)) {
            return Some(m);
        }
    }
    let (at, size) = rect_of(host)?;
    let (cx, cy) = (at[0] + size[0] / 2, at[1] + size[1] / 2);
    arr.iter().find(|m| {
        let (Some(mx), Some(my)) = (m["x"].as_i64(), m["y"].as_i64()) else {
            return false;
        };
        let (Some(mw), Some(mh)) = (m["width"].as_i64(), m["height"].as_i64()) else {
            return false;
        };
        let (mx, my, mw, mh) = (mx as i32, my as i32, mw as i32, mh as i32);
        cx >= mx && cx < mx + mw && cy >= my && cy < my + mh
    })
}

/// monitor `[x, y]` / `[w, h]` from a `hyprctl monitors -j` entry.
fn mon_rect(m: &Value) -> ([i32; 2], [i32; 2]) {
    (
        [
            m["x"].as_i64().unwrap_or(0) as i32,
            m["y"].as_i64().unwrap_or(0) as i32,
        ],
        [
            m["width"].as_i64().unwrap_or(0) as i32,
            m["height"].as_i64().unwrap_or(0) as i32,
        ],
    )
}

/// workspace name of a client entry (e.g. `"3"`, `"atlas"`,
/// `"special:atlas-dock"`); empty string when the payload is odd.
fn ws_of(c: &Value) -> String {
    c["workspace"]["name"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// One dispatch through `hyprctl`; Err goes to the caller as an honest
/// `ok:false` detail (hyprctl missing/failed — never a silent no-op).
async fn wz_dispatch(args: [&str; 3]) -> Result<String, String> {
    windows::hyprctl(&args).await
}

/// POST /api/terminal/wezterm/dock `{"preset":"right-half"|"right-quarter"|"overlay"}`
/// — dock the atlas-wezterm window onto a computed rectangle (WEZTERM-EMBED
/// §B2, stage A). Requires a live atlas instance (409 otherwise — the window
/// is created only by «↗», the open path stays the sole spawner). Steps:
/// ensure floating (`togglefloating` only when the client reports tiled —
/// windowrules stay owner-owned, `hyprctl keyword` is never touched), compute
/// the rect from the atlas host window + its monitor (pure
/// `windows::dock_rect`), then `resizewindowpixel` → `movewindowpixel`
/// (resize before move), and re-read `hyprctl clients -j` to confirm the
/// geometry within ±8 px — the answer is honest about a flapped dock.
pub(crate) async fn api_terminal_wezterm_dock(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let preset = match v.get("preset").and_then(|p| p.as_str()) {
        Some(s) => DockPreset::parse(s).ok_or_else(|| {
            ApiError::unprocessable(format!(
                "unknown preset '{s}' — right-half | right-quarter | overlay"
            ))
        })?,
        None => DockPreset::RightHalf,
    };
    // the dock drives a window; no window without the atlas-owned instance
    let bin = cfg::TERM_BIN.clone();
    let probe_bin = bin.clone();
    let has_instance = tokio::task::spawn_blocking(move || term::atlas_wezterm_instance(&probe_bin))
        .await
        .map_err(join_result)?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ApiError::bad(term::wezterm_missing_error(&bin))
            } else {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("wezterm cli list: {e}"),
                )
            }
        })?
        .is_some();
    if !has_instance {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "нет окна atlas-wezterm — сначала откройте сессию (↗)",
        ));
    }
    let clients = match windows::hyprctl_clients().await {
        Ok(c) => c,
        Err(e) => return Ok(ok_json(json!({ "ok": false, "detail": e }))),
    };
    let Some(wz) = wz_client(&clients) else {
        return Ok(ok_json(json!(
            { "ok": false, "detail": "нет окна atlas-wezterm — сначала откройте сессию (↗)" }
        )));
    };
    let mut floating = wz["floating"].as_bool().unwrap_or(false);
    if !floating {
        if let Err(e) = wz_dispatch(["dispatch", "togglefloating", &wz_sel()]).await {
            return Ok(ok_json(json!({ "ok": false, "detail": e })));
        }
        floating = true;
    }
    let Some(host) = atlas_host(&clients) else {
        return Ok(ok_json(json!(
            { "ok": false, "detail": "окно atlas не найдено — дока не к чему привязать" }
        )));
    };
    let Some((atlas_at, atlas_size)) = rect_of(host) else {
        return Ok(ok_json(json!(
            { "ok": false, "detail": "у окна atlas нет геометрии в hyprctl clients" }
        )));
    };
    let monitors = match windows::hyprctl_monitors().await {
        Ok(m) => m,
        Err(e) => return Ok(ok_json(json!({ "ok": false, "detail": e }))),
    };
    let Some(m) = monitor_of_host(&monitors, host) else {
        return Ok(ok_json(json!(
            { "ok": false, "detail": "монитор окна atlas не найден в hyprctl monitors" }
        )));
    };
    let (mon_at, mon_size) = mon_rect(m);
    let (x, y, w, h) = windows::dock_rect(preset, atlas_at, atlas_size, mon_at, mon_size);
    // resize BEFORE move; the selector rides the same argument, no leading
    // space after the comma (Hyprland#7999)
    let resize_arg = format!("exact {w} {h},{}", wz_sel());
    if let Err(e) = wz_dispatch(["dispatch", "resizewindowpixel", &resize_arg]).await {
        return Ok(ok_json(json!({ "ok": false, "detail": e })));
    }
    let move_arg = format!("exact {x} {y},{}", wz_sel());
    if let Err(e) = wz_dispatch(["dispatch", "movewindowpixel", &move_arg]).await {
        return Ok(ok_json(json!({ "ok": false, "detail": e })));
    }
    // anti-flap: the dispatches' "ok" says nothing about the final geometry
    let after = match windows::hyprctl_clients().await {
        Ok(c) => c,
        Err(e) => return Ok(ok_json(json!({ "ok": false, "detail": e }))),
    };
    let confirmed = wz_client(&after)
        .and_then(rect_of)
        .is_some_and(|(at, size)| {
            [at[0] - x, at[1] - y, size[0] - w, size[1] - h]
                .iter()
                .all(|d| d.abs() <= 8)
        });
    let rect = json!({ "x": x, "y": y, "w": w, "h": h });
    if !confirmed {
        return Ok(ok_json(json!(
            { "ok": false, "rect": rect, "detail": "геометрия не подтвердилась (±8 px) — повторите док" }
        )));
    }
    docked_set((x, y, w, h));
    let workspace = wz_client(&after).map(ws_of).unwrap_or_default();
    Ok(ok_json(json!({
        "ok": true,
        "preset": preset.label(),
        "rect": rect,
        "floating": floating,
        "workspace": workspace,
    })))
}

/// POST /api/terminal/wezterm/hide — move the atlas-wezterm window into the
/// `special:atlas-dock` workspace, silently (§B3: the window and its sessions
/// stay alive; no minimize dispatcher exists). Confirmed by re-reading the
/// client list.
pub(crate) async fn api_terminal_wezterm_hide() -> Result<Response, ApiError> {
    let arg = format!("special:{DOCK_SPECIAL_WS},{}", wz_sel());
    if let Err(e) = wz_dispatch(["dispatch", "movetoworkspacesilent", &arg]).await {
        return Ok(ok_json(json!({ "ok": false, "hidden": false, "detail": e })));
    }
    let hidden = match windows::hyprctl_clients().await {
        Ok(c) => wz_client(&c).is_some_and(|w| ws_of(w) == format!("special:{DOCK_SPECIAL_WS}")),
        Err(e) => return Ok(ok_json(json!({ "ok": false, "hidden": false, "detail": e }))),
    };
    if hidden {
        Ok(ok_json(json!({ "ok": true, "hidden": true })))
    } else {
        Ok(ok_json(json!(
            { "ok": false, "hidden": false, "detail": "окно не подтвердилось в special:atlas-dock — повторите" }
        )))
    }
}

/// POST /api/terminal/wezterm/show — move the atlas-wezterm window back to
/// the workspace of the atlas host window (the terminal panel's toplevel, or
/// any atlas toplevel) and focus it (the windows/focus precedent).
pub(crate) async fn api_terminal_wezterm_show() -> Result<Response, ApiError> {
    let clients = match windows::hyprctl_clients().await {
        Ok(c) => c,
        Err(e) => return Ok(ok_json(json!({ "ok": false, "hidden": true, "detail": e }))),
    };
    let Some(target_ws) = atlas_host(&clients).map(ws_of).filter(|w| !w.is_empty()) else {
        return Ok(ok_json(json!(
            { "ok": false, "hidden": true, "detail": "окно atlas не найдено — некуда показывать" }
        )));
    };
    let arg = format!("{target_ws},{}", wz_sel());
    if let Err(e) = wz_dispatch(["dispatch", "movetoworkspacesilent", &arg]).await {
        return Ok(ok_json(json!({ "ok": false, "hidden": true, "detail": e })));
    }
    if let Err(e) = wz_dispatch(["dispatch", "focuswindow", &wz_sel()]).await {
        return Ok(ok_json(json!({ "ok": false, "hidden": true, "detail": e })));
    }
    let shown = match windows::hyprctl_clients().await {
        Ok(c) => wz_client(&c).is_some_and(|w| ws_of(w) == target_ws),
        Err(e) => return Ok(ok_json(json!({ "ok": false, "hidden": true, "detail": e }))),
    };
    if shown {
        Ok(ok_json(json!({ "ok": true, "hidden": false, "workspace": target_ws })))
    } else {
        Ok(ok_json(json!(
            { "ok": false, "hidden": true, "detail": format!("окно не подтвердилось на воркспейсе «{target_ws}» — повторите") }
        )))
    }
}

/// POST /api/terminal/wezterm/focus — `focuswindow` + `alterzorder top`
/// (§B4: floating already renders above tiles, the z-order call is the
/// belt to the suspenders). Verified by the client's `focused` flag; a
/// failed z-order call is logged but does not sink an otherwise confirmed
/// focus.
pub(crate) async fn api_terminal_wezterm_focus() -> Result<Response, ApiError> {
    let clients = match windows::hyprctl_clients().await {
        Ok(c) => c,
        Err(e) => return Ok(ok_json(json!({ "ok": false, "detail": e }))),
    };
    if wz_client(&clients).is_none() {
        return Ok(ok_json(json!(
            { "ok": false, "detail": "нет окна atlas-wezterm — фокусировать нечего" }
        )));
    }
    if let Err(e) = wz_dispatch(["dispatch", "focuswindow", &wz_sel()]).await {
        return Ok(ok_json(json!({ "ok": false, "detail": e })));
    }
    let zorder = wz_dispatch(["dispatch", "alterzorder", &format!("top,{}", wz_sel())]).await;
    if let Err(e) = zorder {
        log::warn!("terminal: wezterm alterzorder: {e}");
    }
    let focused = match windows::hyprctl_clients().await {
        Ok(c) => wz_client(&c).is_some_and(|w| w["focused"].as_bool().unwrap_or(false)),
        Err(e) => return Ok(ok_json(json!({ "ok": false, "detail": e }))),
    };
    if focused {
        Ok(ok_json(json!({ "ok": true, "focused": true })))
    } else {
        Ok(ok_json(json!(
            { "ok": false, "detail": "фокус не подтвердился в hyprctl clients — повторите" }
        )))
    }
}

/// GET /api/terminal/wezterm/state — read-only: what `hyprctl clients -j`
/// says about the atlas-wezterm window right now, plus the rect the dock
/// applied last (`docked_rect`, process-local best-effort).
pub(crate) async fn api_terminal_wezterm_state() -> Result<Response, ApiError> {
    let clients = windows::hyprctl_clients().await.map_err(|e| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e)
    })?;
    let Some(wz) = wz_client(&clients) else {
        return Ok(ok_json(json!({ "exists": false })));
    };
    let (at, size) = rect_of(wz).unwrap_or(([0, 0], [0, 0]));
    let workspace = ws_of(wz);
    Ok(ok_json(json!({
        "exists": true,
        "docked_rect": docked_get().map(|(x, y, w, h)| json!({ "x": x, "y": y, "w": w, "h": h })),
        "hidden": workspace.starts_with("special:"),
        "floating": wz["floating"].as_bool().unwrap_or(false),
        "at": { "x": at[0], "y": at[1] },
        "size": { "w": size[0], "h": size[1] },
        "workspace": workspace,
    })))
}

// ------------------------------------------------- wezterm tail (get-text) ---

/// `lines` query of the tail: default 40, an integer within 5..=300 — outside
/// that is 422 (the bound keeps a stray client from asking for a gigabyte of
/// scrollback in one snapshot).
const TAIL_LINES_DEFAULT: usize = 40;
const TAIL_LINES_MIN: usize = 5;
const TAIL_LINES_MAX: usize = 300;

fn tail_lines_param(raw: Option<&str>) -> Result<usize, ApiError> {
    match raw {
        None => Ok(TAIL_LINES_DEFAULT),
        Some(s) => {
            let n: usize = s.trim().parse().map_err(|_| {
                ApiError::unprocessable(format!("lines must be an integer, got '{s}'"))
            })?;
            if (TAIL_LINES_MIN..=TAIL_LINES_MAX).contains(&n) {
                Ok(n)
            } else {
                Err(ApiError::unprocessable(format!(
                    "lines must be within {TAIL_LINES_MIN}..={TAIL_LINES_MAX}, got {n}"
                )))
            }
        }
    }
}

/// One `wezterm cli` call against the instance socket (`WEZTERM_SOCK_ENV`,
/// `--no-auto-start` so a stale socket can never autostart a mux server — the
/// trap from WEZTERM-EMBED §4.5): the windows::hyprctl shape — hard 5 s
/// timeout (a wedged cli must not hang a panel poll), Ok = stdout, Err =
/// honest detail (spawn failure / non-zero exit / timeout).
async fn wz_cli(bin: &str, sock: &Path, args: &[&str]) -> Result<String, String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(bin)
            .args(args)
            .env(term::WEZTERM_SOCK_ENV, sock)
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| format!("wezterm cli {} — нет ответа за 5 с", args.join(" ")))?
    .map_err(|e| format!("wezterm cli: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let err = if stderr.is_empty() {
            if out.stdout.is_empty() {
                "нет вывода".to_string()
            } else {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
        } else {
            stderr
        };
        return Err(format!("wezterm cli {} ({}): {err}", args.join(" "), out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// GET /api/terminal/wezterm/tail?lines=40 — read-only «хвост вывода» of the
/// atlas-wezterm pane (WEZTERM-EMBED §A4/этап C): the pane's last `lines`
/// lines as a `get-text` snapshot, rendered by the panel's «Хвост» block.
/// The pane is chosen deterministically (`term::pane_of_min_window`): among
/// the instance's atlas-workspace panes the min `window_id` wins — the same
/// window the dock resolves via `atlas_wezterm_instance` — and inside it the
/// min `pane_id`. No live instance → 404. The snapshot is terminal content
/// returned to the panel ONLY: nothing here is injected into any session,
/// and the input stays in the real wezterm window.
pub(crate) async fn api_terminal_wezterm_tail(uri: Uri) -> Result<Response, ApiError> {
    let lines = tail_lines_param(query_param(&uri, "lines").as_deref())?;
    let bin = cfg::TERM_BIN.clone();
    let probe_bin = bin.clone();
    let inst = tokio::task::spawn_blocking(move || term::atlas_wezterm_instance(&probe_bin))
        .await
        .map_err(join_result)?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ApiError::bad(term::wezterm_missing_error(&bin))
            } else {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("wezterm cli list: {e}"),
                )
            }
        })?;
    let Some(inst) = inst else {
        return Err(ApiError::not_found(
            "нет окна atlas-wezterm — сначала откройте сессию (↗)",
        ));
    };
    let sock = inst.sock.clone();
    let cli_err = |e: String| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e);
    let list = wz_cli(
        &bin,
        &sock,
        &["cli", "--no-auto-start", "list", "--format", "json"],
    )
    .await
    .map_err(cli_err)?;
    let list_v: Value = serde_json::from_str(&list)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("wezterm cli list: {e}")))?;
    let Some(pane) = term::pane_of_min_window(&list_v) else {
        return Err(ApiError::not_found(
            "atlas-wezterm: панель не найдена в списке — окно только что закрылось?",
        ));
    };
    let pane_arg = pane.to_string();
    let start_arg = format!("-{lines}");
    let text = wz_cli(
        &bin,
        &sock,
        &[
            "cli",
            "--no-auto-start",
            "get-text",
            "--pane-id",
            &pane_arg,
            "--start-line",
            &start_arg,
        ],
    )
    .await
    .map_err(cli_err)?;
    let n = text.lines().count();
    Ok(ok_json(json!({ "ok": true, "pane": pane, "lines": n, "text": text })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_lines_defaults_and_bounds() {
        assert!(matches!(tail_lines_param(None), Ok(40)));
        for ok in ["5", "40", "300", " 7 "] {
            assert!(matches!(tail_lines_param(Some(ok)), Ok(_)), "{ok} must be ok");
        }
    }

    #[test]
    fn tail_lines_outside_range_is_unprocessable() {
        for bad in ["4", "301", "0", "-10", "abc", ""] {
            let status = match tail_lines_param(Some(bad)) {
                Err(e) => e.status,
                Ok(n) => panic!("{bad} must be rejected, got {n}"),
            };
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        }
    }
}

