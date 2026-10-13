//! Shared transport of the two hand-written `omp --mode rpc` clients: the
//! interactive panel driver ([`crate::orch`]) and the headless delegation
//! driver ([`crate::delegate`]). Everything that used to be duplicated and
//! could drift lives HERE: the argv shape, the JSONL frame decode, the wire
//! format of an outgoing frame, the spawn stdio setup, the `ready` handshake
//! window and the SIGTERM→SIGKILL escalation. The POLICY stays with the
//! consumers: frame→event/ring mapping, the 5-minute approval auto-deny
//! (panel only), ring mirroring (delegation only), turn timeouts, and the
//! kill style (the panel restart SIGKILLs the tokio child; a delegation
//! cancel escalates through [`kill_graceful`] or, for a transient-unit
//! child, [`stop_unit_graceful`]). The delegation spawn mode ([`SPAWN_MODE`])
//! and the `systemd-run` wrapper ([`systemd_run_argv`]) live here too.
//!
//! The drivers differ in one structural way that is preserved, not
//! papered over: the panel child is a `tokio::process` child (async stdin
//! writes, `kill_on_drop` backstop), a delegation child is a plain
//! `std::process` child driven from a dedicated thread. Each flavor is a
//! thin wrapper over the same primitives, so the protocol cannot fork.

use crate::cfg;
use crate::runner;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, mpsc};
use std::time::{Duration, Instant};

/// How long the headless start waits for omp's `{"type":"ready",…}` frame
/// before sending the prompt anyway. omp announces ready when the rpc host is
/// up (see omp://rpc.md); a silent host cannot wedge the start — the prompt
/// goes out at the same 1 s mark the old blind sleep used.
pub const READY_WAIT: Duration = Duration::from_secs(1);

/// How long a delegation start waits for the transient unit to report a
/// non-zero `MainPID` before declaring the start failed. On this class of
/// boxes systemd-run returns with the unit already running, so the usual
/// cost is one query.
pub const MAINPID_WAIT: Duration = Duration::from_secs(2);

/// The delegation sandbox applied to every transient worker unit (§7), one
/// place. Starts with only what cannot break a normal worker — an own /tmp —
/// deeper sandboxing (ProtectHome/ReadWrite paths) is a follow-up once
/// `PrivateTmp` has proven safe in the wild.
pub const DELEG_SANDBOX_PROPERTIES: &[&str] = &["PrivateTmp=yes"];

/// How delegation children spawn, detected once at first use — which is
/// startup (`delegate::reap_stale` reads it). `systemd-run` on PATH → the
/// child becomes a transient `--user` unit; otherwise exactly the old plain
/// direct spawn. No config key on purpose: a platform binary is a capability,
/// not a user preference.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpawnMode {
    /// Direct `std::process` spawn (foreign backend / non-systemd box).
    Direct,
    /// `systemd-run --user` transient unit per child.
    Systemd,
}

pub static SPAWN_MODE: LazyLock<SpawnMode> = LazyLock::new(|| {
    // same PATH probe main.rs uses for systemctl — a capability check, not a
    // reachability check: an unreachable user manager surfaces as an honest
    // spawn error on the actual start, not as a silent mode downgrade
    let systemd = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("systemd-run").is_file()))
        .unwrap_or(false);
    let mode = if systemd { SpawnMode::Systemd } else { SpawnMode::Direct };
    log::info!(
        "delegation spawn mode: {}",
        match mode {
            SpawnMode::Systemd => "systemd-run --user transient units",
            SpawnMode::Direct => "direct spawn",
        }
    );
    mode
});

/// The systemd-run wrapper for one delegation child:
/// `[systemd-run, --user, --unit=<unit>, --collect, --pipe,
/// --property=KillMode=control-group, --property=<sandbox…>, --, <argv…>]`.
/// The child argv goes after `--` verbatim; `--cwd` stays an omp argument
/// (the unit's WorkingDirectory defaults to `$HOME` in the user manager —
/// landing in the plan dir is omp's job via that absolute path). `--pipe` is
/// load-bearing: the service inherits the invocation's stdio, so the rpc
/// JSONL pipes and the output ring keep working — without it stdio goes to
/// /dev/null and the journal. `--collect` garbage-collects dead units.
pub fn systemd_run_argv(unit: &str, child_argv: &[String]) -> Vec<String> {
    let mut argv = vec![
        "systemd-run".to_string(),
        "--user".to_string(),
        format!("--unit={unit}"),
        "--collect".to_string(),
        "--pipe".to_string(),
        "--property=KillMode=control-group".to_string(),
    ];
    argv.extend(DELEG_SANDBOX_PROPERTIES.iter().map(|p| format!("--property={p}")));
    argv.push("--".to_string());
    argv.extend(child_argv.iter().cloned());
    argv
}

