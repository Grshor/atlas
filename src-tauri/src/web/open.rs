//! File opening: `POST /api/open` — launcher argv construction, the bounded
//! hand-off wait (`run_opener`), and the honest outcome the panel renders.

use super::{join_result, ok_json, ApiError};
use crate::cfg;
use crate::util::ref_to_file;
use axum::response::Response;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};


/// How long the launcher gets to hand the file over before we stop claiming
/// success. `gio open` returns in ~50 ms here; a launcher still running after
/// that is waiting on a handler that took our `/dev/null` stdio and will never
/// show anything (see `opener_argv`) — the panel gets the truth, not a ✓.
const OPEN_HANDOFF: Duration = Duration::from_secs(2);

/// argv for the desktop launcher of `path`. The launcher is config
/// `binaries.opener` (default `gio`, what works in this setup): it asks the
/// MIME database, which sniffs content — `.md` → `text/markdown` →
/// `omawrite.desktop`, a GUI app — and honours `Terminal=true` entries via
/// `xdg-terminal-exec`. `xdg-open` under Hyprland (`XDG_CURRENT_DESKTOP=
/// Hyprland` matches no `detectDE` branch → `DE=generic`) instead runs
/// `file --mime-type` → `text/plain` → `nvim.desktop` and then executes its
/// `Exec` **directly, with our null stdio and `Terminal=true` ignored**: nvim
/// sits headless on /dev/null forever while xdg-open blocks on it — the
/// «↗ файл ничего не делает» report (six such parked pairs hung off the GUI
/// process when this was diagnosed, one per click). xdg-open stays as the
/// fallback for hosts without the configured opener.
fn opener_argv(path: &str) -> Vec<String> {
    let opener = cfg::opener_bin();
    let has = |bin: &str| {
        if bin.contains('/') {
            std::path::Path::new(bin).is_file()
        } else {
            std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
                .unwrap_or(false)
        }
    };
    if has(opener) {
        vec![opener.into(), "open".into(), path.into()]
    } else {
        vec!["xdg-open".into(), path.into()]
    }
}

/// stderr sinks for `run_opener`, unique per call.
static OPEN_ERR_SEQ: AtomicU64 = AtomicU64::new(0);

/// What the launcher did, within `handoff`.
///
/// The launcher is waited on by its **own exit status**, never by its output
/// pipes: GLib hands the launcher's stdout/stderr straight to the app it
/// starts (`/proc/<app>/fd/1` was the very pipe we would be reading), so the
/// app keeps that pipe open for as long as its window lives and an EOF wait
/// outlives the launcher — measured: window up, `ok:false`, the mirror image
/// of the bug this endpoint was fixed for. stderr therefore goes to a temp
/// file: readable the moment the launcher exits, even though the app holds the
/// fd forever. A launcher that outlives the budget has handed nothing over
/// (the xdg-open → headless-nvim symptom), so it is killed — the app it may
/// already have started is a process of its own and is left alone.
fn run_opener(argv: &[String], handoff: Duration) -> Result<(), String> {
    let program = argv[0].clone();
    let log = std::env::temp_dir().join(format!(
        "atlas-open-{}-{}.err",
        std::process::id(),
        OPEN_ERR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let sink = std::fs::File::create(&log).map_err(|e| format!("{program}: {e}"))?;
    let mut child = match std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(sink))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&log);
            return Err(format!("{program}: {e}"));
        }
    };
    let deadline = Instant::now() + handoff;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&log);
                return Err(format!("{program}: {e}"));
            }
        }
    };
    let said = std::fs::read(&log)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    let said = said.trim();
    let how = if said.is_empty() {
        String::new()
    } else {
        format!(" — {said}")
    };
    match status {
        Some(st) if st.success() => Ok(()),
        Some(st) => Err(format!(
            "{program}: exit {}{how}",
            st.code().unwrap_or(-1)
        )),
        None => Err(format!(
            "timeout after {:.1}s: {}{how}",
            handoff.as_secs_f64(),
            argv.join(" ")
        )),
    }
}

