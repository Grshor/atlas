//! GUI plumbing: the process-global AppHandle (set in tauri::Builder::setup)
//! and window open/close/live-list helpers for the minimize/restore API.
//! In `--serve-only` the handle is unset and every call degrades gracefully.
use crate::settings::{HOST, PANELS};
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub static APP: OnceLock<AppHandle> = OnceLock::new();

pub fn app() -> Option<&'static AppHandle> {
    APP.get()
}

pub struct WinDef {
    pub label: &'static str,
    pub title: String,
    pub url: String,
    pub w: f64,
    pub h: f64,
    pub x: f64,
    pub y: f64,
}

/// Window geometry/titles come from the embedded tauri.conf.json (no
/// duplicated literals); panels not opened at startup get a definition from
/// SYNTH_WINDOWS below in the same style.
fn conf_windows() -> Vec<Value> {
    serde_json::from_str::<Value>(include_str!("../tauri.conf.json"))
        .ok()
        .and_then(|c| {
            c.get("app")
                .and_then(|a| a.get("windows"))
                .and_then(|w| w.as_array())
                .cloned()
        })
        .unwrap_or_default()
}

/// Every panel except the host is created programmatically: tauri.conf.json
/// declares only `projects`, so a fresh launch opens exactly the windows the
/// user did NOT minimise into the host tabs (`ui.tabs`). Geometry/titles match
/// the values that used to sit in tauri.conf.json.
const SYNTH_WINDOWS: [(&str, &str, f64, f64, f64, f64); 8] = [
    ("board", "atlas — Доска", 1240.0, 780.0, 180.0, 120.0),
    ("deps", "atlas — Зависимости", 860.0, 560.0, 1200.0, 80.0),
    ("search", "atlas — Поиск", 760.0, 540.0, 1400.0, 520.0),
    ("sources", "atlas — Источники", 620.0, 460.0, 240.0, 760.0),
    ("herdr", "atlas — herdr", 900.0, 640.0, 200.0, 200.0),
    ("view", "atlas — Просмотр", 1100.0, 760.0, 240.0, 240.0),
    ("terminal", "atlas — Терминал", 900.0, 560.0, 280.0, 280.0),
    ("orchestrator", "atlas — Оркестратор", 900.0, 680.0, 320.0, 320.0),
];

/// Pure startup decision: the panel windows a fresh launch must create =
/// every panel except the host and the ones minimised into the host tabs, in
/// PANELS order. `tabs` entries that are not panels are ignored (a stale
/// label cannot resurrect a window).
pub fn startup_windows(panels: &[&'static str], tabs: &[String]) -> Vec<&'static str> {
    panels
        .iter()
        .copied()
        .filter(|p| *p != HOST && !tabs.iter().any(|t| t == p))
        .collect()
}

/// The stored tab state (`ui.tabs`). Reads the DB, so call it OFF the main
/// thread — the panel windows themselves must be created ON it.
pub fn stored_tabs() -> Vec<String> {
    crate::settings::tabs(&crate::db::db_static().lock())
}

/// Create the panel windows a fresh launch needs for the given tab state.
/// MUST run on the main thread (GTK); the caller has already read `ui.tabs`,
/// so nothing here touches the DB.
pub fn open_panels(tabs: &[String]) {
    let wanted = startup_windows(&PANELS, tabs);
    log::info!(
        "atlas: startup windows — tabs {:?}, creating {:?}",
        tabs,
        wanted
    );
    for label in wanted {
        if let Err(e) = open(label) {
            log::warn!("atlas: startup window '{label}': {e}");
        }
    }
}

pub fn def_of(label: &str) -> Option<WinDef> {
    if let Some(w) = conf_windows().into_iter().find(|w| w["label"] == label) {
        return Some(WinDef {
            label: label_static(label)?,
            title: w["title"].as_str().unwrap_or("atlas").to_string(),
            url: w["url"].as_str().unwrap_or("index.html").to_string(),
            w: w["width"].as_f64().unwrap_or(800.0),
            h: w["height"].as_f64().unwrap_or(600.0),
            x: w["x"].as_f64().unwrap_or(80.0),
            y: w["y"].as_f64().unwrap_or(80.0),
        });
    }
    let (label, title, w, h, x, y) = *SYNTH_WINDOWS.iter().find(|s| s.0 == label)?;
    Some(WinDef {
        label: label_static(label)?,
        title: title.into(),
        url: format!("index.html?win={label}"),
        w,
        h,
        x,
        y,
    })
}

fn label_static(label: &str) -> Option<&'static str> {
    PANELS.iter().copied().find(|p| *p == label)
}

/// Labels of the panel toplevels that exist right now (derived live, so a
/// window the user closed via the WM disappears from here immediately).
pub fn live() -> Vec<String> {
    APP.get()
        .map(|a| {
            let mut v: Vec<String> = a
                .webview_windows()
                .keys()
                .filter(|k| PANELS.contains(&k.as_str()))
                .cloned()
                .collect();
            v.sort();
            v
        })
        .unwrap_or_default()
}

/// Open (or focus) a panel window. Ok(true) = created, Ok(false) = was/gets focused.
pub fn open(label: &str) -> Result<bool, String> {
    let Some(app) = APP.get() else {
        return Err("GUI не запущен (--serve-only): окна недоступны".into());
    };
    if !PANELS.contains(&label) {
        return Err(format!("unknown panel '{label}'"));
    }
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.set_focus();
        return Ok(false);
    }
    let d = def_of(label).ok_or_else(|| format!("unknown panel '{label}'"))?;
    WebviewWindowBuilder::new(app, d.label, WebviewUrl::App(d.url.into()))
        .title(d.title)
        .inner_size(d.w, d.h)
        .position(d.x, d.y)
        .resizable(true)
        .visible(true)
        .build()
        .map_err(|e| format!("не удалось открыть окно '{label}': {e}"))?;
    Ok(true)
}