/// The transient unit's real child pid: `systemctl --user show
/// --property=MainPID --value <unit>`, retried briefly — systemd-run can
/// return a beat before the manager has forked the payload. `None` = the
/// unit never got a process (start failed, e.g. the binary is gone).
pub fn main_pid(unit: &str) -> Option<u32> {
    let deadline = Instant::now() + MAINPID_WAIT;
    loop {
        let pid = Command::new("systemctl")
            .args(["--user", "show", "--property=MainPID", "--value", unit])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u32>())
            .and_then(|r| r.ok())
            .unwrap_or(0);
        if pid != 0 {
            return Some(pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// `systemctl --user stop <unit>` — the SIGTERM step for unit-mode children:
/// with `KillMode=control-group` it terminates the whole tree, grandchildren
/// included. false = the stop could not be issued (unit already gone).
pub fn unit_stop(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "stop", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `systemctl --user is-active <unit>`: `Some(true)` only for a genuinely
/// running unit; false for inactive/failed/garbage-collected; None when
/// systemctl itself failed (no user manager) — callers fall back to their
/// without-systemd behavior.
pub fn unit_is_active(unit: &str) -> Option<bool> {
    let out = Command::new("systemctl")
        .args(["--user", "is-active", unit])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim() == "active")
}

/// Which session the child is pinned to, as omp spells it on the argv.
/// Resume carries the FULL session-file path: omp resolves a bare id by
/// prefix against ULID session ids and errors out when nothing matches, so
/// the path form is the only honest dedicated-session mechanism.
#[derive(Debug)]
pub enum Session {
    /// `--resume <file>` — continue an existing transcript.
    Resume(PathBuf),
    /// A fresh session. omp 18.9 has no `--session <file>` anymore: a fresh
    /// session is minted INSIDE a `--session-dir`, and omp names the file
    /// itself (`<timestamp>_<uuid>.jsonl`). The carried path is therefore a
    /// PIN PLACEHOLDER — its PARENT is the session dir, the file name is
    /// advisory; callers resolve what actually landed with
    /// [`resolve_fresh_session`] and later resume that resolved path.
    Fresh(PathBuf),
}

/// The session file omp actually created for a fresh spawn: the newest
/// `*.jsonl` in the session dir whose mtime is not older than `not_before`
/// (small clock skew tolerated). `None` = omp wrote nothing (died early) —
/// the caller keeps the placeholder and logs.
pub fn resolve_fresh_session(dir: &Path, not_before: std::time::SystemTime) -> Option<PathBuf> {
    let skew = std::time::Duration::from_secs(5);
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else { continue };
        if modified + skew < not_before {
            continue;
        }
        if best.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
            best = Some((modified, p));
        }
    }
    best.map(|(_, p)| p)
}

/// `[omp, --mode, rpc] + config_args + [--resume|--session-dir, path, --cwd, cwd]`.
///
/// `config_args` is the approval-policy argv segment: the panel passes
/// [`cfg::orch_policy_args()`] (empty when the overlay file is missing —
/// never a hard error), a delegation passes `--config <overlay>`
/// unconditionally. Callers may extend the argv afterwards (the panel appends
/// the persona's `--append-system-prompt` pair).
pub fn build_argv(config_args: &[String], session: &Session, cwd: &Path) -> Vec<String> {
    let mut argv = vec![cfg::OMP_BIN.clone(), "--mode".into(), "rpc".into()];
    argv.extend(config_args.iter().cloned());
    let (flag, path) = match session {
        Session::Resume(p) => ("--resume", std::borrow::Cow::Borrowed(p)),
        Session::Fresh(p) => (
            "--session-dir",
            std::borrow::Cow::Owned(
                p.parent().map(Path::to_path_buf).unwrap_or_else(|| p.clone()),
            ),
        ),
    };
    argv.push(flag.to_string());
    argv.push(path.to_string_lossy().into_owned());
    argv.push("--cwd".into());
    argv.push(cwd.to_string_lossy().into_owned());
    argv
}

/// One inbound rpc frame from a raw JSONL line: trimmed, blank and non-JSON
/// lines dropped — the exact read semantics both drivers have always had.
pub fn parse_frame(line: &str) -> Option<Value> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

/// The wire form of one outgoing frame: compact JSON plus `'\n'`.
pub fn frame_line(frame: &Value) -> String {
    let mut line = serde_json::to_string(frame).expect("serialize frame");
    line.push('\n');
    line
}

/// Blocking write of one frame to the child's stdin (the delegation driver's
/// flavor), flushed — the host treats a flushed newline as a complete frame.
pub fn write_frame<W: std::io::Write>(stdin: &mut W, frame: &Value) -> std::io::Result<()> {
    stdin.write_all(frame_line(frame).as_bytes())?;
    stdin.flush()
}

/// Async write of one frame to the child's stdin (the panel driver's flavor),
/// flushed. Same bytes as [`write_frame`].
pub async fn write_frame_async<W: tokio::io::AsyncWrite + Unpin>(
    stdin: &mut W,
    frame: &Value,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    stdin.write_all(frame_line(frame).as_bytes()).await?;
    stdin.flush().await
}

/// Spawn the rpc child with the delegation driver's stdio shape: piped
/// stdin/stdout/stderr in `cwd` (stderr is dropped by the caller —
/// model-retry noise the ring does not carry).
pub fn spawn_sync(argv: &[String], cwd: &Path) -> std::io::Result<std::process::Child> {
    std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
}

/// Spawn the rpc child with the panel driver's stdio shape: piped
/// stdin/stdout, stderr to null, and `kill_on_drop` — a replaced [`Proc`]
/// must not leak the old child.
pub fn spawn_async(argv: &[String]) -> std::io::Result<tokio::process::Child> {
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    cmd.spawn()
}

/// One step of the frame stream.
pub enum Next {
    /// A decoded rpc frame.
    Frame(Value),
    /// The deadline passed with no (further) frame; the reader thread stays
    /// alive and later frames still arrive via [`Frames::next`].
    Timeout,
    /// The stream ended (child exit or a read error).
    Eof,
}

/// The child's stdout as a stream of decoded rpc frames. A reader thread
/// pumps lines through [`parse_frame`] and a channel, so a plain blocking
/// consumer keeps the exact no-timeout read semantics both drivers have
/// always had — the turn-timeout policy stays with the caller (the
/// delegation watchdog SIGTERMs the child, which ends the stream).
pub struct Frames {
    rx: mpsc::Receiver<Value>,
}

impl Frames {
    /// Start pumping a piped stdout. Ends (channel closes) on EOF or a read
    /// error; blank and non-JSON lines are dropped, never end the stream.
    pub fn spawn<R: Read + Send + 'static>(stdout: R) -> Frames {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Some(frame) = parse_frame(&line) else {
                    continue;
                };
                if tx.send(frame).is_err() {
                    break; // consumer gone
                }
            }
        });
        Frames { rx }
    }

    /// The next frame if it arrives before `deadline` — the only read both
    /// drivers use (the delegation turn loop caps each wait at its tail
    /// cadence; the handshake caps at readiness).
    pub fn next_within(&self, deadline: Instant) -> Next {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Next::Timeout;
            }
            match self.rx.recv_timeout(deadline - now) {
                Ok(frame) => return Next::Frame(frame),
                Err(mpsc::RecvTimeoutError::Timeout) => return Next::Timeout,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Next::Eof,
            }
        }
    }
}

