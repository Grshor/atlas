//! Shared per-run child-process machinery of the in-process runners: the
//! per-project `make <target>` runner (`tasks.rs`) and the board delegation
//! runner (`delegate.rs`).
//!
//! A bounded output ring, a run handle carrying exit/ended bookkeeping, merged
//! stdout+stderr line readers and SIGTERM/SIGKILL helpers. Both callers keep
//! their own registry (keyed by project name resp. board item id); the run
//! state itself lives in the server process, so a run dies with it — both
//! runners accept that (see `tasks.rs`).

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

/// Output lines one run keeps before the oldest are dropped.
pub const RING_CAP: usize = 400;

static NEXT_ID: AtomicI64 = AtomicI64::new(1);

/// Monotonic per-process run id (shared by every runner, so ids never collide).
pub fn next_id() -> i64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Lock helper: a poisoned lock (a panicked reader thread) must not lose the
/// run state, so every lock recovers the guard instead of unwrapping.
pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Bounded line ring: the oldest lines drop once [`RING_CAP`] is reached, and
/// the first emitted line then marks the truncation.
pub struct Ring {
    lines: VecDeque<String>,
    truncated: bool,
}

impl Ring {
    pub fn new() -> Ring {
        Ring {
            lines: VecDeque::new(),
            truncated: false,
        }
    }

    pub fn push(&mut self, line: String) {
        if self.lines.len() == RING_CAP {
            self.lines.pop_front();
            self.truncated = true;
        }
        self.lines.push_back(line);
    }

    pub fn json(&self) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::with_capacity(self.lines.len() + 1);
        if self.truncated {
            out.push(json!(format!(
                "… (обрезано, показаны последние {RING_CAP} строк)"
            )));
        }
        out.extend(self.lines.iter().cloned().map(Value::String));
        out
    }
}

/// One live or finished run. `exit: None` = still running.
pub struct Run {
    pub id: i64,
    /// Child pid; `0` for synthetic runs that own no process (the orchestrator
    /// delegation watches an rpc turn instead).
    pub pid: u32,
    pub started_at: f64,
    exit: Mutex<Option<i32>>,
    ended_at: Mutex<Option<f64>>,
    lines: Mutex<Ring>,
}

impl Run {
    /// A run with a pre-reserved id: the delegation start row is written
    /// BEFORE the child spawns, so the id the row carries must be the id the
    /// later `Run` gets — both spawn paths (`spawn_with_id`, the unit twin)
    /// hand in the id; there is no self-minting constructor.
    pub fn with_id(id: i64, pid: u32) -> Arc<Run> {
        Arc::new(Run {
            id,
            pid,
            started_at: crate::util::now(),
            exit: Mutex::new(None),
            ended_at: Mutex::new(None),
            lines: Mutex::new(Ring::new()),
        })
    }

    pub fn active(&self) -> bool {
        lock(&self.exit).is_none()
    }

    pub fn exit(&self) -> Option<i32> {
        *lock(&self.exit)
    }

    pub fn ended_at(&self) -> Option<f64> {
        *lock(&self.ended_at)
    }

    /// Record the end of the run. Idempotent: true only for the caller that
    /// actually closed it (so a cancel and the exit watcher never both fire).
    pub fn finish(&self, code: i32) -> bool {
        let mut g = lock(&self.exit);
        if g.is_some() {
            return false;
        }
        *g = Some(code);
        *lock(&self.ended_at) = Some(crate::util::now());
        true
    }

    pub fn push_line(&self, line: String) {
        lock(&self.lines).push(line);
    }

    pub fn lines_json(&self) -> Vec<Value> {
        lock(&self.lines).json()
    }

    /// Snapshot of the LAST `n` lines, oldest first — the evidence a terminal
    /// run persists into the DB (the in-memory ring dies with the server).
    pub fn tail_lines(&self, n: usize) -> Vec<String> {
        let g = lock(&self.lines);
        g.lines.iter().rev().take(n).rev().cloned().collect()
    }

