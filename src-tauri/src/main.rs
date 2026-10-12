// atlas: ONE binary — axum control-plane server + five Wayland toplevels.
// The Python layer is left untouched; this binary replaces it at cutover.

// Prevents an additional console window on Windows in release; harmless on Linux.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cfg;
mod cli;
mod db;
mod delegate;
mod deporigin;
mod embed;
mod focus;
mod gui;
mod http;
mod importers;
mod orch;
mod omp_rpc;
mod patrol;
mod runner;
mod settings;
mod stt;
mod tasks;
mod term;
mod util;

use std::sync::Arc;
use std::time::Duration;

fn print_usage() {
    println!(
        "atlas — control plane + desktop shell\n \
         \n \
         usage:\n \
           atlas                     GUI + HTTP server on 127.0.0.1:8787\n \
           atlas --serve-only        headless server without windows (systemd unit runs this)\n \
           atlas --port N            HTTP port (default 8787)\n \
           atlas --db PATH           SQLite path (default ~/.local/share/atlas/control.db)\n \
           atlas --config PATH       alternative config file (default ~/.config/atlas/config.toml)\n \
           config file               ~/.config/atlas/config.toml — optional overrides of paths and\n \
                                     external binaries (keys + defaults: README «Конфигурация»)\n \
           atlas notify \"текст\" [--project P] [--level info|warn|err] [--url http://127.0.0.1:8787]\n \
                                     send a notification to the running server (no DB, no GUI)\n \
           atlas board add \"<title>\" [--project P] [--quadrant q1..q4]\n \
                                     [--description \"...\"] [--dedupe-key KEY]\n \
           atlas board list [--quadrant qN] [--json]\n \
           atlas board status [--json]\n \
                                     board verbs (exit: 0 ok · 1 runtime · 2 usage · 3 unreachable)\n \
           atlas help                this text"
    );
}

/// What to do after a failed bind of the HTTP port (GUI mode).
#[derive(Debug, PartialEq)]
enum Reclaim {
    /// Stop atlas.service (the usual port squatter), then retry.
    StopServiceThenRetry,
    /// Just wait and retry (no systemctl, or the stop already happened).
    RetryAfterDelay,
    /// Keep the GUI on the foreign backend; window ops will not work here.
    FallbackForeignBackend,
}

/// Pure reclaim policy: bind attempt number (1-based) + systemctl presence
/// on PATH → action. Two retries total, the service stop happens once.
fn reclaim_plan(attempt: u8, systemctl_available: bool) -> Reclaim {
    match attempt {
        1 if systemctl_available => Reclaim::StopServiceThenRetry,
        1 | 2 => Reclaim::RetryAfterDelay,
        _ => Reclaim::FallbackForeignBackend,
    }
}

fn systemctl_available() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("systemctl").is_file()))
        .unwrap_or(false)
}

/// `systemctl --user stop atlas.service`; true on clean exit. Errors ignored
/// by policy (the unit may not exist), the wait is capped at ~5 s.
fn stop_atlas_service() -> bool {
    let mut child = match std::process::Command::new("systemctl")
        .args(["--user", "stop", "atlas.service"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!("systemctl stop atlas.service: {e}");
            return false;
        }
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return st.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                log::warn!("systemctl stop atlas.service timed out after 5s");
                return false;
            }
            Err(e) => {
                log::warn!("systemctl stop atlas.service: {e}");
                return false;
            }
        }
    }
}

/// Pure `/api/ui` verdict: only a live atlas answers 2xx with a JSON object
/// carrying a `panels` array (a plain `http.server` 404s; anything else that
/// happens to sit on the port is not an atlas).
fn is_atlas_ui_response(status: u16, body: &str) -> bool {
    (200..300).contains(&status)
        && serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("panels").map(|p| p.is_array()))
            .unwrap_or(false)
}

/// Does a *live atlas* already own `port`? Cheap honest probe: one blocking
/// `GET /api/ui` to 127.0.0.1 (instant against a local listener). Used by
/// `--serve-only` to tell "another atlas is up" from "a foreign process stole
/// the port".
fn live_atlas_on_port(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}");
    match cli::http_get(&url, "/api/ui") {
        Ok((status, body)) => is_atlas_ui_response(status, &body),
        Err(_) => false,
    }
}

