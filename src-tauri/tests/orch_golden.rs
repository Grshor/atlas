//! Golden probe — the REAL `omp` binary in rpc mode against the REAL approval
//! overlay (`orchestrator-approval.yml`). FRAGILE.md §6: unit-test models of
//! omp's matcher drifted from reality twice, so this test stops modelling and
//! asks the actual child.
//!
//! One `#[ignore]`d test spawns `omp --mode rpc` three times (same argv as
//! `src-tauri/src/delegate.rs::start_orchestrator`: `--config <overlay>
//! --session <file> --cwd <dir>`), speaks the same newline-delimited JSON
//! frames over stdin/stdout, and asserts what actually comes back:
//!
//!   1. `echo atlas-golden-ok`          → runs free: NO approval-request frame.
//!   2. `rm -rf /tmp/atlas-golden-x`    → blocked by the `*rm -rf /*` deny rule;
//!                                        the tool call ends in error naming the
//!                                        pattern (or "Blocked"), no card shown.
//!   3. `dd if=/dev/zero of=/tmp/…`     → the `dd*` prompt rule raises the
//!                                        `extension_ui_request`/`select`
//!                                        approval gate; we answer
//!                                        `{"type":"extension_ui_response",
//!                                        "id":…,"value":"Deny"}` and the turn
//!                                        ends with the file NOT created.
//!
//! Frame shapes mirrored from delegate.rs/orch.rs and confirmed against the
//! live child (2026-10-09): we send `{"type":"prompt","message":…}`; the child
//! answers `{"type":"ready","protocolVersion":1,…}`, `agent_start`,
//! `message_update`/`message_start`/`message_end`, `turn_start`/`turn_end`,
//! `tool_execution_start` (`toolCallId`, `toolName`, `args`),
//! `tool_execution_end` (`toolCallId`, `isError`, `result.content[0].text` —
//! NO `args`), `response` (`command:"prompt"`, `success:true` ack),
//! `extension_ui_request` — both the approval gate (`method:"select"`,
//! options `["Approve","Deny"]`) and non-gate widgets (`method:"setWidget"`,
//! ignored here) — and a terminal `agent_end`.
//!
//! Run explicitly (spawns real processes, needs the model backend up):
//! ```text
//! cargo test --test orch_golden -- --ignored --nocapture
//! ```
//! Plain `cargo test` compiles this target but skips it. If `omp` is not on
//! PATH (~/.local/bin/omp first, then PATH — mirroring `cfg::OMP_BIN`), the
//! test eprints a skip reason and passes as skipped.

use parking_lot::{Condvar, Mutex};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Per-frame read timeout: no new frame for this long fails the probe.
const FRAME_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard cap per probe (spawn → terminal frame).
const PROBE_CAP: Duration = Duration::from_secs(120);

// --------------------------------------------------------------- overlay ----

fn overlay_path() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("src-tauri has a parent (repo root)")
        .join("orchestrator-approval.yml");
    assert!(p.is_file(), "approval overlay not found at {}", p.display());
    p
}

/// The `approval:` value of the first rule whose `match:` pattern equals
/// `pattern` (substring-level, mirroring how the YAML itself is written).
fn rule_approval(yml: &str, pattern: &str) -> Option<String> {
    let needle = format!("match: \"{pattern}\"");
    let pos = yml.find(&needle)?;
    let rest = &yml[pos + needle.len()..];
    let end = rest.find("\n    - match:").unwrap_or(rest.len());
    rest[..end].lines().find_map(|l| {
        let l = l.trim();
        l.strip_prefix("approval:").map(|v| v.trim().to_string())
    })
}

/// Plain substring assertions on the overlay itself (cheap, run even before
/// any process is spawned).
fn assert_overlay_rules(overlay: &Path) {
    let yml = fs::read_to_string(overlay).expect("read orchestrator-approval.yml");
    assert!(
        yml.contains("allowCompoundCommands: true"),
        "overlay must keep `allowCompoundCommands: true`"
    );
    assert_eq!(
        rule_approval(&yml, "*rm -rf /*").as_deref(),
        Some("deny"),
        "overlay must DENY `rm -rf` on absolute paths (pattern `*rm -rf /*`)"
    );
    assert_eq!(
        rule_approval(&yml, "dd*").as_deref(),
        Some("prompt"),
        "overlay must PROMPT for `dd*`"
    );
}

// ------------------------------------------------------------------ omp -----