/// The graceful stop of a delegation's rpc child: SIGTERM, wait `grace`, then
/// SIGKILL if it is still alive. Blocking for `grace` — callers that must not
/// block wrap this in a blocking pool task (the cancel path does).
pub fn kill_graceful(pid: u32, alive: impl FnOnce() -> bool, grace: Duration) {
    escalate(|| {
        runner::sigterm(pid);
    }, pid, alive, grace);
}

/// The same escalation, unit mode: `systemctl --user stop <unit>` is the
/// SIGTERM step — cgroup-correct, the whole worker tree dies, grandchildren
/// included — then the same `grace` wait and bare-pid SIGKILL backstop.
pub fn stop_unit_graceful(unit: &str, pid: u32, alive: impl FnOnce() -> bool, grace: Duration) {
    escalate(|| {
        unit_stop(unit);
    }, pid, alive, grace);
}

fn escalate(term: impl FnOnce(), pid: u32, alive: impl FnOnce() -> bool, grace: Duration) {
    term();
    std::thread::sleep(grace);
    if alive() {
        runner::sigkill(pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The argv shape is the wire contract with omp: mode first, then the
    /// config segment, then the full-path session pin, then the cwd.
    #[test]
    fn argv_forms_resume_fresh_and_config() {
        let overlay = vec!["--config".to_string(), "/repo/orchestrator-approval.yml".into()];
        let argv = build_argv(
            &overlay,
            &Session::Resume(PathBuf::from("/d/proj.jsonl")),
            Path::new("/d/proj"),
        );
        assert_eq!(
            argv,
            vec![
                cfg::OMP_BIN.clone(),
                "--mode".to_string(),
                "rpc".to_string(),
                "--config".to_string(),
                "/repo/orchestrator-approval.yml".to_string(),
                "--resume".to_string(),
                "/d/proj.jsonl".to_string(),
                "--cwd".to_string(),
                "/d/proj".to_string(),
            ]
        );

        // the panel's form: possibly-empty config args, a dedicated fresh
        // session pin (--session-dir = the pin's PARENT, omp 18.9 mints the
        // file itself), and the caller may append more afterwards
        let mut argv = build_argv(&[], &Session::Fresh(PathBuf::from("/d/orch.jsonl")), Path::new("/repo"));
        argv.extend(["--append-system-prompt".to_string(), "/repo/orchestrator.md".into()]);
        assert_eq!(&argv[1..4], &["--mode".to_string(), "rpc".to_string(), "--session-dir".to_string()]);
        assert_eq!(&argv[4..6], &["/d".to_string(), "--cwd".to_string()]);
        assert_eq!(&argv[6..8], &["/repo".to_string(), "--append-system-prompt".to_string()]);
        assert_eq!(&argv[8..], &["/repo/orchestrator.md".to_string()]);
    }

    /// Fresh-session resolution: the newest jsonl at/after the watermark
    /// wins; other extensions and missing dirs are ignored, a future
    /// watermark (nothing new yet) finds nothing.
    #[test]
    fn resolve_fresh_session_picks_newest_at_or_after_watermark() {
        let dir = std::env::temp_dir().join(format!("omp-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let watermark = std::time::SystemTime::now();
        assert!(
            resolve_fresh_session(&dir, watermark).is_none(),
            "nothing new yet"
        );
        let old = dir.join("old.jsonl");
        std::fs::write(&old, "{}").unwrap();
        let new = dir.join("new.jsonl");
        std::fs::write(&new, "{}").unwrap();
        assert_eq!(resolve_fresh_session(&dir, watermark), Some(new));
        // a non-jsonl file never wins even when newest
        let txt = dir.join("newest.txt");
        std::fs::write(&txt, "x").unwrap();
        assert_ne!(resolve_fresh_session(&dir, watermark), Some(txt));
        // a watermark in the future (nothing landed after it) — honestly none
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        assert!(resolve_fresh_session(&dir, future).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Frame decode: blanks and garbage are skipped, whitespace tolerated.
    #[test]
    fn parse_frame_skips_blank_and_bad_lines() {
        assert_eq!(parse_frame("  {\"type\":\"ready\"}  "), Some(json!({"type": "ready"})));
        assert_eq!(parse_frame(""), None);
        assert_eq!(parse_frame("   "), None);
        assert_eq!(parse_frame("{not json"), None);
    }

    /// The wire form: compact JSON, exactly one trailing newline.
    #[test]
    fn frame_line_is_compact_json_plus_newline() {
        assert_eq!(frame_line(&json!({"type": "abort"})), "{\"type\":\"abort\"}\n");
    }

    /// The systemd-run wrapper: fixed head, sandbox properties, then the
    /// child argv VERBATIM after `--` — the omp argv must reach the unit
    /// untouched (the session/cwd pins live inside it).
    #[test]
    fn systemd_run_argv_wraps_the_child_verbatim() {
        let child = vec![
            "omp".to_string(),
            "--mode".to_string(),
            "rpc".to_string(),
            "--config".to_string(),
            "/repo/orchestrator-approval.yml".to_string(),
            "--resume".to_string(),
            "/d/p.jsonl".to_string(),
            "--cwd".to_string(),
            "/d".to_string(),
        ];
        let argv = systemd_run_argv("atlas-deleg-7-ab12", &child);
        assert_eq!(
            &argv[..6],
            &[
                "systemd-run".to_string(),
                "--user".to_string(),
                "--unit=atlas-deleg-7-ab12".to_string(),
                "--collect".to_string(),
                "--pipe".to_string(),
                "--property=KillMode=control-group".to_string(),
            ]
        );
        assert_eq!(argv[6], "--property=PrivateTmp=yes");
        assert_eq!(&argv[7], "--");
        assert_eq!(&argv[8..], &child[..]);
    }

    /// §7 starts minimal: exactly one property, one that cannot break a
    /// normal worker. Widening this list is a deliberate follow-up.
    #[test]
    fn sandbox_property_list_is_minimal() {
        assert_eq!(DELEG_SANDBOX_PROPERTIES, &["PrivateTmp=yes"]);
    }
}