/// POST /api/open — open the local file behind a search ref in the desktop's
/// own viewer. `dry_run:true` returns the argv without executing anything.
pub(crate) async fn api_open(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let reference = v
        .get("ref")
        .and_then(|r| r.as_str())
        .ok_or_else(|| ApiError::unprocessable("ref must be a string"))?;
    let dry = v.get("dry_run").and_then(|d| d.as_bool()).unwrap_or(false);
    let Some(path) = ref_to_file(reference) else {
        return Err(ApiError::unprocessable(
            "за этой записью нет локального файла",
        ));
    };
    let path_str = path.to_string_lossy().into_owned();
    let argv = opener_argv(&path_str);
    if dry {
        return Ok(ok_json(json!({
            "ok": true,
            "argv": argv,
            "dry_run": true,
        })));
    }
    // The launcher's own outcome decides the answer, so `ok:false` carries its
    // exit status and stderr (no handler, no display, headless TUI handler)
    // instead of a ✓ for a window that never opened.
    let outcome = tokio::task::spawn_blocking(move || run_opener(&argv, OPEN_HANDOFF))
        .await
        .map_err(join_result)?;
    Ok(ok_json(match outcome {
        Ok(()) => json!({"ok": true, "opened": path_str}),
        Err(detail) => json!({"ok": false, "detail": detail}),
    }))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::web::support::{call, test_router};
    use axum::http::StatusCode;
    use std::time::{Duration, Instant};
    // ---- /api/open: the launcher's outcome, not just its spawn ----

    fn argv_of(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    /// A clean launcher exit is the only thing that means «opened».
    /// `/bin/sh` stands in for gio/xdg-open: no window, no desktop involved.
    #[test]
    fn open_launcher_exit_zero_is_success() {
        let a = argv_of(&["/bin/sh", "-c", "exit 0"]);
        assert_eq!(run_opener(&a, OPEN_HANDOFF), Ok(()));
    }

    /// The regression that made `util::run_cmd` unusable here: the launcher
    /// hands the file to the app and exits, while the app holds the inherited
    /// stdio open for as long as its window lives. The wait must be on the
    /// launcher itself, so this answers at once instead of timing out on an
    /// open that actually worked.
    #[test]
    fn open_launcher_hands_off_without_waiting_for_the_app() {
        let a = argv_of(&["/bin/sh", "-c", "sleep 2 & exit 0"]);
        let t0 = Instant::now();
        assert_eq!(run_opener(&a, OPEN_HANDOFF), Ok(()));
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "waited on the app, not on the launcher: {:?}",
            t0.elapsed()
        );
    }

    /// A launcher that fails must reach the user as `ok:false` plus its own
    /// words — the fake ✓ was the whole bug.
    #[test]
    fn open_launcher_failure_carries_exit_code_and_stderr() {
        let a = argv_of(&[
            "/bin/sh",
            "-c",
            "echo 'no default handler for text/markdown' >&2; exit 3",
        ]);
        let said = run_opener(&a, OPEN_HANDOFF).unwrap_err();
        assert!(said.contains("exit 3"), "the exit code is in the detail — {said}");
        assert!(
            said.contains("no default handler for text/markdown"),
            "stderr must survive into the detail — {said}"
        );
    }

    /// No launcher on disk at all: it is a negative outcome naming the binary,
    /// not a panic and not a ✓.
    #[test]
    fn open_missing_launcher_is_a_failure() {
        let a = argv_of(&["/nonexistent/atlas-open-launcher"]);
        let said = run_opener(&a, OPEN_HANDOFF).unwrap_err();
        assert!(said.contains("/nonexistent/atlas-open-launcher"), "{said}");
    }

    /// The symptom from the report in one test: the launcher never returns
    /// (xdg-open parked on a headless nvim). The budget must expire, the
    /// answer must be negative, and the stuck launcher must not be left
    /// behind. Short budget here; the endpoint uses 2 s.
    #[test]
    fn open_launcher_that_never_returns_is_a_failure() {
        let a = argv_of(&["/bin/sh", "-c", "exec sleep 30"]);
        let t0 = Instant::now();
        let said = run_opener(&a, Duration::from_millis(200)).unwrap_err();
        assert!(said.contains("timeout after 0.2s"), "{said}");
        assert!(t0.elapsed() < Duration::from_secs(5), "ran past the budget: {:?}", t0.elapsed());
    }

    /// argv contract: gio (with its `open` subcommand) when gio is on PATH,
    /// xdg-open otherwise; the path is always last.
    #[test]
    fn open_argv_names_the_launcher_and_the_path() {
        let a = opener_argv("/tmp/note.md");
        if a[0] == "gio" {
            assert_eq!(a, argv_of(&["gio", "open", "/tmp/note.md"]));
        } else {
            assert_eq!(a, argv_of(&["xdg-open", "/tmp/note.md"]));
        }
    }

    /// `/api/open`, dry branch: the ref→file mapping is unchanged, the body
    /// carries the launcher argv, and nothing is spawned. A ref with no local
    /// file behind it stays a 422.
    #[tokio::test]
    async fn open_dry_run_returns_the_launcher_argv_for_a_real_file() {
        let r = test_router();
        let dir = std::env::temp_dir().join(format!("atlas-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let note = dir.join("note.md");
        std::fs::write(&note, "# hi\n").unwrap();
        let note_s = note.to_string_lossy().into_owned();

        let body = json!({"ref": format!("vault:{note_s}"), "dry_run": true}).to_string();
        let (st, out) = call(&r, "POST", "/api/open", Some(&body)).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["dry_run"], true, "{out}");
        assert_eq!(
            out["argv"].as_array().unwrap().last().unwrap().as_str(),
            Some(note_s.as_str()),
            "{out}"
        );

        let (st, out) = call(&r, "POST", "/api/open", Some(r#"{"ref":"mnemopi:facts/x"}"#)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