fn is_executable(p: &Path) -> bool {
    p.is_file()
        && fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// Mirror `cfg::OMP_BIN`: `~/.local/bin/omp` first, then PATH. `None` → skip.
fn find_omp() -> Option<String> {
    if let Some(home) = std::env::var_os("HOME") {
        let local = PathBuf::from(home).join(".local").join("bin").join("omp");
        if is_executable(&local) {
            return Some(local.to_string_lossy().into_owned());
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join("omp");
            if is_executable(&p) {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    None
}

// ---------------------------------------------------------------- frames ----

fn type_of(ev: &Value) -> &str {
    ev.get("type").and_then(Value::as_str).unwrap_or("")
}

/// omp's approval gate: `uiContext.select(prompt, ["Approve","Deny"])` —
/// `extension_ui_request` with `method:"select"` and an "Approve" option.
fn is_approval_request(ev: &Value) -> bool {
    type_of(ev) == "extension_ui_request"
        && ev.get("method").and_then(Value::as_str) == Some("select")
        && ev
            .get("options")
            .and_then(Value::as_array)
            .map(|o| o.iter().any(|v| v.as_str() == Some("Approve")))
            .unwrap_or(false)
}

fn frame_command(ev: &Value) -> String {
    ev.pointer("/args/command")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            ev.get("args")
                .map(|a| a.to_string())
                .unwrap_or_default()
        })
}

fn end_is_error(ev: &Value) -> bool {
    ev.get("isError").and_then(Value::as_bool).unwrap_or(false)
}

fn end_text(ev: &Value) -> String {
    ev.pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// A frame after which the turn is over (mirrors orch.rs's terminal mapping).
fn is_terminal_frame(f: &Value) -> bool {
    match type_of(f) {
        "agent_end" => f.get("isTerminal").and_then(Value::as_bool).unwrap_or(true),
        "response" => f.get("success").and_then(Value::as_bool) == Some(false),
        "prompt_result" => f.get("agentInvoked").and_then(Value::as_bool) == Some(false),
        _ => false,
    }
}

// ----------------------------------------------------------------- probe ----

#[derive(Default)]
struct Shared {
    raw: Vec<String>,
    events: Vec<Value>,
    done: bool,
    eof: bool,
}

/// One live rpc child + its frame log. Kills the child and removes the temp
/// dir on drop.
struct Probe {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    stderr_tail: Arc<Mutex<String>>,
    dir: PathBuf,
    pid: u32,
}

impl Probe {
    /// Same argv as `delegate.rs::start_orchestrator` (fresh session, no
    /// `--resume`, no `--append-system-prompt` — our instruction is the prompt
    /// frame), piped stdin/stdout/stderr.
    fn spawn(omp: &str, overlay: &Path, dir: &Path) -> std::io::Result<Probe> {
        fs::create_dir_all(dir.join("cwd"))?;
        let session = dir.join("golden-session.jsonl");
        let mut child = Command::new(omp)
            .args(["--mode", "rpc"])
            .args(["--config"])
            .arg(overlay)
            .args(["--session"])
            .arg(&session)
            .args(["--cwd"])
            .arg(dir.join("cwd"))
            .current_dir(dir.join("cwd"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let pid = child.id();
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let stderr_tail: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        {
            let tail = Arc::clone(&stderr_tail);
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut t = tail.lock();
                    t.push_str(&line);
                    t.push('\n');
                    let len = t.len();
                    if len > 4000 {
                        t.drain(..len - 4000);
                    }
                }
            });
        }

        let shared: Arc<(Mutex<Shared>, Condvar)> =
            Arc::new((Mutex::new(Shared::default()), Condvar::new()));
        {
            let sh = Arc::clone(&shared);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let mut g = sh.0.lock();
                    g.raw.push(line.trim_end_matches(&['\n', '\r'][..]).to_string());
                    if let Ok(f) = serde_json::from_str::<Value>(&line) {
                        if is_terminal_frame(&f) {
                            g.done = true;
                        }
                        g.events.push(f);
                    }
                    sh.1.notify_all();
                    line.clear();
                }
                let mut g = sh.0.lock();
                g.eof = true;
                sh.1.notify_all();
            });
        }

        Ok(Probe {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            shared,
            stderr_tail,
            dir: dir.to_path_buf(),
            pid,
        })
    }

    fn send(&self, frame: &Value) {
        let mut w = self.stdin.lock();
        let _ = writeln!(w, "{frame}");
        let _ = w.flush();
    }

    fn events_snapshot(&self) -> Vec<Value> {
        self.shared.0.lock().events.clone()
    }

    fn wait_any_frame(&self, timeout: Duration) -> bool {
        let (lock, cvar) = &*self.shared;
        let deadline = Instant::now() + timeout;
        let mut g = lock.lock();
        while g.raw.is_empty() {
            if Instant::now() >= deadline {
                return false;
            }
            cvar.wait_for(&mut g, Duration::from_millis(100));
        }
        true
    }

    /// Wait until the turn is terminal, a NEW frame (index > `seen`) arrives,
    /// or the deadline passes. The drain loop between waits must answer
    /// approval gates while the turn is still open, so it has to wake on every
    /// push (measured 09.10.2026: the `dd*` gate arrives ~30 s before the cap —
    /// a wait that ignores pushes leaves omp's `select` unanswered and the
    /// whole turn wedged).
    fn wait_progress(&self, seen: usize, deadline: Instant) -> bool {
        let (lock, cvar) = &*self.shared;
        let mut g = lock.lock();
        loop {
            if g.done {
                return true;
            }
            if g.events.len() > seen {
                return false;
            }
            if Instant::now() >= deadline {
                return false;
            }
            cvar.wait_for(&mut g, Duration::from_millis(100));
        }
    }

    fn is_eof(&self) -> bool {
        self.shared.0.lock().eof
    }

    /// Every received line (capped), for debugging a mismatch.
    fn dump(&self) -> String {
        let g = self.shared.0.lock();
        let mut out = format!("--- {} raw frames received ---\n", g.raw.len());
        let n = g.raw.len();
        let mut push = |out: &mut String, l: &str| {
            let mut l = l.to_string();
            if l.chars().count() > 300 {
                l = l.chars().take(300).collect::<String>() + "…";
            }
            out.push_str("    ");
            out.push_str(&l);
            out.push('\n');
        };
        let head = n.min(40);
        for l in g.raw.iter().take(head) {
            push(&mut out, l);
        }
        if n > head {
            if n > 100 {
                out.push_str(&format!("    … {} frames elided …\n", n - head - 60));
            }
            let skip = if n > 100 { n - 60 } else { head };
            for l in g.raw.iter().skip(skip) {
                push(&mut out, l);
            }
        }
        let stderr = self.stderr_tail.lock();
        if !stderr.is_empty() {
            out.push_str("--- stderr tail ---\n");
            out.push_str(&stderr);
        }
        out
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let mut c = self.child.lock();
        let _ = c.kill();
        let _ = c.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

// -------------------------------------------------------------- run turn ----

/// Everything one turn produced.
struct Turn {
    events: Vec<Value>,
    /// Approval-gate ids we answered with Deny (non-empty only when
    /// `auto_deny`).
    answered: Vec<String>,
    dump: String,
}

/// Spawn a fresh rpc child on the overlay, run ONE prompt turn to its terminal
/// frame, return every event. With `auto_deny`, each approval gate is answered
/// `Deny` as it arrives. Panics (with the full frame dump) on timeout, hang or
/// premature child exit.
fn run_turn(tag: &str, omp: &str, overlay: &Path, instruction: &str, auto_deny: bool) -> Turn {
    let started = Instant::now();
    let dir = std::env::temp_dir().join(format!("atlas-golden-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let probe = Probe::spawn(omp, overlay, &dir).unwrap_or_else(|e| {
        panic!("[{tag}] failed to spawn omp rpc: {e}");
    });
    println!(
        "[{tag}] omp pid {} · cwd {} · overlay {}",
        probe.pid,
        dir.join("cwd").display(),
        overlay.display()
    );

    // Handshake: omp announces {"type":"ready",…}; delegate.rs merely sleeps
    // ~1s and sends. Wait up to 10s for the first frame, then send regardless
    // so a silent handshake cannot wedge the probe.
    probe.wait_any_frame(Duration::from_secs(10));
    probe.send(&json!({"type": "prompt", "message": instruction}));

    let cap = started + PROBE_CAP;
    let mut seen = 0usize;
    let mut answered: Vec<String> = Vec::new();
    let mut last_progress = Instant::now();
    loop {
        // Drain everything new first; an approval gate must be answered while
        // the turn is open (omp's select blocks the tool call until answered).
        let events = probe.events_snapshot();
        let mut drained = false;
        while seen < events.len() {
            let ev = &events[seen];
            seen += 1;
            drained = true;
            last_progress = Instant::now();
            if let Some(id) = ev.get("id").and_then(Value::as_str) {
                println!("[{tag}]   frame {} (id {id})", type_of(ev));
            } else {
                println!("[{tag}]   frame {}", type_of(ev));
            }
            if auto_deny && is_approval_request(ev) {
                let id = ev.get("id").and_then(Value::as_str).unwrap_or_default();
                let title = ev.get("title").and_then(Value::as_str).unwrap_or("");
                println!("[{tag}]   approval gate {title:?} → answering Deny");
                probe.send(&json!({"type": "extension_ui_response", "id": id, "value": "Deny"}));
                answered.push(id.to_string());
            }
        }
        if drained {
            continue;
        }
        if probe.wait_progress(seen, cap) {
            break;
        }
        if probe.is_eof() {
            panic!(
                "[{tag}] rpc child exited before a terminal frame (answered {answered:?});\n{}",
                probe.dump()
            );
        }
        if Instant::now() >= cap {
            panic!(
                "[{tag}] hard cap {PROBE_CAP:?} hit without a terminal frame (answered {answered:?});\n{}",
                probe.dump()
            );
        }
        if last_progress.elapsed() > FRAME_TIMEOUT {
            panic!(
                "[{tag}] no frame for {FRAME_TIMEOUT:?} — turn hung (answered {answered:?});\n{}",
                probe.dump()
            );
        }
    }
    println!(
        "[{tag}] turn ended in {:?}, {} frames",
        started.elapsed(),
        probe.shared.0.lock().raw.len()
    );
    Turn {
        events: probe.events_snapshot(),
        answered,
        dump: probe.dump(),
    }
}

// ----------------------------------------------------------------- probes ----

/// Classification of one turn's events. Reality checked against the live rpc
/// child: `tool_execution_start` carries `args` (+ `toolCallId`), while
/// `tool_execution_end` carries NO `args` — start/end are paired by
/// `toolCallId`.
struct TurnEvents {
    approvals: Vec<String>,
    /// (toolCallId, command)
    starts: Vec<(String, String)>,
    /// (toolCallId, isError, result text)
    ends: Vec<(String, bool, String)>,
}

fn collect(turn: &Turn) -> TurnEvents {
    let mut ev = TurnEvents {
        approvals: Vec::new(),
        starts: Vec::new(),
        ends: Vec::new(),
    };
    for e in &turn.events {
        match type_of(e) {
            "extension_ui_request" if is_approval_request(e) => ev.approvals.push(
                e.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
            ),
            "tool_execution_start" => ev.starts.push((
                e.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string(),
                frame_command(e),
            )),
            "tool_execution_end" => ev.ends.push((
                e.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string(),
                end_is_error(e),
                end_text(e),
            )),
            _ => {}
        }
    }
    ev
}

/// The `toolCallId` of the start whose command contains `needle`.
fn start_id(starts: &[(String, String)], needle: &str) -> Option<String> {
    starts
        .iter()
        .find(|(_, cmd)| cmd.contains(needle))
        .map(|(id, _)| id.clone())
}

fn probe_echo_runs_free(omp: &str, overlay: &Path) {
    let turn = run_turn(
        "echo",
        omp,
        overlay,
        "Autonomous golden probe. Your ONLY action this turn: run the bash command \
         `echo atlas-golden-ok`. Run nothing else. Then reply with that command's output \
         and end your turn.",
        false,
    );
    let ev = collect(&turn);

    assert!(
        ev.approvals.is_empty(),
        "probe 1: `echo atlas-golden-ok` must run free under the overlay — \
         approval-request frame(s) arrived: {:?}\n{}",
        ev.approvals,
        turn.dump
    );
    let sid = start_id(&ev.starts, "echo atlas-golden-ok");
    assert!(
        sid.is_some(),
        "probe 1: no tool_execution_start for the echo; starts: {:?}\n{}",
        ev.starts,
        turn.dump
    );
    let sid = sid.unwrap();
    let executed = ev.ends.iter().any(|(id, is_err, text)| {
        *id == sid && !*is_err && text.contains("atlas-golden-ok")
    });
    assert!(
        executed,
        "probe 1: echo did not execute cleanly; starts: {:?}; ends: {:?}\n{}",
        ev.starts,
        ev.ends,
        turn.dump
    );
    println!("[golden] probe 1/3 PASS — echo ran free, no approval-request frame, output observed");
}

fn probe_rm_rf_blocked(omp: &str, overlay: &Path) {
    let turn = run_turn(
        "rmrf",
        omp,
        overlay,
        "Autonomous golden probe. Your ONLY action this turn: run the bash command \
         `rm -rf /tmp/atlas-golden-x`. Run nothing else. Then report exactly what the \
         command returned and end your turn.",
        false,
    );
    let ev = collect(&turn);

    assert!(
        ev.approvals.is_empty(),
        "probe 2: `rm -rf /tmp/…` is a deny rule — no approval card may be shown: {:?}\n{}",
        ev.approvals,
        turn.dump
    );
    let sid = start_id(&ev.starts, "rm -rf /tmp/atlas-golden-x");
    assert!(
        sid.is_some(),
        "probe 2: the model never attempted the rm -rf call; starts: {:?}\n{}",
        ev.starts,
        turn.dump
    );
    let sid = sid.unwrap();
    let rm_ends: Vec<&(String, bool, String)> =
        ev.ends.iter().filter(|(id, _, _)| *id == sid).collect();
    assert!(
        !rm_ends.is_empty(),
        "probe 2: no tool_execution_end for the rm -rf call; ends: {:?}\n{}",
        ev.ends,
        turn.dump
    );
    assert!(
        rm_ends.iter().all(|(_, is_err, _)| *is_err),
        "probe 2: rm -rf was NOT blocked — a call ended without error: {rm_ends:?}\n{}",
        turn.dump
    );
    let block_text = rm_ends
        .iter()
        .map(|(_, _, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        block_text.contains("rm -rf") || block_text.to_lowercase().contains("blocked"),
        "probe 2: block message should name the pattern or say \"Blocked\"; got: {block_text:?}\n{}",
        turn.dump
    );
    println!(
        "[golden] probe 2/3 PASS — rm -rf blocked by the overlay deny rule: {:?}",
        block_text.lines().next().unwrap_or("")
    );
}

fn probe_dd_prompt_denied(omp: &str, overlay: &Path) {
    // A stale file from an earlier run must not fake a pass/fail either way.
    let dd_file = PathBuf::from("/tmp/atlas-golden-dd");
    let _ = fs::remove_file(&dd_file);

    let turn = run_turn(
        "dd",
        omp,
        overlay,
        "Autonomous golden probe. Your ONLY action this turn: run the bash command \
         `dd if=/dev/zero of=/tmp/atlas-golden-dd bs=1 count=1`. Run nothing else and do \
         not create /tmp/atlas-golden-dd by any other means. Then report exactly what the \
         command returned and end your turn.",
        true, // answer every approval gate with Deny
    );
    let ev = collect(&turn);

    assert_eq!(
        ev.approvals.len(),
        1,
        "probe 3: `dd … of=/tmp/…` must raise exactly one approval gate (prompt rule `dd*`); \
         answered {:?}, gates: {:?}\n{}",
        turn.answered,
        ev.approvals,
        turn.dump
    );
    assert_eq!(
        turn.answered.len(),
        1,
        "probe 3: exactly one Deny answer should have been sent\n{}",
        turn.dump
    );
    assert!(
        ev.approvals.iter().any(|t| t.contains("dd")),
        "probe 3: the approval-gate title should name the dd command; titles: {:?}\n{}",
        ev.approvals,
        turn.dump
    );
    let sid = start_id(&ev.starts, "dd if=/dev/zero");
    assert!(
        sid.is_some(),
        "probe 3: no tool_execution_start for the dd call; starts: {:?}\n{}",
        ev.starts,
        turn.dump
    );
    let sid = sid.unwrap();
    // A denied call may abort without any end frame at all — but every end it
    // does produce must be an error, and the file must not exist.
    assert!(
        ev.ends
            .iter()
            .all(|(id, is_err, _)| *id != sid || *is_err),
        "probe 3: the dd call succeeded after a Deny answer — the deny frame did not hold: {:?}\n{}",
        ev.ends,
        turn.dump
    );
    assert!(
        !dd_file.exists(),
        "probe 3: /tmp/atlas-golden-dd exists after the Deny answer — the command executed"
    );
    println!("[golden] probe 3/3 PASS — dd* raised the approval card, Deny held, file not created");
}

// ------------------------------------------------------------------ test ----

#[test]
#[ignore = "spawns the real omp rpc binary against the real overlay; run with \
           cargo test --test orch_golden -- --ignored --nocapture"]
fn golden_probes() {
    let overlay = overlay_path();
    assert_overlay_rules(&overlay);

    let Some(omp) = find_omp() else {
        eprintln!(
            "SKIP golden probes: `omp` binary not found (~/.local/bin/omp or PATH) — \
             the probes need the real binary"
        );
        return;
    };
    println!("[golden] omp = {omp}");

    probe_echo_runs_free(&omp, &overlay);
    probe_rm_rf_blocked(&omp, &overlay);
    probe_dd_prompt_denied(&omp, &overlay);
    println!("[golden] all three probes passed");
}