/// Close a panel window; closing an already-gone window is fine.
pub fn close(label: &str) -> Result<(), String> {
    let Some(app) = APP.get() else {
        return Err("GUI не запущен (--serve-only): окна недоступны".into());
    };
    match app.get_webview_window(label) {
        Some(w) => w.close().map_err(|e| format!("close '{label}': {e}")),
        None => Ok(()),
    }
}

pub fn host_present() -> bool {
    live().iter().any(|w| w == HOST)
}

/// Addresses of the live Hyprland clients whose class is `atlas` AND whose
/// title equals the panel's exact title, in `hyprctl clients -j` order.
/// Pure: the HTTP handler feeds it the parsed client list and dispatches
/// `focuswindow` on the first hit (ambiguity is logged there, never hidden).
pub fn atlas_clients(clients: &Value, title: &str) -> Vec<String> {
    clients
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|c| c["class"] == "atlas" && c["title"] == title)
                .filter_map(|c| c["address"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Anti-flap verification budget: the minimize/restore endpoints answer only
/// after the change is confirmed against the live window list — a Tauri call
/// returning says nothing about when (or whether) the WM actually maps the
/// toplevel. Probed every [`WINDOW_VERIFY_TICK`] for at most
/// [`WINDOW_VERIFY`].
pub const WINDOW_VERIFY: Duration = Duration::from_secs(2);
pub const WINDOW_VERIFY_TICK: Duration = Duration::from_millis(50);

/// Poll `probe` every `tick` until it equals `want`, at most `timeout`.
/// `false` is the honest timeout the handler turns into `ok:false` (the
/// frontend retries once). tokio's clock is the injection point: tests run
/// with paused time, the spy probe supplies the states.
pub async fn wait_until(
    mut probe: impl FnMut() -> bool,
    want: bool,
    timeout: Duration,
    tick: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if probe() == want {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(tick).await;
    }
}

/// Verified live state of one panel toplevel: true once the label is there
/// (`want_live:true`) or no longer there (`want_live:false`), polled against
/// the real window list up to [`WINDOW_VERIFY`].
pub async fn wait_window(label: &str, want_live: bool) -> bool {
    wait_until(
        || live().iter().any(|w| w == label),
        want_live,
        WINDOW_VERIFY,
        WINDOW_VERIFY_TICK,
    )
    .await
}

/// Pure choice of the window a second launch should raise in the PRIMARY
/// instance: the host window when it exists, else the first live panel,
/// None when this instance has no windows at all (e.g. the headless
/// `--serve-only` unit received the forwarded launch).
pub fn primary_focus_target(live: &[String]) -> Option<&'static str> {
    if live.iter().any(|w| w == HOST) {
        return Some(HOST);
    }
    live.iter()
        .find_map(|w| PANELS.iter().copied().find(|p| *p == w.as_str()))
}

/// The single-instance callback body (PRIMARY instance side): raise the
/// chosen window, nothing else — no window creation, no state changes.
pub fn raise_primary(app: &AppHandle) {
    let target = primary_focus_target(&live());
    let Some(label) = target else {
        log::info!("atlas: второй запуск получен, но окон нет — нечего поднимать");
        return;
    };
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        log::info!("atlas: второй запуск — поднято окно '{label}' первичного экземпляра");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_defs_come_from_tauri_conf() {
        // only the host window is declared in the conf; every other panel is
        // created programmatically from SYNTH_WINDOWS
        let conf = conf_windows();
        assert_eq!(conf.len(), 1, "tauri.conf.json must declare the host only");
        assert_eq!(conf[0]["label"], "projects");
        let host = def_of("projects").expect("projects in conf");
        assert_eq!(host.title, "atlas — Проекты");
        assert_eq!(host.url, "index.html?win=projects");
        assert_eq!((host.w, host.h), (1180.0, 720.0));

        // the former conf windows keep their geometry through the table
        let board = def_of("board").expect("board synthesized");
        assert_eq!(board.title, "atlas — Доска");
        assert_eq!(board.url, "index.html?win=board");
        assert_eq!((board.w, board.h), (1240.0, 780.0));
        let deps = def_of("deps").expect("deps synthesized");
        assert_eq!(deps.title, "atlas — Зависимости");
        assert_eq!((deps.w, deps.h), (860.0, 560.0));
        let sources = def_of("sources").expect("sources synthesized");
        assert_eq!((sources.w, sources.h), (620.0, 460.0));
        let search = def_of("search").expect("search synthesized");
        assert_eq!((search.w, search.h), (760.0, 540.0));
        let herdr = def_of("herdr").expect("herdr synthesized");
        assert_eq!(herdr.title, "atlas — herdr");
        assert_eq!(herdr.url, "index.html?win=herdr");
        assert_eq!((herdr.w, herdr.h), (900.0, 640.0));
        let view = def_of("view").expect("view synthesized");
        assert_eq!(view.title, "atlas — Просмотр");
        assert_eq!((view.w, view.h), (1100.0, 760.0));
        let term = def_of("terminal").expect("terminal synthesized");
        assert_eq!(term.title, "atlas — Терминал");
        assert_eq!((term.w, term.h), (900.0, 560.0));
        let orch = def_of("orchestrator").expect("orchestrator synthesized");
        assert_eq!(orch.title, "atlas — Оркестратор");
        assert_eq!(orch.url, "index.html?win=orchestrator");
        assert_eq!((orch.w, orch.h), (900.0, 680.0));
        // the «Настройки» window is gone (settings open inside a window now)
        assert!(def_of("settings").is_none());
        assert!(!PANELS.contains(&"settings"));
        assert!(def_of("nope").is_none());
        // every panel resolves
        for p in PANELS {
            assert!(def_of(p).is_some(), "{p} must have a window def");
        }
    }

    /// The windows/focus matcher over a `hyprctl clients -j` fixture: only
    /// class=atlas + the panel's exact title match, in input order; foreign
    /// classes, foreign titles and malformed payloads match nothing.
    #[test]
    fn atlas_clients_match_class_and_exact_title() {
        let clients: Value = serde_json::json!([
            {"address": "0x111", "class": "kitty", "title": "atlas — Оркестратор"},
            {"address": "0x222", "class": "atlas", "title": "atlas — Доска"},
            {"address": "0x333", "class": "atlas", "title": "atlas — Оркестратор"},
            {"address": "0x444", "class": "atlas", "title": "постороннее окно"}
        ]);
        assert_eq!(atlas_clients(&clients, "atlas — Оркестратор"), vec!["0x333"]);
        assert_eq!(atlas_clients(&clients, "atlas — Доска"), vec!["0x222"]);
        assert!(atlas_clients(&clients, "atlas — Терминал").is_empty());
        // ambiguity: both candidates come back in hyprctl order — the
        // handler raises the first and logs
        let dup: Value = serde_json::json!([
            {"address": "0xaaa", "class": "atlas", "title": "atlas — herdr"},
            {"address": "0xbbb", "class": "atlas", "title": "atlas — herdr"}
        ]);
        assert_eq!(atlas_clients(&dup, "atlas — herdr"), vec!["0xaaa", "0xbbb"]);
        // malformed payloads match nothing, never panic
        assert!(atlas_clients(&serde_json::json!({}), "atlas — Доска").is_empty());
        assert!(atlas_clients(&serde_json::json!([{"class": "atlas"}]), "x").is_empty());
    }

    /// Every panel def carries the `atlas — <Панель>` title the Hyprland
    /// matcher relies on (host included).
    #[test]
    fn every_panel_title_has_the_atlas_dash_shape() {
        for label in PANELS {
            let d = def_of(label).unwrap_or_else(|| panic!("def of {label}"));
            assert!(d.title.starts_with("atlas — "), "{label}: {}", d.title);
        }
    }

    /// The anti-flap wait loop: spy probe for the states, tokio's paused
    /// clock for the time — the full 2 s budget elapses virtually, instantly.
    #[tokio::test(start_paused = true)]
    async fn wait_until_polls_and_times_out() {
        use std::cell::Cell;
        // appears on the 3rd probe
        let n = Cell::new(0u32);
        let ok = wait_until(
            || {
                n.set(n.get() + 1);
                n.get() >= 3
            },
            true,
            WINDOW_VERIFY,
            WINDOW_VERIFY_TICK,
        )
        .await;
        assert!(ok);
        assert_eq!(n.get(), 3, "returns as soon as the probe says want");

        // never appears → honest false at the deadline, after a bounded
        // number of probes (2 s / 50 ms = 40 ticks, +1 for the first probe)
        let n = Cell::new(0u32);
        let ok = wait_until(
            || {
                n.set(n.get() + 1);
                false
            },
            true,
            WINDOW_VERIFY,
            WINDOW_VERIFY_TICK,
        )
        .await;
        assert!(!ok, "timeout is an honest false");
        assert!((40..=41).contains(&n.get()), "polled the whole budget: {}", n.get());

        // disappearance: probe reports «live» first, then «gone» — found
        let present = Cell::new(true);
        let ok = wait_until(
            || {
                let was = present.get();
                present.set(false);
                !was
            },
            false,
            WINDOW_VERIFY,
            WINDOW_VERIFY_TICK,
        )
        .await;
        assert!(ok, "want=false finds the window gone");
    }

    #[test]
    fn startup_windows_follow_stored_tabs() {
        // nothing minimised → every panel except the host
        let all = startup_windows(&PANELS, &[]);
        assert_eq!(
            all,
            vec!["board", "deps", "search", "sources", "herdr", "view", "terminal", "orchestrator"]
        );
        assert!(!all.contains(&HOST), "the host is never created programmatically");
        // the ticket's example: deps/search/sources are tabs → no such windows
        let tabs: Vec<String> = ["deps", "search", "sources"].iter().map(|s| s.to_string()).collect();
        let wanted = startup_windows(&PANELS, &tabs);
        assert_eq!(wanted, vec!["board", "herdr", "view", "terminal", "orchestrator"]);
        for t in &tabs {
            assert!(!wanted.contains(&t.as_str()), "{t} is a tab, not a window");
        }
        // a fully tabbed UI leaves only the host window
        let everything: Vec<String> = PANELS
            .iter()
            .filter(|p| **p != HOST)
            .map(|p| p.to_string())
            .collect();
        assert!(startup_windows(&PANELS, &everything).is_empty());
        // stale labels (a removed panel) cannot resurrect a window
        let stale = vec!["settings".to_string(), "board".to_string()];
        assert_eq!(
            startup_windows(&PANELS, &stale),
            vec!["deps", "search", "sources", "herdr", "view", "terminal", "orchestrator"]
        );
    }

    #[test]
    fn live_and_host_are_safe_without_gui() {
        // in tests (and --serve-only) there is no AppHandle
        assert!(APP.get().is_none());
        assert!(live().is_empty());
        assert!(!host_present());
        assert!(open("board").is_err());
        assert!(close("board").is_err());
    }

    /// Single-instance callback decision: raise the host when present, else
    /// the first live panel, nothing without windows. The callback itself
    /// does nothing else (no window creation, no state changes).
    #[test]
    fn primary_focus_target_prefers_host_then_panels() {
        let s = |v: &[&str]| -> Vec<String> { v.iter().map(|x| x.to_string()).collect() };
        assert_eq!(primary_focus_target(&s(&["board", "projects"])), Some(HOST));
        assert_eq!(primary_focus_target(&s(&["projects"])), Some(HOST));
        // host closed by the user → raise a live panel
        assert_eq!(primary_focus_target(&s(&["terminal"])), Some("terminal"));
        // labels outside the panel registry are ignored
        assert_eq!(primary_focus_target(&s(&["ghost"])), None);
        assert_eq!(primary_focus_target(&[]), None);
    }
}