    pub fn truncated(&self) -> bool {
        lock(&self.lines).truncated
    }

    /// Seconds from start to the end (or to now while still running).
    pub fn duration_s(&self) -> f64 {
        let end = self.ended_at().unwrap_or_else(crate::util::now);
        (end - self.started_at).max(0.0)
    }
}

/// Drain `reader` line by line into the run's ring (one detached thread).
pub fn spawn_reader<R: std::io::Read + Send + 'static>(reader: R, run: Arc<Run>) {
    std::thread::spawn(move || {
        let mut br = std::io::BufReader::new(reader);
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            match br.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    let mut line = String::from_utf8_lossy(&buf).into_owned();
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    run.push_line(line);
                }
                Err(_) => break,
            }
        }
    });
}

/// Spawn `argv` in `dir` — argv array, no shell — with stdout+stderr merged
/// into the returned run's ring. `on_exit(code)` runs once on the waiter
/// thread after the child is reaped (and after the run is marked finished).
pub fn spawn(
    argv: &[String],
    dir: &std::path::Path,
    on_exit: impl FnOnce(i32) + Send + 'static,
) -> std::io::Result<Arc<Run>> {
    spawn_with_id(next_id(), argv, dir, on_exit)
}

/// [`spawn`] with a pre-reserved run id: the delegation start row is written
/// before the child exists, so the id the row carries must be the id the run
/// reports (tasks runners don't care and use [`spawn`]).
pub fn spawn_with_id(
    id: i64,
    argv: &[String],
    dir: &std::path::Path,
    on_exit: impl FnOnce(i32) + Send + 'static,
) -> std::io::Result<Arc<Run>> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let run = Run::with_id(id, child.id());
    if let Some(out) = child.stdout.take() {
        spawn_reader(out, Arc::clone(&run));
    }
    if let Some(err) = child.stderr.take() {
        spawn_reader(err, Arc::clone(&run));
    }
    let monitored = Arc::clone(&run);
    std::thread::spawn(move || {
        let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        monitored.finish(code);
        on_exit(code);
    });
    Ok(run)
}

/// SIGTERM one pid. false = the kill binary failed (e.g. the process is gone).
pub fn sigterm(pid: u32) -> bool {
    signal("-TERM", pid)
}

/// SIGKILL one pid.
pub fn sigkill(pid: u32) -> bool {
    signal("-KILL", pid)
}

/// `/bin/kill`, not std's `Child::kill`: the runners ask for SIGTERM and std
/// would send SIGKILL.
fn signal(flag: &str, pid: u32) -> bool {
    Command::new("kill")
        .arg(flag)
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run adopted via `with_id` keeps the pre-reserved id (the delegation
    /// start row is written before the child exists) and `finish` stays
    /// idempotent: only the first closer wins — the exact property the idle
    /// watchdog and the frame reader arbitrate on.
    #[test]
    fn with_id_keeps_reserved_id_and_finish_picks_one_owner() {
        let run = Run::with_id(4242, 0);
        assert_eq!(run.id, 4242);
        assert!(run.active());
        assert!(run.finish(0), "first finish wins");
        assert!(!run.finish(-1), "second finish is a no-op");
        assert_eq!(run.exit(), Some(0));
    }

    /// `tail_lines` is the LAST n lines, oldest first, and never panics on an
    /// empty or short ring — it is what a terminal run persists into the DB.
    #[test]
    fn tail_lines_takes_the_last_n_oldest_first() {
        let run = Run::with_id(next_id(), 0);
        assert!(run.tail_lines(3).is_empty());
        for i in 0..5 {
            run.push_line(format!("line {i}"));
        }
        assert_eq!(run.tail_lines(3), vec!["line 2", "line 3", "line 4"]);
        assert_eq!(run.tail_lines(10).len(), 5, "short ring is returned whole");
    }
}