/// GUI mode: server on its own thread, windows on the main thread. The process
/// serving the port must be the one that owns the windows — when the port is
/// held by the headless systemd unit, stop it and reclaim.
///
/// Spawned from the app `setup` closure (which runs AFTER the single-instance
/// plugin's setup): a second launch is forwarded to the primary and exits
/// inside the plugin, so it never reaches here, never races the port and never
/// stops atlas.service.
fn spawn_server_thread(port: u16) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime (server)");
        rt.block_on(async move {
            let state = http::AppState {
                orch: orch::Orchestrator::new(),
                stt: stt::SttState::new(),
            };
            let mut bound = http::bind(port).await;
            if bound.is_err() {
                log::warn!("port {port} busy — trying to reclaim it from atlas.service");
                for attempt in 1..=2 {
                    match reclaim_plan(attempt, systemctl_available()) {
                        Reclaim::StopServiceThenRetry => {
                            log::info!("stopping atlas.service to free port {port}");
                            stop_atlas_service();
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                        Reclaim::RetryAfterDelay => {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                        Reclaim::FallbackForeignBackend => break,
                    }
                    log::info!("rebinding port {port} (retry {attempt}/2)");
                    bound = http::bind(port).await;
                    if bound.is_ok() {
                        log::info!("port {port} reclaimed after {attempt} retry(ies)");
                        break;
                    }
                }
            }
            match bound {
                Err(e) => log::error!(
                    "{e} — port {port} is owned by another process; the GUI keeps running \
                     against that foreign backend, but window operations (/api/windows/*, \
                     /api/ui) report port_owner=other and will NOT work from this process"
                ),
                Ok(listener) => {
                    log::info!("this process owns port {port}: windows + server in one process");
                    startup_init().await;
                    patrol::spawn();
                    delegate::spawn_autopilot();
                    http::spawn_theme_watcher();
                    if let Err(e) = http::serve(listener, state).await {
                        log::error!("atlas server failed: {e}");
                    }
                }
            }
        });
    });
}

/// Pure `--config <path>` extraction over the full server argv (before the
/// verb dispatch — the config must be resolved before `--db`'s default).
/// `Ok(None)` = flag absent (default config file); `Ok(Some(path))` = the
/// last `--config` wins; `Err` = flag without a value (usage error, exit 2).
fn config_override(argv: &[String]) -> Result<Option<std::path::PathBuf>, String> {
    let mut found = None;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if a == "--config" {
            found = Some(
                it.next()
                    .ok_or_else(|| "--config needs a path".to_string())?
                    .clone()
                    .into(),
            );
        }
    }
    Ok(found)
}

fn parse_args() -> Option<cfg::Cfg> {
    let mut port: u16 = 8787;
    let mut db_path = cfg::default_db_path();
    let mut serve_only = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--serve-only" => serve_only = true,
            "--config" => {
                // consumed and validated before init_config (see
                // config_override); here only so the parser does not
                // reject the flag as unknown
                let _ = args.next();
            }
            "--port" => {
                port = args.next().and_then(|v| v.parse().ok()).or_else(|| {
                    eprintln!("atlas: --port needs a number");
                    std::process::exit(2);
                })?;
            }
            "--db" => {
                db_path = args
                    .next()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| {
                        eprintln!("atlas: --db needs a path");
                        std::process::exit(2);
                    });
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => {
                eprintln!("atlas: unknown argument {other:?}");
                print_usage();
                std::process::exit(2);
            }
        }
    }
    Some(cfg::Cfg {
        port,
        db_path,
        serve_only,
    })
}

