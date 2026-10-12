//! Windows: Hyprland window surgery for panels — minimize into host tab,
//! restore, focus — with verified (anti-flap) application steps; plus the
//! tab persistence helpers the plan steps share.

use super::{db_lock, join_result, ok_json, ApiError};
use crate::gui;
use crate::settings;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

fn set_tabs_blocking(tabs: Vec<String>) -> Result<(), ApiError> {
    let conn = db_lock();
    settings::set_tabs(&conn, &tabs)
        .map(|_| ())
        .map_err(|e| ApiError::bad(format!("{e}")))
}

async fn store_tabs(tabs: Vec<String>) -> Result<(), ApiError> {
    tokio::task::spawn_blocking(move || set_tabs_blocking(tabs))
        .await
        .map_err(join_result)?
}


fn win_unknown(win: &str) -> ApiError {
    ApiError::not_found(format!(
        "unknown window '{win}'; panels: {}",
        settings::PANELS.join(", ")
    ))
}

fn no_gui_body(tabs_now: Value) -> Value {
    json!({
        "ok": false,
        "plan": "none",
        "tabs": tabs_now,
        "gui": false,
        "detail": "GUI не запущен (--serve-only): окна недоступны",
    })
}

pub(crate) async fn api_windows_minimize(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let Some(win) = v.get("win").and_then(|w| w.as_str()) else {
        return Err(ApiError::unprocessable("win must be a string"));
    };
    if !settings::PANELS.contains(&win) {
        return Err(win_unknown(win));
    }
    let dry = v.get("dry_run").and_then(|d| d.as_bool()).unwrap_or(false);
    let tabs_now = tokio::task::spawn_blocking(|| settings::tabs(&db_lock()))
        .await
        .map_err(join_result)?;
    let gui_on = gui::app().is_some();
    if win == settings::HOST {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "ok": false,
                "plan": "none",
                "tabs": tabs_now,
                "host_present": gui::host_present(),
                "gui": gui_on,
                "detail": "главное окно нельзя свернуть в себя",
            })),
        )
            .into_response());
    }
    let already = tabs_now.iter().any(|t| t == win);
    let mut tabs_new = tabs_now.clone();
    if !already {
        tabs_new.push(win.to_string());
    }
    let host_present = gui::host_present();

    if dry {
        let mut detail = if already {
            format!("dry-run: '{win}' уже во вкладках, состояние не изменится")
        } else {
            format!(
                "dry-run: '{win}' будет свёрнут во вкладку (вкладок станет {})",
                tabs_new.len()
            )
        };
        if !gui_on {
            detail.push_str("; GUI не запущен (--serve-only)");
        }
        return Ok(ok_json(json!({
            "ok": true,
            "plan": "close+tab",
            "tabs": tabs_new,
            "host_present": host_present,
            "gui": gui_on,
            "detail": detail,
        })));
    }
    if !gui_on {
        return Ok(ok_json(no_gui_body(json!(tabs_now))));
    }
    // Apply: record the tab → close the toplevel → make sure the host is open.
    // Anti-flap: the Tauri call's Ok says nothing about when (or whether) the
    // WM actually unmaps the toplevel — every answer below is verified
    // against the live window list before it is sent.
    store_tabs(tabs_new.clone()).await?;
    let mut closed = false;
    if gui::live().iter().any(|w| w == win) {
        if let Err(e) = gui::close(win) {
            store_tabs(tabs_now.clone()).await?;
            return Ok(ok_json(json!({
                "ok": false, "plan": "none", "tabs": tabs_now,
                "host_present": host_present, "gui": true, "detail": e,
            })));
        }
        closed = true;
    }
    if closed && !gui::wait_window(win, false).await {
        let live_now = gui::live().iter().any(|w| w == win);
        return Ok(ok_json(json!({
            "ok": false, "plan": "close+tab", "tabs": tabs_new,
            "appeared": live_now, "host_present": gui::host_present(), "gui": true,
            "detail": format!("окно '{win}' не исчезло из живого списка за 2 с — повторите запрос"),
        })));
    }
    let mut host_now = gui::host_present();
    if !host_now {
        if let Err(e) = gui::open(settings::HOST) {
            store_tabs(tabs_now.clone()).await?;
            return Ok(ok_json(json!({
                "ok": false, "plan": "none", "tabs": tabs_now,
                "host_present": host_now, "gui": true, "detail": e,
            })));
        }
        if !gui::wait_window(settings::HOST, true).await {
            return Ok(ok_json(json!({
                "ok": false, "plan": "close+tab", "tabs": tabs_new,
                "appeared": false, "host_present": gui::host_present(), "gui": true,
                "detail": "главное окно не подтвердилось в живом списке за 2 с — повторите запрос",
            })));
        }
        host_now = gui::host_present();
    }
    let detail = if already {
        format!(
            "'{win}' уже был свёрнут во вкладку (вкладок: {})",
            tabs_new.len()
        )
    } else {
        format!(
            "'{win}' свёрнут во вкладку (вкладок: {}); главное окно открыто",
            tabs_new.len()
        )
    };
    Ok(ok_json(json!({
        "ok": true,
        "plan": "close+tab",
        "tabs": tabs_new,
        "appeared": false,
        "host_present": host_now,
        "gui": true,
        "detail": detail,
    })))
}

pub(crate) async fn api_windows_restore(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let Some(win) = v.get("win").and_then(|w| w.as_str()) else {
        return Err(ApiError::unprocessable("win must be a string"));
    };
    if !settings::PANELS.contains(&win) {
        return Err(win_unknown(win));
    }
    let dry = v.get("dry_run").and_then(|d| d.as_bool()).unwrap_or(false);
    let tabs_now = tokio::task::spawn_blocking(|| settings::tabs(&db_lock()))
        .await
        .map_err(join_result)?;
    let gui_on = gui::app().is_some();
    let in_tabs = tabs_now.iter().any(|t| t == win);
    let open = gui::live().iter().any(|w| w == win);
    let mut tabs_new = tabs_now.clone();
    tabs_new.retain(|t| t != win);

    if dry {
        let plan = if open && !in_tabs { "none" } else { "create" };
        let mut detail = if plan == "none" {
            format!("dry-run: окно '{win}' уже открыто, делать нечего")
        } else {
            format!("dry-run: окно '{win}' будет открыто (index.html?win={win})")
        };
        if !gui_on {
            detail.push_str("; GUI не запущен (--serve-only)");
        }
        return Ok(ok_json(json!({
            "ok": true, "plan": plan, "tabs": tabs_new, "gui": gui_on, "detail": detail,
        })));
    }
    if !gui_on {
        return Ok(ok_json(no_gui_body(json!(tabs_now))));
    }
    if open && !in_tabs {
        return Ok(ok_json(json!({
            "ok": true, "plan": "none", "tabs": tabs_now, "gui": true,
            "appeared": true,
            "detail": format!("окно '{win}' уже открыто"),
        })));
    }
    if in_tabs {
        store_tabs(tabs_new.clone()).await?;
    }
    // Anti-flap: gui::open's Ok is the Tauri object, not a mapped toplevel —
    // the answer waits until the window is really in the live list (≤2 s) or
    // reports the timeout honestly (the frontend retries once).
    match gui::open(win) {
        Ok(created) => {
            if !gui::wait_window(win, true).await {
                let live_now = gui::live().iter().any(|w| w == win);
                return Ok(ok_json(json!({
                    "ok": false, "plan": "none", "tabs": tabs_new, "gui": true,
                    "appeared": live_now,
                    "detail": format!(
                        "окно '{win}' не подтвердилось в живом списке за 2 с — повторите запрос"
                    ),
                })));
            }
            Ok(ok_json(json!({
                "ok": true,
                "plan": "create",
                "tabs": tabs_new,
                "gui": true,
                "appeared": true,
                "detail": if created {
                    format!("окно '{win}' открыто")
                } else {
                    format!("окно '{win}' уже было открыто (переведено в фокус)")
                },
            })))
        }
        Err(e) => {
            if in_tabs {
                store_tabs(tabs_now.clone()).await?;
            }
            Ok(ok_json(json!({
                "ok": false, "plan": "none", "tabs": tabs_now, "gui": true, "detail": e,
            })))
        }
    }
}

// ------------------------------------------------- windows / focus (hyprctl) ---

/// One hyprctl call — a read (`clients -j`) or a dispatch — the single
/// process-running pattern of this module, with a hard 5 s timeout (a wedged
/// compositor must not hang a panel request). Ok = trimmed stdout; Err =
/// spawn failure / non-zero exit / timeout with the honest detail.
pub(crate) async fn hyprctl(args: &[&str]) -> Result<String, String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new("hyprctl").args(args).output(),
    )
    .await
    .map_err(|_| format!("hyprctl {} — нет ответа за 5 с", args.join(" ")))?
    .map_err(|e| format!("hyprctl: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let err = if stderr.is_empty() {
            if stdout.is_empty() {
                "нет вывода (проверьте HYPRLAND_INSTANCE_SIGNATURE)"
            } else {
                stdout.as_str()
            }
        } else {
            stderr.as_str()
        };
        return Err(format!("hyprctl {} ({}): {err}", args.join(" "), out.status));
    }
    Ok(stdout)
}