/// Schema + initial sync, retried on transient SQLite locks (server.py lifespan).
async fn startup_init() -> Arc<db::Db> {
    let app = db::init().expect("init db");
    // Pre-load the embedding model on a background thread: the cold load
    // (~1 min) used to happen inside the first sync, on the shared DB lock,
    // which stalled every endpoint. Fire-and-forget — startup never waits for
    // it, and a failed load is `embed::backend`'s own warn (search degrades to
    // FTS-only, the next sync retries). Startup path only: tests never run it.
    std::thread::spawn(embed::warm_up);
    // A delegation never survives a server restart (both runners are
    // in-process): settle the rows that would otherwise claim to run forever.
    delegate::reap_stale();
    let mut last: Option<String> = None;
    for attempt in 1..=4 {
        match importers::sync_all(Arc::clone(&app)).await {
            Ok(_) => return app,
            Err(e) => {
                if db::is_busy(&e) {
                    last = Some(format!("{e}"));
                    log::warn!("startup sync retry {attempt}: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                } else {
                    log::error!("startup sync failed: {e}");
                    return app;
                }
            }
        }
    }
    log::error!(
        "startup sync failed, serving last state: {}",
        last.unwrap_or_default()
    );
    app
}

/// FRAGILE.md §10 diagnostic: the linked webkit2gtk version via the official
/// C API (`webkit_get_{major,minor,micro}_version`, plain getters exported by
/// libwebkit2gtk, which this binary already links through tauri/wry). No crate
/// in the tree re-exports webkit2gtk-sys, and a new dependency for one log
/// line is not justified — so the three symbols are declared here.
#[cfg(target_os = "linux")]
fn webkit_version() -> Option<(u32, u32, u32)> {
    extern "C" {
        fn webkit_get_major_version() -> std::ffi::c_uint;
        fn webkit_get_minor_version() -> std::ffi::c_uint;
        fn webkit_get_micro_version() -> std::ffi::c_uint;
    }
    // SAFETY: pure version getters of the already-loaded libwebkit2gtk; no
    // state, no preconditions.
    Some(unsafe {
        (
            webkit_get_major_version(),
            webkit_get_minor_version(),
            webkit_get_micro_version(),
        )
    })
}

/// Off-Linux the DMABUF workaround is still set (the env var is harmless),
/// but the webkit C API does not exist there: honest no-version note.
#[cfg(not(target_os = "linux"))]
fn webkit_version() -> Option<(u32, u32, u32)> {
    None
}

/// The one startup line about the DMABUF workaround: with the version number
/// where it is queryable, so rotting of the workaround (protocol error 71 was
/// hit before) is at least visible against a known webkit.
fn dmabuf_log_line(version: Option<(u32, u32, u32)>) -> String {
    match version {
        Some((maj, min, mic)) => format!(
            "webkit {maj}.{min}.{mic}; DMABUF-рендер отключён обходкой — \
             если артефакты ушли в новой webkit, попробуй убрать \
             WEBKIT_DISABLE_DMABUF_RENDERER"
        ),
        None => "версия webkit не определяется; DMABUF-рендер отключён обходкой — \
                 если артефакты ушли в новой webkit, попробуй убрать \
                 WEBKIT_DISABLE_DMABUF_RENDERER"
            .to_string(),
    }
}

fn main() {
    // webkit2gtk's DMABUF renderer trips over Wayland explicit sync on this
    // compositor: protocol error 71 (wp_linux_drm_syncobj_surface_v1,
    // "Missing acquire timeline") kills the app at startup. Shared-memory
    // rendering is the documented fallback; an explicit user setting wins.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    // CLI verbs run before any config/DB/GUI machinery.
    let mut argv = std::env::args().skip(1);
    match argv.next().as_deref() {
        Some("notify") => match cli::parse_notify_args(argv.collect()) {
            Ok(args) => std::process::exit(cli::notify_main(args)),
            Err(e) => {
                eprintln!("atlas: {e}");
                print_usage();
                std::process::exit(2);
            }
        },
        Some("board") => match cli::parse_board_args(argv.collect()) {
            Ok(cmd) => {
                let out = match cmd {
                    cli::BoardCmd::Add(a) => cli::board_add_main(a),
                    cli::BoardCmd::List(a) => cli::board_list_main(a),
                    cli::BoardCmd::Status(a) => cli::board_status_main(a),
                };
                out.emit();
                std::process::exit(out.code());
            }
            Err(e) => {
                eprintln!("atlas: {e}");
                std::process::exit(2);
            }
        },
        Some("help") => {
            print_usage();
            std::process::exit(0);
        }
        _ => {}
    }

    // The optional user config (~/.config/atlas/config.toml, or the --config
    // override such as the dev instance's config-dev.toml) must be resolved
    // before parse_args: the `--db` default already derives from data_dir.
    let config_override = match config_override(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("atlas: {e}");
            print_usage();
            std::process::exit(2);
        }
    };
    cfg::init_config(config_override.as_deref());

    let app_cfg = match parse_args() {
        Some(c) => c,
        None => return,
    };
    let serve_only = app_cfg.serve_only;
    let _ = cfg::CFG.set(app_cfg);
    cfg::init_logging(serve_only);
    cfg::log_config_note();
    // FRAGILE.md §10: name the webkit the DMABUF workaround above applies to
    // (logged here, after logging init — the env var itself is set earlier,
    // before any webkit/GTK code can run).
    log::info!("{}", dmabuf_log_line(webkit_version()));

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    if serve_only {
        rt.block_on(async move {
            let state = http::AppState {
                orch: orch::Orchestrator::new(),
                stt: stt::SttState::new(),
            };
            let port = cfg::cfg().port;
            match http::bind(port).await {
                Err(e) => {
                    // Port busy: if a *live atlas* already serves it, this
                    // headless copy has nothing to do — exit 0 so
                    // Restart=on-failure does not flap the unit. A foreign
                    // squatter is a real failure.
                    if tokio::task::spawn_blocking(move || live_atlas_on_port(port))
                        .await
                        .unwrap_or(false)
                    {
                        log::info!("порт {port} уже обслуживает живой atlas — выходим штатно");
                        std::process::exit(0);
                    }
                    log::error!("atlas server failed: {e}");
                    std::process::exit(1);
                }
                Ok(listener) => {
                    startup_init().await;
                    patrol::spawn();
                    delegate::spawn_autopilot();
                    http::spawn_theme_watcher();
                    if let Err(e) = http::serve(listener, state).await {
                        log::error!("atlas server failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
        });
        return;
    }

    // GUI mode: windows on the main thread; the server thread is spawned from
    // the app `setup` closure below (see spawn_server_thread), which tauri runs
    // AFTER the single-instance plugin's setup — a second launch is forwarded
    // and exits inside the plugin, so it never races the port or stops the unit.
    //
    // Single-instance MUST be the first plugin: a second GUI launch is
    // forwarded to the primary instance and exits right here — it never
    // creates its own window set (previously each launch opened ALL its
    // panels programmatically, so two launches meant two `atlas` window
    // groups on the desktop) and never lingers. The primary side just raises
    // its host window; nothing else happens in the callback.
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            gui::raise_primary(app);
        }))
        .setup(|app| {
            // Runs AFTER the single-instance plugin's setup, so by now a
            // duplicate launch has forwarded to the primary and exited.
            spawn_server_thread(cfg::cfg().port);
            // Process-global handle so HTTP handlers (own threads) can
            // manage windows; unset in --serve-only, which the HTTP layer
            // treats as "GUI недоступен".
            let _ = gui::APP.set(app.handle().clone());
            // Restore the panel windows the user had NOT minimised into the
            // host tabs. tauri.conf.json declares the host only, so without
            // this a fresh launch would show a single window. The server
            // thread initialises the DB (and therefore ui.tabs) in parallel:
            // wait for that first, then create on the main thread. A GUI that
            // fell back to a foreign backend never initialises a DB here —
            // it also owns no windows to restore, so it stays host-only.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                while !db::is_ready() && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if !db::is_ready() {
                    log::warn!(
                        "atlas: DB not initialised in this process (foreign backend?) — \
                         startup windows skipped"
                    );
                    return;
                }
                // read the tab state off-thread (DB lock), create windows on
                // the main thread (GTK)
                let tabs = gui::stored_tabs();
                if let Err(e) = handle.run_on_main_thread(move || gui::open_panels(&tabs)) {
                    log::warn!("atlas: startup windows: {e}");
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running atlas shell");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_override_flag_honored() {
        // flag → file, exactly as the dev instance passes it
        assert_eq!(
            config_override(&[
                "--serve-only".to_string(),
                "--config".to_string(),
                "/home/u/.config/atlas/config-dev.toml".to_string()
            ]),
            Ok(Some(std::path::PathBuf::from(
                "/home/u/.config/atlas/config-dev.toml"
            )))
        );
        // flag absent → default config, unchanged behaviour
        assert_eq!(config_override(&["--serve-only".to_string()]), Ok(None));
        assert_eq!(config_override(&[]), Ok(None));
        // the last --config wins (same rule as --port/--db: later flag wins)
        assert_eq!(
            config_override(&[
                "--config".to_string(),
                "/tmp/a.toml".to_string(),
                "--config".to_string(),
                "/tmp/b.toml".to_string()
            ]),
            Ok(Some(std::path::PathBuf::from("/tmp/b.toml")))
        );
        // flag without a value → usage error, named before any I/O
        assert_eq!(
            config_override(&["--config".to_string()]),
            Err("--config needs a path".to_string())
        );
    }

    #[test]
    fn reclaim_policy() {
        // table: (attempt, systemctl available) → action
        let cases = [
            (1u8, true, Reclaim::StopServiceThenRetry),
            (1, false, Reclaim::RetryAfterDelay),
            (2, true, Reclaim::RetryAfterDelay), // the stop already happened
            (2, false, Reclaim::RetryAfterDelay),
            (3, true, Reclaim::FallbackForeignBackend),
            (9, false, Reclaim::FallbackForeignBackend),
        ];
        for (attempt, sysctl, want) in cases {
            assert_eq!(reclaim_plan(attempt, sysctl), want, "attempt={attempt} systemctl={sysctl}");
        }
    }

    #[test]
    fn systemctl_lookup_does_not_panic() {
        // real environment: systemctl exists on PATH; the check itself is cheap
        let _ = systemctl_available();
    }

    /// Bind a throwaway 127.0.0.1 port and answer exactly one request with
    /// `response`; returns the port. A fixture "listener" standing in for a
    /// process that already owns the port.
    fn fixture_listener(response: String) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut sock, &mut buf);
                let _ = std::io::Write::write_all(&mut sock, response.as_bytes());
            }
        });
        port
    }

    #[test]
    fn live_atlas_probe_distinguishes_the_owner() {
        // a live atlas: 2xx + JSON with a `panels` array → exit-0 decision
        let body = r#"{"panels":["orchestrator","settings"],"gui":true}"#;
        let atlas = format!(
            "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        assert!(live_atlas_on_port(fixture_listener(atlas)), "atlas fixture → live");
        // foreign listener (e.g. `python3 -m http.server`): 404 → keep error path
        assert!(
            !live_atlas_on_port(fixture_listener(
                "HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n".into()
            )),
            "404 fixture → not atlas"
        );
        // foreign listener answering 200 with garbage → keep error path
        assert!(
            !live_atlas_on_port(fixture_listener(
                "HTTP/1.0 200 OK\r\nContent-Length: 11\r\n\r\nhello world".into()
            )),
            "garbage fixture → not atlas"
        );
        // nothing listening → keep error path
        assert!(!live_atlas_on_port(9), "closed port → not atlas");
    }

    #[test]
    fn atlas_ui_verdict() {
        assert!(is_atlas_ui_response(200, r#"{"panels":[]}"#));
        assert!(is_atlas_ui_response(204, r#"{"panels":["a"]}"#));
        assert!(!is_atlas_ui_response(404, r#"{"panels":[]}"#));
        assert!(!is_atlas_ui_response(200, r#"{"windows":[],"gui":true}"#));
        assert!(!is_atlas_ui_response(200, "<html>not atlas</html>"));
    }

    #[test]
    fn dmabuf_note_names_webkit_when_known() {
        let line = dmabuf_log_line(Some((2, 48, 1)));
        assert!(line.starts_with("webkit 2.48.1;"), "{line}");
        assert!(line.contains("WEBKIT_DISABLE_DMABUF_RENDERER"), "{line}");
    }

    #[test]
    fn dmabuf_note_honest_without_version() {
        let line = dmabuf_log_line(None);
        assert!(!line.contains("webkit 2."), "no fabricated version: {line}");
        assert!(line.contains("не определяется"), "{line}");
        assert!(line.contains("WEBKIT_DISABLE_DMABUF_RENDERER"), "{line}");
    }
}