/// `hyprctl clients -j` — the read-only live client list. Err = hyprctl
/// missing/failed; the handler answers `ok:false` with the detail and never
/// dispatches.
pub(crate) async fn hyprctl_clients() -> Result<Value, String> {
    let out = hyprctl(&["clients", "-j"]).await?;
    serde_json::from_str(&out).map_err(|e| format!("hyprctl clients -j: {e}"))
}

/// `hyprctl monitors -j` — the read-only monitor list: global `x`/`y`
/// origins plus `width`/`height`, the other half of the dock rectangle.
pub(crate) async fn hyprctl_monitors() -> Result<Value, String> {
    let out = hyprctl(&["monitors", "-j"]).await?;
    serde_json::from_str(&out).map_err(|e| format!("hyprctl monitors -j: {e}"))
}

/// POST /api/windows/focus `{"win":"<label>"}` — raise the EXISTING toplevel
/// of a panel («Открыть оркестратор» и т.п.). This is an **app feature fired
/// by a user's click** on a frontend button: the `hyprctl dispatch
/// focuswindow` here runs only as a consequence of an explicit API call made
/// by that button — agents must not call this endpoint themselves (см.
/// «Правила проверки»; the rest of the agent ban list is unchanged).
///
/// The client is located by class `atlas` + the label's exact title
/// (`gui::def_of`, read-only `hyprctl clients -j`); several clients match →
/// the first is raised and the ambiguity is logged. Nothing matches →
/// `{"ok":false,"detail":"окно не открыто"}` — the frontend falls back to
/// `windows/restore`. hyprctl unavailable → honest `ok:false`, no dispatch.
pub(crate) async fn api_windows_focus(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let Some(win) = v.get("win").and_then(|w| w.as_str()) else {
        return Err(ApiError::unprocessable("win must be a string"));
    };
    if !settings::PANELS.contains(&win) {
        return Err(win_unknown(win));
    }
    let title = gui::def_of(win)
        .ok_or_else(|| win_unknown(win))?
        .title;
    let clients = match hyprctl_clients().await {
        Ok(c) => c,
        Err(e) => return Ok(ok_json(json!({"ok": false, "win": win, "detail": e}))),
    };
    let hits = gui::atlas_clients(&clients, &title);
    let Some(addr) = hits.first() else {
        return Ok(ok_json(
            json!({"ok": false, "win": win, "detail": "окно не открыто"}),
        ));
    };
    if hits.len() > 1 {
        log::warn!(
            "atlas: windows/focus '{win}': {} клиентов с заголовком «{title}» — поднимаю первый {addr}",
            hits.len()
        );
    }
    match hyprctl(&["dispatch", "focuswindow", &format!("address:{addr}")]).await {
        Ok(_) => Ok(ok_json(json!({
            "ok": true, "win": win, "address": addr, "detail": "окно поднято",
        }))),
        Err(e) => Ok(ok_json(json!({
            "ok": false, "win": win, "detail": e,
        }))),
    }
}

// ------------------------------------------------ wezterm dock (hyprctl) ---

/// Dock preset of `POST /api/terminal/wezterm/dock` — where the atlas-wezterm
/// window goes relative to the atlas window and its monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DockPreset {
    /// From the atlas window's horizontal middle to the monitor's right edge,
    /// full monitor height (typical: atlas fills the left half → the dock is
    /// exactly the free right half).
    RightHalf,
    /// The monitor's right quarter, full monitor height — monitor-relative,
    /// independent of the atlas geometry.
    RightQuarter,
    /// Exactly the atlas host rect (pixel-in-pixel over it).
    Overlay,
}

impl DockPreset {
    pub(crate) fn parse(s: &str) -> Option<DockPreset> {
        match s {
            "right-half" => Some(DockPreset::RightHalf),
            "right-quarter" => Some(DockPreset::RightQuarter),
            "overlay" => Some(DockPreset::Overlay),
            _ => None,
        }
    }

    /// Russian label for details / the frontend log line.
    pub(crate) fn label(self) -> &'static str {
        match self {
            DockPreset::RightHalf => "правая половина",
            DockPreset::RightQuarter => "правая четверть",
            DockPreset::Overlay => "поверх окна atlas",
        }
    }
}

/// Pure geometry of a dock preset. All inputs and the output `(x, y, w, h)`
/// share ONE coordinate base — the desktop-global one `hyprctl` reports:
/// client `at` and monitor `x`/`y` both are global (monitor entries carry
/// their global origin, not screen-local zeros), and `movewindowpixel exact
/// X Y` addresses the same global space. Nothing here talks to Hyprland —
/// unit-tested arithmetic only.
pub(crate) fn dock_rect(
    preset: DockPreset,
    atlas_at: [i32; 2],
    atlas_size: [i32; 2],
    monitor_at: [i32; 2],
    monitor_size: [i32; 2],
) -> (i32, i32, i32, i32) {
    let [ax, ay] = atlas_at;
    let [aw, ah] = atlas_size;
    let [mx, my] = monitor_at;
    let [mw, mh] = monitor_size;
    match preset {
        DockPreset::Overlay => (ax, ay, aw, ah),
        DockPreset::RightHalf => {
            // anchor at the atlas window's own half; an atlas spanning
            // monitors (or degenerate geometry) is clamped into the monitor
            // that holds the atlas center — the caller passes that monitor
            let x = (ax + aw / 2).clamp(mx, mx + mw);
            (x, my, mx + mw - x, mh)
        }
        DockPreset::RightQuarter => {
            let q = mw / 4;
            (mx + mw - q, my, q, mh)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// right-half: from the atlas window's horizontal middle to the right
    /// monitor edge, full monitor height — on the monitor's own base.
    #[test]
    fn dock_rect_right_half() {
        // atlas fills the left half of monitor 0
        assert_eq!(
            dock_rect(DockPreset::RightHalf, [0, 200], [960, 600], [0, 0], [1920, 1080]),
            (480, 0, 1440, 1080)
        );
        // same picture on a right-hand monitor: the base is global, the
        // arithmetic identical
        assert_eq!(
            dock_rect(DockPreset::RightHalf, [2000, 100], [800, 600], [1920, 0], [1920, 1080]),
            (2400, 0, 1440, 1080)
        );
    }

    /// right-quarter: the monitor's right 25%, atlas-independent.
    #[test]
    fn dock_rect_right_quarter() {
        assert_eq!(
            dock_rect(DockPreset::RightQuarter, [0, 200], [960, 600], [0, 0], [1920, 1080]),
            (1440, 0, 480, 1080)
        );
        // odd width: floor division keeps the rect inside the monitor
        assert_eq!(
            dock_rect(DockPreset::RightQuarter, [500, 0], [700, 500], [0, 0], [1919, 1080]),
            (1440, 0, 479, 1080)
        );
    }

    /// overlay: exactly the atlas host rect, no transform.
    #[test]
    fn dock_rect_overlay() {
        assert_eq!(
            dock_rect(DockPreset::Overlay, [1727, 1495], [1701, 1373], [0, 0], [1920, 1080]),
            (1727, 1495, 1701, 1373)
        );
    }

    /// An atlas window spanning two monitors is clamped to the monitor
    /// holding its center (the caller passes that monitor): right-half and
    /// right-quarter stay fully inside it; overlay is exempt by definition —
    /// «exactly the atlas rect», spanning is the point there.
    #[test]
    fn dock_rect_clamps_to_monitor_holding_center() {
        // atlas spans [0..3000], center at x=1500 → monitor A (0..1920)
        let atlas_at = [0, 0];
        let atlas_size = [3000, 600];
        let mon_a = ([0, 0], [1920, 1080]);
        let (x, y, w, h) = dock_rect(DockPreset::RightHalf, atlas_at, atlas_size, mon_a.0, mon_a.1);
        assert_eq!((x, y, w, h), (1500, 0, 420, 1080));
        assert!(x >= mon_a.0[0] && x + w <= mon_a.0[0] + mon_a.1[0] && h == mon_a.1[1]);

        // same atlas, but the caller picks monitor B (1920..3840): the half
        // anchor clamps back into B — the rect never leaves the monitor
        let mon_b = ([1920, 0], [1920, 1080]);
        let (x, y, w, h) = dock_rect(DockPreset::RightHalf, atlas_at, atlas_size, mon_b.0, mon_b.1);
        assert_eq!((x, y, w, h), (1920, 0, 1920, 1080));
        assert!(x >= mon_b.0[0] && x + w <= mon_b.0[0] + mon_b.1[0]);

        // quarter stays monitor-relative either way
        let (qx, _, qw, _) = dock_rect(DockPreset::RightQuarter, atlas_at, atlas_size, mon_b.0, mon_b.1);
        assert_eq!((qx, qw), (3360, 480));
        // overlay = the raw atlas rect, spanning untouched
        assert_eq!(
            dock_rect(DockPreset::Overlay, atlas_at, atlas_size, mon_b.0, mon_b.1),
            (0, 0, 3000, 600)
        );
    }
}

