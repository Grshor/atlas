//! Persistent `omp --mode rpc` orchestrator: one child per server process,
//! lazily (re)started. Raw rpc frames are mapped onto the canonical UI event
//! shapes broadcast to the Оркестратор panel:
//!
//! ```text
//! {"type":"turn_start","prompt":"…"}
//! {"type":"text","delta":"…"}      {"type":"thinking","delta":"…"}
//! {"type":"tool","name":"bash","detail":"…","phase":"start"|"end"}
//! {"type":"approval_request","id":"…","tool":"bash","detail":"…"}
//! {"type":"approval_resolved","id":"…","allow":true}
//! {"type":"done","duration_s":12.3}          {"type":"error","message":"…"}
//! ```
//!
//! Mapping (rpc frame → canonical event), see omp://rpc.md:
//!   * our own `{"type":"prompt"}` send      → `turn_start`
//!   * `message_update` + `assistantMessageEvent.type=="text_delta"` → `text`
//!   * `message_update` + `assistantMessageEvent.type=="thinking_delta"` → `thinking`
//!   * `tool_execution_start`/`tool_execution_end` → `tool` (phase start/end)
//!   * `extension_ui_request` `method:"select"` with an `"Approve"` option →
//!     `approval_request` (omp's approval gate is `uiContext.select(prompt,
//!     ["Approve","Deny"])`); the response frame is
//!     `{"type":"extension_ui_response","id":…,"value":"Approve"|"Deny"}`
//!   * `agent_end` with `isTerminal !== false`, or `prompt_result` with
//!     `agentInvoked:false` → `done`
//!   * a `{"type":"abort"}` cancel → `done` with `"cancelled":true`
//!   * `response` with `success:false` → `error`
//!
//! The last [`HISTORY_CAP`] canonical events are kept in memory and replayed by
//! `GET /api/orch/history`; after a server restart the ring is empty.

use crate::cfg;
use crate::db;
use crate::omp_rpc;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{broadcast, Mutex as AsyncMutex};

/// A turn that never terminates (hung child) is capped here.
pub const TURN_TIMEOUT_SECS: u64 = 900;
/// An approval left pending this long is auto-denied (warn notification).
pub const APPROVAL_TIMEOUT_SECS: u64 = 300;
/// In-memory event ring replayed by `GET /api/orch/history`.
pub const HISTORY_CAP: usize = 500;

/// Why an approval response could not be delivered.
#[derive(Debug)]
pub enum ApproveError {
    /// No pending approval with that id (unknown or already expired).
    Unknown,
    /// The rpc child is gone / its stdin pipe is broken.
    Channel(String),
}

/// The argv of the persistent rpc child (also the honest `cmd` in status).
///
/// The child is pinned to ONE dedicated omp session: `--session
/// <data_dir>/atlas-orchestrator.jsonl`. The first spawn creates the session
/// at that exact file (verified: `omp --session <missing path>` starts a new
/// session there, omp v18.6.1), every later spawn resumes it — with the full
/// transcript — instead of `-c`, which resumed whatever session happened to be
/// latest in the atlas cwd (once the parent conductor's own transcript). A
/// bare `--session atlas-orchestrator` id is NOT an option: omp resolves ids
/// by prefix against ULID session ids and errors out with
/// «Session "atlas-orchestrator" not found» when nothing matches, so the
/// path form is the only honest dedicated-session mechanism.
///
/// `--config <atlas>/orchestrator-approval.yml` is what makes harmless commands
/// run free. The child used to be pinned to `--approval-mode always-ask`, which
/// raised the `extension_ui_request` approval gate for EVERY tool call —
/// including `echo hi` («оркестратор постоянно спрашивает разрешение…»). The
/// inverse mode cannot express "free except risky": `write` still prompts for
/// the `exec` tier, and every shell command goes through `bash`, which is
/// `exec` (omp://approval-mode.md). So the policy lives in ONE versioned file
/// instead: the overlay sets `tools.approvalMode: yolo` and carries the curated
/// `bash.patterns` list whose `prompt` rules still ask for the destructive and
/// outward-facing classes (deny rules stay absolute in `yolo`). The overlay is
/// passed, not a mode flag, on purpose: an overlay outranks the global and
/// project configs, an RPC host re-reads overlays on change (so the file is
/// live-tunable), and a hardcoded `--approval-mode` in argv would silently
/// override any edit to the file's copy of the key.
///
/// Everything else about the child is deliberate and unchanged: the dedicated
/// `--session` (see above), `--cwd <atlas>`, the persona appended as a file,
/// the 900 s turn timeout and the approval machinery in this module (pending
/// vec, 5-min auto-deny, `POST /api/orch/approve`). Only WHAT gets asked
/// changed — an approval card now appears solely for a command that matches a
/// `prompt` rule in the overlay.
/// The panel orchestrator's omp session file, omp 18.9 edition: a fresh
/// session is minted inside `<data_dir>/orch-sessions/` (no `--session
/// <file>` anymore), and the file that actually landed is remembered HERE —
/// every later spawn resumes it by path. `None` = the first spawn has not
/// happened yet (or never wrote a file).
static ORCH_SESSION: LazyLock<parking_lot::RwLock<Option<std::path::PathBuf>>> =
    LazyLock::new(|| parking_lot::RwLock::new(None));

/// The orch sessions dir: `<data_dir>/orch-sessions` — isolated from the
/// delegation sessions (which live per-run in their dossier dirs) so the
/// post-spawn resolution cannot confuse one for the other.
fn orch_sessions_dir() -> std::path::PathBuf {
    crate::cfg::data_dir().join("orch-sessions")
}

fn spawn_argv() -> Vec<String> {
    let pinned = ORCH_SESSION.read().clone();
    let session = match pinned {
        Some(p) => omp_rpc::Session::Resume(p),
        None => {
            let dir = orch_sessions_dir();
            let _ = std::fs::create_dir_all(&dir);
            // pin placeholder: build_argv passes its PARENT as --session-dir
            omp_rpc::Session::Fresh(dir.join("atlas-orchestrator.jsonl"))
        }
    };
    let fresh = matches!(session, omp_rpc::Session::Fresh(_));
    let mut argv = omp_rpc::build_argv(
        &cfg::orch_policy_args(),
        &session,
        &cfg::atlas_dir(),
    );
    // Standing instructions live in `<atlas dir>/orchestrator.md` (falling back
    // to the built-in one-liner).
    argv.extend(cfg::orch_prompt_args());
    if fresh {
        // pin the file omp actually created: the next spawn resumes it, so the
        // orchestrator keeps its transcript across child restarts
        let dir = orch_sessions_dir();
        let started = std::time::SystemTime::now();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            match omp_rpc::resolve_fresh_session(&dir, started) {
                Some(p) => {
                    log::info!("orch: сессия оркестратора закреплена: {}", p.display());
                    *ORCH_SESSION.write() = Some(p);
                }
                None => log::warn!("orch: свежая сессия оркестратора не найдена в {}", dir.display()),
            }
        });
    }
    argv
}

struct Proc {
    #[allow(dead_code)] // held so the child stays alive; polled via try_wait
    child: Mutex<Child>,
    stdin: AsyncMutex<ChildStdin>,
    /// The argv actually used at spawn (the honest `cmd` in status).
    argv: Vec<String>,
}

impl Proc {
    fn is_alive(&self) -> bool {
        matches!(
            self.child
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .try_wait(),
            Ok(None)
        )
    }

    fn pid(&self) -> Option<u32> {
        self.child
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .id()
    }
}

/// One approval waiting for a human decision.
#[derive(Clone)]
pub struct Pending {
    pub id: String,
    pub tool: String,
    pub detail: String,
    pub ts: u64,
}

impl Pending {
    fn json(&self) -> Value {
        json!({"id": self.id, "tool": self.tool, "detail": self.detail, "ts": self.ts})
    }
}

pub struct Orchestrator {
    proc: Mutex<Option<Arc<Proc>>>,
    spawn_lock: AsyncMutex<()>,
    /// Serialises turns; `try_lock_owned` in the prompt handler → 409 when busy.
    pub gate: Arc<AsyncMutex<()>>,
    tx: broadcast::Sender<Value>,
    done: AtomicBool,
    turn_started_ms: AtomicU64,
    current_prompt: Mutex<String>,
    model: Mutex<Option<String>>,
    pending: Mutex<Vec<Pending>>,
    history: Mutex<VecDeque<Value>>,
}

impl Orchestrator {
    pub fn new() -> Arc<Orchestrator> {
        let (tx, _) = broadcast::channel(4096);
        Arc::new(Orchestrator {
            proc: Mutex::new(None),
            spawn_lock: AsyncMutex::new(()),
            gate: Arc::new(AsyncMutex::new(())),
            tx,
            done: AtomicBool::new(false),
            turn_started_ms: AtomicU64::new(0),
            current_prompt: Mutex::new(String::new()),
            model: Mutex::new(None),
            pending: Mutex::new(Vec::new()),
            history: Mutex::new(VecDeque::new()),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.tx.subscribe()
    }

    /// Broadcast one canonical event and append it to the history ring.
    fn emit(&self, ev: Value) {
        {
            let mut h = self.history.lock().unwrap_or_else(|p| p.into_inner());
            if h.len() >= HISTORY_CAP {
                h.pop_front();
            }
            h.push_back(ev.clone());
        }
        let _ = self.tx.send(ev);
    }

    /// The last [`HISTORY_CAP`] canonical events (oldest first).
    pub fn history(&self) -> Vec<Value> {
        self.history
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub fn pending(&self) -> Vec<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn current_prompt(&self) -> String {
        self.current_prompt
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn push_pending(&self, p: Pending) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(p);
    }

    fn take_pending(&self, id: &str) -> Option<Pending> {
        let mut g = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let idx = g.iter().position(|p| p.id == id)?;
        Some(g.remove(idx))
    }

    /// Remove and return every approval older than `ttl_ms` (pure, testable).
    pub fn expire_approvals(&self, now: u64, ttl_ms: u64) -> Vec<Pending> {
        let mut g = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let (expired, keep): (Vec<Pending>, Vec<Pending>) =
            g.drain(..).partition(|p| now.saturating_sub(p.ts) >= ttl_ms);
        *g = keep;
        expired
    }

    /// `GET /api/orch/status` body.
    pub fn status_json(&self) -> Value {
        let proc = self.proc.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let (alive, pid, cmd) = match &proc {
            Some(p) => {
                let alive = p.is_alive();
                let pid = if alive { p.pid() } else { None };
                (alive, pid, p.argv.clone())
            }
            None => (false, None, spawn_argv()),
        };
        let pending: Vec<Value> = self.pending().iter().map(|p| p.json()).collect();
        let model = self.model.lock().unwrap_or_else(|p| p.into_inner()).clone();
        json!({
            "alive": alive,
            "pid": pid,
            "cmd": cmd,
            "turn_active": self.gate.try_lock().is_err(),
            "pending": pending,
            "model": model,
        })
    }

    fn spawn(self: &Arc<Self>) -> Arc<Proc> {
        let argv = spawn_argv();
        let mut child = omp_rpc::spawn_async(&argv).expect("spawn omp rpc");
        let stdin = child.stdin.take().expect("omp stdin");
        let stdout = child.stdout.take().expect("omp stdout");
        let proc = Arc::new(Proc {
            child: Mutex::new(child),
            stdin: AsyncMutex::new(stdin),
            argv,
        });

        self.done.store(false, Ordering::SeqCst);
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Some(frame) = omp_rpc::parse_frame(&line) {
                            me.route(&frame);
                        }
                    }
                }
            }
            // stdout EOF → child gone. If a turn was in flight the panel must
            // hear about it; otherwise the death is silent (status shows it).
            if !me.done.swap(true, Ordering::SeqCst) {
                me.emit(json!({"type": "error", "message": "процесс оркестратора завершился"}));
            }
        });
        proc
    }

    async fn ensure(self: &Arc<Self>) -> Arc<Proc> {
        let _g = self.spawn_lock.lock().await;
        let existing = {
            let guard = self.proc.lock().unwrap_or_else(|p| p.into_inner());
            match guard.as_ref() {
                Some(p) if p.is_alive() => Some(Arc::clone(p)),
                _ => None,
            }
        };
        if let Some(p) = existing {
            return p;
        }
        let p = self.spawn();
        {
            let mut guard = self.proc.lock().unwrap_or_else(|p| p.into_inner());
            *guard = Some(Arc::clone(&p));
        }
        // Ask the fresh child for its resolved model (recorded, never broadcast).
        let _ = Self::write_frame(&p, &json!({"type": "get_state", "id": "atlas-status"})).await;
        p
    }

    async fn write_frame(proc: &Proc, frame: &Value) -> std::io::Result<()> {
        let mut stdin = proc.stdin.lock().await;
        omp_rpc::write_frame_async(&mut *stdin, frame).await
    }

    /// Send one turn prompt; respawns once on a dead pipe (mirrors orch.py).
    pub async fn prompt(self: &Arc<Self>, message: &str) {
        let proc = self.ensure().await;
        self.done.store(false, Ordering::SeqCst);
        self.turn_started_ms.store(now_ms(), Ordering::SeqCst);
        *self
            .current_prompt
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = message.to_string();
        self.emit(json!({"type": "turn_start", "prompt": message}));

        let frame = json!({"type": "prompt", "message": message});
        if Self::write_frame(&proc, &frame).await.is_err() {
            // process died between ensure() and write; respawn and retry once
            {
                let _g = self.spawn_lock.lock().await;
                let mut guard = self.proc.lock().unwrap_or_else(|p| p.into_inner());
                *guard = Some(self.spawn());
            }
            let proc2 = self
                .proc
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
                .expect("respawned proc");
            if let Err(e) = Self::write_frame(&proc2, &frame).await {
                log::error!("orch: prompt write failed after respawn: {e}");
                self.emit(json!({"type": "error", "message": format!("rpc: запрос не отправлен ({e})")}));
                self.finish(false);
            }
        }
    }

    /// Emit the turn's terminal `done` (idempotent). `Some(ev)` when it emitted.
    pub fn finish(&self, cancelled: bool) -> Option<Value> {
        if self.done.swap(true, Ordering::SeqCst) {
            return None;
        }
        let started = self.turn_started_ms.load(Ordering::SeqCst);
        let duration_s = if started == 0 {
            0.0
        } else {
            (now_ms().saturating_sub(started)) as f64 / 1000.0
        };
        let mut ev = json!({"type": "done", "duration_s": duration_s});
        if cancelled {
            ev["cancelled"] = json!(true);
        }
        self.emit(ev.clone());
        Some(ev)
    }

    /// Kill the child and clear it: the next prompt lazily respawns a fresh one.
    pub async fn restart(self: &Arc<Self>) -> bool {
        let _g = self.spawn_lock.lock().await;
        let old = {
            let mut guard = self.proc.lock().unwrap_or_else(|p| p.into_inner());
            guard.take()
        };
        let had = old.is_some();
        if let Some(p) = old {
            if let Ok(mut c) = p.child.lock() {
                let _ = c.start_kill();
            }
        }
        // No in-flight turn survives a restart: close it for subscribers (the
        // SSE stream then breaks instead of hanging until the 900 s timeout).
        self.finish(false);
        *self.model.lock().unwrap_or_else(|p| p.into_inner()) = None;
        had
    }

    /// `POST /api/orch/cancel`: rpc protocol has an `abort` command, so we
    /// interrupt the turn with it (no kill/respawn). Returns the response body.
    pub async fn cancel(self: &Arc<Self>) -> Value {
        let turn_active = self.gate.try_lock().is_err();
        let proc = self.proc.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let mut abort_sent = false;
        if let Some(p) = proc {
            if p.is_alive() && Self::write_frame(&p, &json!({"type": "abort"})).await.is_ok() {
                abort_sent = true;
            }
        }
        let cancelled = if turn_active { self.finish(true).is_some() } else { false };
        json!({
            "ok": true,
            "abort_sent": abort_sent,
            "turn_active": turn_active,
            "cancelled": cancelled,
        })
    }

    /// Write the rpc response for a pending approval.
    async fn write_approval_response(&self, id: &str, allow: bool) -> std::io::Result<()> {
        let proc = self.proc.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let Some(p) = proc else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "rpc-процесс не запущен",
            ));
        };
        if !p.is_alive() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "rpc-процесс завершился",
            ));
        }
        let value = if allow { "Approve" } else { "Deny" };
        Self::write_frame(
            &p,
            &json!({"type": "extension_ui_response", "id": id, "value": value}),
        )
        .await
    }

    /// `POST /api/orch/approve`: resolve a pending approval.
    pub async fn respond_approval(self: &Arc<Self>, id: &str, allow: bool) -> Result<(), ApproveError> {
        let Some(p) = self.take_pending(id) else {
            return Err(ApproveError::Unknown);
        };
        if let Err(e) = self.write_approval_response(id, allow).await {
            // still unresolved — keep it so the timeout path can deny it later
            self.push_pending(p);
            return Err(ApproveError::Channel(e.to_string()));
        }
        self.emit(json!({"type": "approval_resolved", "id": id, "allow": allow}));
        Ok(())
    }

    /// Auto-deny every approval past its deadline: deny frame, resolved event,
    /// warn notification. One task per approval calls this after the timeout.
    async fn expire_approvals_now(self: &Arc<Self>) {
        let expired = self.expire_approvals(now_ms(), APPROVAL_TIMEOUT_SECS * 1000);
        for p in expired {
            let _ = self.write_approval_response(&p.id, false).await;
            self.emit(json!({"type": "approval_resolved", "id": p.id, "allow": false}));
            let text = format!("rpc: подтверждение {} отклонено по таймауту", p.tool);
            log::warn!("orch: {text}");
            let _ = tokio::task::spawn_blocking(move || {
                let conn = db::db_static().lock();
                if let Err(e) = db::insert_notification(&conn, "warn", None, &text) {
                    log::warn!("orch: approval timeout notification failed: {e}");
                }
            })
            .await;
        }
    }

    fn handle_ui_request(self: &Arc<Self>, frame: &Value) {
        // omp's approval gate is `uiContext.select(prompt, ["Approve","Deny"])`.
        if frame.get("method").and_then(|m| m.as_str()) != Some("select") {
            return;
        }
        let is_approval = frame
            .get("options")
            .and_then(|o| o.as_array())
            .map(|o| o.iter().any(|v| v.as_str() == Some("Approve")))
            .unwrap_or(false);
        if !is_approval {
            return;
        }
        let Some(id) = frame.get("id").and_then(|i| i.as_str()) else {
            return;
        };
        let title = frame.get("title").and_then(|t| t.as_str()).unwrap_or("");
        let tool = parse_tool(title);
        let ts = now_ms();
        self.push_pending(Pending {
            id: id.to_string(),
            tool: tool.clone(),
            detail: title.to_string(),
            ts,
        });
        self.emit(json!({"type": "approval_request", "id": id, "tool": tool, "detail": title}));
        let me = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS)).await;
            me.expire_approvals_now().await;
        });
    }

    fn route(self: &Arc<Self>, frame: &Value) {
        let ftype = frame.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ftype {
            "message_update" => {
                let ev = frame
                    .get("assistantMessageEvent")
                    .cloned()
                    .unwrap_or(Value::Null);
                match ev.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        let delta = ev.get("delta").cloned().unwrap_or_else(|| json!(""));
                        self.emit(json!({"type": "text", "delta": delta}));
                    }
                    Some("thinking_delta") => {
                        let delta = ev.get("delta").cloned().unwrap_or_else(|| json!(""));
                        self.emit(json!({"type": "thinking", "delta": delta}));
                    }
                    _ => {}
                }
            }
            "tool_execution_start" => {
                let name = tool_name(frame);
                let detail = tool_detail(frame.get("args"));
                self.emit(json!({"type": "tool", "name": name, "detail": detail, "phase": "start"}));
            }
            "tool_execution_end" => {
                let name = tool_name(frame);
                let detail = if frame
                    .get("isError")
                    .and_then(|e| e.as_bool())
                    .unwrap_or(false)
                {
                    "ошибка".to_string()
                } else {
                    String::new()
                };
                self.emit(json!({"type": "tool", "name": name, "detail": detail, "phase": "end"}));
            }
            "extension_ui_request" => self.handle_ui_request(frame),
            "response" => {
                let success = frame.get("success").and_then(|s| s.as_bool()).unwrap_or(true);
                if !success {
                    let msg = frame
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("rpc error")
                        .to_string();
                    self.emit(json!({"type": "error", "message": msg}));
                } else if frame.get("command").and_then(|c| c.as_str()) == Some("get_state") {
                    if let Some(m) = frame.pointer("/data/model") {
                        let provider = m.get("provider").and_then(|p| p.as_str()).unwrap_or("");
                        let id = m.get("id").and_then(|i| i.as_str()).unwrap_or("");
                        if !id.is_empty() {
                            let s = if provider.is_empty() {
                                id.to_string()
                            } else {
                                format!("{provider}/{id}")
                            };
                            *self.model.lock().unwrap_or_else(|p| p.into_inner()) = Some(s);
                        }
                    }
                }
            }
            "agent_end" => {
                // `isTerminal:false` = more work is queued; not a run end.
                if frame.get("isTerminal").and_then(|t| t.as_bool()).unwrap_or(true) {
                    self.finish(false);
                }
            }
            "prompt_result" => {
                // local-only prompt completion (agentInvoked:false)
                if frame.get("agentInvoked").and_then(|a| a.as_bool()) == Some(false) {
                    self.finish(false);
                }
            }
            _ => {}
        }
    }
}

fn tool_name(frame: &Value) -> String {
    frame
        .get("toolName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| "tool".into())
}

fn tool_detail(args: Option<&Value>) -> String {
    match args {
        None | Some(Value::Null) => String::new(),
        Some(v) => {
            let s = v.to_string();
            if s.chars().count() <= 200 {
                s
            } else {
                let mut t: String = s.chars().take(200).collect();
                t.push('…');
                t
            }
        }
    }
}

/// The approval prompt's first line is `Allow tool: <name>`.
fn parse_tool(title: &str) -> String {
    if let Some(rest) = title.lines().next().and_then(|l| l.strip_prefix("Allow tool: ")) {
        let name = rest.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    "tool".to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &mut broadcast::Receiver<Value>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        out
    }

    /// Frame fixtures mirror the shapes documented in omp://rpc.md; every one
    /// must land on the canonical UI event contract.
    #[tokio::test]
    async fn rpc_frames_map_to_canonical_events() {
        let orch = Orchestrator::new();
        let mut rx = orch.subscribe();

        orch.route(&json!({
            "type": "message_update",
            "message": {"role": "assistant", "content": []},
            "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": "привет"}
        }));
        orch.route(&json!({
            "type": "message_update",
            "message": {"role": "assistant", "content": []},
            "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 0, "delta": "думаю"}
        }));
        // the mapped frames land as canonical text/thinking events
        orch.route(&json!({
            "type": "tool_execution_start",
            "toolCallId": "t1", "toolName": "bash", "args": {"command": "ls"}
        }));
        orch.route(&json!({
            "type": "tool_execution_end",
            "toolCallId": "t1", "toolName": "bash", "result": {"ok": true}
        }));
        orch.route(&json!({
            "type": "tool_execution_end",
            "toolCallId": "t2", "toolName": "bash", "result": {}, "isError": true
        }));
        // approval gate: extension_ui_request/select with an Approve option
        orch.route(&json!({
            "type": "extension_ui_request", "id": "ui_7", "method": "select",
            "title": "Allow tool: bash\necho hi", "options": ["Approve", "Deny"]
        }));
        // non-approval UI requests and the omp `turn_start` frame are ignored
        orch.route(&json!({
            "type": "extension_ui_request", "id": "ui_8", "method": "notify",
            "message": "hi", "notifyType": "info"
        }));
        orch.route(&json!({"type": "turn_start"}));
        // a failed rpc response is an error event
        orch.route(&json!({
            "type": "response", "command": "prompt", "success": false,
            "error": "Model not found: nope/nope"
        }));

        let events = drain(&mut rx);
        assert_eq!(
            events,
            vec![
                json!({"type": "text", "delta": "привет"}),
                json!({"type": "thinking", "delta": "думаю"}),
                json!({"type": "tool", "name": "bash", "detail": "{\"command\":\"ls\"}", "phase": "start"}),
                json!({"type": "tool", "name": "bash", "detail": "", "phase": "end"}),
                json!({"type": "tool", "name": "bash", "detail": "ошибка", "phase": "end"}),
                json!({"type": "approval_request", "id": "ui_7", "tool": "bash", "detail": "Allow tool: bash\necho hi"}),
                json!({"type": "error", "message": "Model not found: nope/nope"}),
            ]
        );
        // the approval is pending with the parsed tool name
        let pending = orch.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "ui_7");
        assert_eq!(pending[0].tool, "bash");

        // `done` fires once on a terminal agent_end and is recorded in history
        let more = orch.finish(false).expect("first finish emits");
        assert_eq!(more["type"], "done");
        assert!(more["duration_s"].is_number());
        assert!(orch.finish(false).is_none(), "finish is idempotent");
        let hist = orch.history();
        assert_eq!(hist.last().unwrap()["type"], "done");
    }

    #[test]
    fn approval_pending_and_expiry() {
        let orch = Orchestrator::new();
        // fresh approvals survive a timeout sweep; stale ones expire
        orch.push_pending(Pending {
            id: "a".into(),
            tool: "bash".into(),
            detail: "Allow tool: bash".into(),
            ts: 1_000,
        });
        orch.push_pending(Pending {
            id: "b".into(),
            tool: "write".into(),
            detail: "Allow tool: write".into(),
            ts: 9_000,
        });
        let expired = orch.expire_approvals(10_000, 5_000);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, "a");
        let left = orch.pending();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "b");

        // take_pending removes once; a second take misses
        let taken = orch.take_pending("b").expect("pending b");
        assert_eq!(taken.tool, "write");
        assert!(orch.take_pending("b").is_none());
    }

    #[test]
    fn approval_tool_is_parsed_from_the_prompt() {
        assert_eq!(parse_tool("Allow tool: bash\necho hi"), "bash");
        assert_eq!(parse_tool("Allow tool: mcp__foo__bar"), "mcp__foo__bar");
        assert_eq!(parse_tool("что-то другое"), "tool");
    }

    /// The rpc child must be pinned to the ONE dedicated session file: no
    /// `-c` (it resumed whatever was latest in the cwd), and the session is
    /// addressed by full path — a bare id never matches omp's ULID session
    /// ids («Session ... not found»).
    #[test]
    fn spawn_argv_binds_the_dedicated_session() {
        let argv = spawn_argv();
        assert_eq!(argv[0], cfg::OMP_BIN.clone());
        assert_eq!(&argv[1..3], &["--mode".to_string(), "rpc".to_string()]);
        // the dedicated session: a fresh pin inside <data_dir>/orch-sessions
        // (omp 18.9: fresh sessions are minted inside a --session-dir; the
        // resolved file is resumed on later spawns)
        let si = argv
            .iter()
            .position(|a| a == "--session-dir" || a == "--resume")
            .expect("a session pin present");
        let session = argv[si + 1].clone();
        if argv[si] == "--session-dir" {
            // the fresh pin IS the sessions dir (omp mints the file inside it)
            assert_eq!(
                session,
                orch_sessions_dir().to_string_lossy().into_owned(),
                "fresh pin is the orch sessions dir"
            );
        } else {
            assert!(
                session.starts_with(orch_sessions_dir().to_string_lossy().as_ref()),
                "resolved session resumed from the orch sessions dir: {session}"
            );
        }
        assert!(
            !argv.iter().any(|a| a == "-c" || a == "--continue"),
            "-c must be gone: it resumes an unrelated latest session"
        );
        assert!(
            argv.iter().filter(|a| a.as_str() == "--session-dir" || a.as_str() == "--resume").count() == 1,
            "exactly one session pin flag"
        );
        // the approval policy is an overlay, not a mode flag: `--config
        // <atlas>/orchestrator-approval.yml` (mode + the curated prompt list)
        let ci = argv.iter().position(|a| a == "--config").expect("--config present");
        assert_eq!(
            &argv[ci + 1],
            &cfg::orch_overlay_file().to_string_lossy().into_owned(),
            "the child gets the repo's versioned policy overlay"
        );
        assert_eq!(
            argv.iter().filter(|a| a.as_str() == "--config").count(),
            1,
            "exactly one --config overlay"
        );
        assert!(
            !argv.iter().any(|a| a == "always-ask"),
            "always-ask is the bug being fixed: every harmless call prompted"
        );
        // cwd is still the atlas repo, persona still appended
        let wi = argv.iter().position(|a| a == "--cwd").expect("--cwd present");
        assert_eq!(&argv[wi + 1], &cfg::atlas_dir().to_string_lossy().into_owned());
        assert!(argv.iter().any(|a| a == "--append-system-prompt"));
    }

    // --- the approval policy overlay -------------------------------------
    //
    // The overlay is the thing that decides what the orchestrator asks about,
    // so it is parsed and exercised here. The real resolver lives in omp; the
    // matcher below models the DOCUMENTED and MEASURED semantics
    // (omp://tools/bash.md, omp://approval-mode.md, plus raw rpc frames from the
    // 09.10.2026 probes): whitespace runs collapse to one space, `*` is the only
    // wildcard, patterns are anchored at both ends, and `deny`/`prompt` are
    // evaluated against the whole collapsed command — plus, only with
    // `bash.allowCompoundCommands` on and only for a flat `&&` chain, against
    // each segment. The model does NOT model omp's built-in critical override:
    // with no chain to segment it is returned bare and yolo ignores it, so a
    // prompt rule cannot gate those classes — which is exactly why block 0
    // denies them (asserted below). The live probe is the authority; this is the
    // regression net.

    /// One parsed `match`/`approval` rule from the overlay.
    #[derive(Debug, PartialEq)]
    struct Rule {
        matcher: String,
        approval: String,
    }

    /// Parse the overlay's flat `bash.patterns` list, rejecting anything that
    /// is not exactly the shape we ship: comments/blank lines/known blocks, and
    /// `- match: "…"` immediately followed by `approval: <allow|prompt|deny>`.
    /// A real YAML parser is not in the dependency tree; this is stricter than
    /// "it has the keys" because an unexpected line is an error, not a skip.
    fn parse_overlay(text: &str) -> Vec<Rule> {
        let mut rules = Vec::new();
        let mut lines = text.lines().peekable();
        let mut in_patterns = false;
        while let Some(raw) = lines.next() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "patterns:" {
                in_patterns = true;
                continue;
            }
            if !in_patterns {
                // the header keys (`tools:`, `approvalMode: yolo`, `bash:`)
                continue;
            }
            let matcher = line
                .strip_prefix("- match: ")
                .unwrap_or_else(|| panic!("unexpected line inside patterns: {}", line));
            let matcher = matcher
                .strip_prefix('"')
                .and_then(|m| m.strip_suffix('"'))
                .unwrap_or_else(|| panic!("match must be a quoted string: {line}"));
            let approval_line = lines
                .next()
                .unwrap_or_else(|| panic!("match without approval: {matcher}"))
                .trim()
                .to_string();
            let approval = approval_line
                .strip_prefix("approval: ")
                .unwrap_or_else(|| panic!("expected approval: after {matcher}, got {approval_line}"))
                .to_string();
            assert!(
                matches!(approval.as_str(), "allow" | "prompt" | "deny"),
                "bad approval value {approval} for {matcher}"
            );
            rules.push(Rule {
                matcher: matcher.to_string(),
                approval,
            });
        }
        assert!(in_patterns, "no `patterns:` key in the overlay");
        rules
    }

    /// `*`-only glob over whitespace-collapsed text, anchored at both ends
    /// (the global config's `*wtype *` form only makes sense anchored).
    fn glob(pattern: &str, text: &str) -> bool {
        let (p, t) = (pattern.as_bytes(), text.as_bytes());
        // classic two-pointer wildcard match
        let (mut pi, mut ti) = (0usize, 0usize);
        let (mut star, mut mark) = (None, 0usize);
        while ti < t.len() {
            if pi < p.len() && p[pi] == t[ti] {
                pi += 1;
                ti += 1;
            } else if pi < p.len() && p[pi] == b'*' {
                star = Some(pi);
                mark = ti;
                pi += 1;
            } else if let Some(s) = star {
                pi = s + 1;
                mark += 1;
                ti = mark;
            } else {
                return false;
            }
        }
        while pi < p.len() && p[pi] == b'*' {
            pi += 1;
        }
        pi == p.len()
    }

    /// Segments the harness itself splits a command into, before any rule runs,
    /// whenever `bash.allowCompoundCommands` is on and the shell is positively
    /// classified as POSIX-quoting (bash here). Both joins below are MEASURED
    /// with rpc probes against this overlay: `cd /tmp && rm -rf e6` raised the
    /// `rm -rf*` card, and `x=1; sudo pacman -Syu` raised the `sudo*` card — so
    /// the split is not limited to a flat `&&` chain as the docs put it. Joins
    /// and shapes this model does NOT cover (`|`, `&`, subshells, expansions, a
    /// script doing the work) are deliberately not asserted anywhere here.
    fn segments_of(normalized: &str) -> Vec<String> {
        normalized
            .split(" && ")
            .flat_map(|part| part.split("; "))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// `None` = free (the yolo mode allows it), else the resolved approval, with
    /// `deny` winning over `prompt`. Segments are only considered when the
    /// overlay turned compound evaluation on (it ships that key — `verdict`
    /// reads it from the file rather than assuming it).
    fn verdict_with(rules: &[Rule], command: &str, compound: bool) -> Option<String> {
        let normalized = command.split_whitespace().collect::<Vec<_>>().join(" ");
        let segments: Vec<String> = if compound {
            segments_of(&normalized)
        } else {
            Vec::new()
        };
        let mut found: Option<String> = None;
        for probe in std::iter::once(normalized.clone()).chain(segments) {
            for rule in rules {
                if glob(&rule.matcher, &probe) {
                    if rule.approval == "deny" {
                        return Some("deny".to_string());
                    }
                    found.get_or_insert_with(|| "prompt".to_string());
                    break; // first match wins inside one probe text
                }
            }
        }
        found
    }

    /// The overlay as shipped, key included.
    fn verdict(rules: &[Rule], command: &str) -> Option<String> {
        let text =
            std::fs::read_to_string(cfg::orch_overlay_file()).expect("read the overlay");
        verdict_with(rules, command, text.contains("allowCompoundCommands: true"))
    }

    #[test]
    fn policy_overlay_parses_and_covers_the_risky_classes() {
        let path = cfg::orch_overlay_file();
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let rules = parse_overlay(&text);
        assert!(rules.len() > 40, "curated list, not a token rule: {}", rules.len());

        // the overlay must carry the mode, since no --approval-mode flag is
        // passed any more (a flag would silently override edits to this file)
        assert!(
            text.contains("approvalMode: yolo"),
            "the overlay owns the orchestrator's approval mode"
        );

        // the guard's deny rules must be restated (arrays REPLACE, so an
        // overlay that only lists prompt rules would shadow them)
        for tool in ["wtype", "ydotool", "dotool", "wlrctl", "xdotool"] {
            assert!(
                rules
                    .iter()
                    .any(|r| r.approval == "deny" && r.matcher.contains(tool)),
                "deny rule for {tool} must be present"
            );
        }
        for sub in ["hyprctl eval", "hyprctl dispatch", "hyprctl keyword"] {
            assert!(
                rules
                    .iter()
                    .any(|r| r.approval == "deny" && r.matcher.contains(sub)),
                "deny rule for {sub} must be present"
            );
        }

        // the brief's risky classes must all be covered by a prompt rule
        for class in [
            "rm -rf", "rm -r", "dd", "mkfs", "shred", "sudo", "pacman -S", "pacman -R",
            "apt", "npm", "pip install", "systemctl", "loginctl", "pkill", "kill -9",
            "git push", "git reset --hard", "git clean", "git rebase", "chmod -R",
            "chown -R", "/etc/", "/dev/sd", "shutdown", "reboot", "fdisk", "mount",
        ] {
            assert!(
                rules
                    .iter()
                    .any(|r| r.approval == "prompt" && r.matcher.contains(class)),
                "no prompt rule mentions {class}"
            );
        }
    }

    /// The heart of the user's complaint: harmless commands must NOT match a
    /// single rule, while the risky classes must resolve to `deny`/`prompt`
    /// (deny for the guard, prompt for everything else that matters).
    #[test]
    fn policy_overlay_lets_benign_commands_through() {
        let text = std::fs::read_to_string(cfg::orch_overlay_file()).expect("read overlay");
        let rules = parse_overlay(&text);

        for benign in [
            "echo hi",
            "ls -la",
            "cargo build --release",
            "cargo test",
            "git status",
            "git log --oneline -5",
            "git diff HEAD",
            "git add -A",
            "git commit -m x",
        ] {
            assert_eq!(
                verdict(&rules, benign),
                None,
                "benign command must not raise an approval: {benign}"
            );
        }
        // a few more that must stay free, with the reason they are risky-looking
        assert_eq!(verdict(&rules, "rm tmp/file.txt"), None, "single-file rm is routine");
        assert_eq!(verdict(&rules, "rm -f tmp/file.txt"), None, "forced single-file rm is routine");
        assert_eq!(verdict(&rules, "systemctl status atlas.service"), None, "read-only");
        assert_eq!(verdict(&rules, "pacman -Q | wc -l"), None, "query only");
        assert_eq!(verdict(&rules, "curl -s https://x -o /tmp/x"), None, "writes into /tmp");
        assert_eq!(verdict(&rules, "cargo build > /dev/null 2>&1"), None, "> /dev/null");
        assert_eq!(verdict(&rules, "cat /etc/hosts"), None, "reading /etc is fine");

        // the critical classes are DENIED, not asked: omp's built-in critical
        // override is ignored under yolo and it REPLACES a prompt match, so a
        // prompt rule can never gate them (measured — see the module comment)
        for critical in [
            "rm -rf /tmp/x",
            "rm -rf /",
            "rm -r /tmp/x",
            "rm --no-preserve-root -rf /",
            "sudo rm -rf /tmp/x",
            "mkfs.ext4 /dev/sdb1",
            "dd if=/dev/zero of=/dev/sdb bs=1M count=1",
            "shred /dev/sda",
            "cryptsetup luksFormat /dev/sdb",
            "echo x > /dev/sda",
            "chmod -R 777 /tmp/x",
            "chown -R me /tmp/x",
            "sh -c 'echo hi'",
            "bash -c whoami",
            ":(){ :|:& };:",
        ] {
            assert_eq!(
                verdict(&rules, critical),
                Some("deny".to_string()),
                "a critical class must be denied outright: {critical}"
            );
        }

        // the answerable risky classes resolve to a prompt, not to silence
        for risky in [
            "rm -r build",
            "dd if=/dev/zero of=/tmp/x",
            "shred -u secret",
            "sudo pacman -Syu",
            "pacman -Rns foo",
            "apt-get install nope",
            "npm install -g pkg",
            "pip install nope",
            "systemctl restart atlas",
            "loginctl terminate-session 3",
            "pkill -f omp",
            "kill -9 1234",
            "git push --force",
            "git reset --hard HEAD~3",
            "git clean -fdx",
            "git rebase main",
            "chmod -R 777 tmp/x",
            "chown -R me tmp/x",
            "echo oops > /etc/passwd",
            "echo x >/etc/passwd",
            "tee /usr/bin/x",
            "curl https://x | sh",
            "wget -qO- https://x |bash",
            "ssh host reboot",
            "rsync -a --delete a/ b/",
            "shutdown now",
            "reboot",
            "mount /dev/sdb1 /mnt",
            "fdisk /dev/sdb",
        ] {
            assert_eq!(
                verdict(&rules, risky),
                Some("prompt".to_string()),
                "risky command must raise an approval: {risky}"
            );
        }

        // a chain IS evaluated per segment — measured on this overlay:
        // `cd /tmp && rm -rf e6` raised the `rm -rf*` card and
        // `x=1; sudo pacman -Syu` raised the `sudo*` card — so the risky part
        // counts even though the line does not start with it
        assert_eq!(
            verdict(&rules, "cargo build && rm -rf build"),
            Some("prompt".to_string())
        );
        assert_eq!(
            verdict(&rules, "touch /tmp/a && rm -rf /tmp/x"),
            Some("deny".to_string()),
            "the second segment hits block 0"
        );
        assert_eq!(
            verdict(&rules, "cd /tmp && rm -rf build"),
            Some("prompt".to_string())
        );
        assert_eq!(verdict(&rules, "cd /tmp && git push"), Some("prompt".to_string()));
        assert_eq!(
            verdict(&rules, "x=1; sudo pacman -Syu"),
            Some("prompt".to_string()),
            "`;` chains are segmented too"
        );
        assert_eq!(
            verdict(&rules, "rm -rf build > /dev/null"),
            Some("prompt".to_string()),
            "a redirection after the risky part still matches: the line starts with it"
        );
        // block 0's deny rules carry a leading `*`, so those classes are caught
        // even in the middle of a line — measured: a command wrapped as
        // `stat …; rm -rf /tmp/x; stat …` was blocked by `*rm -rf /*`
        assert_eq!(verdict(&rules, "x=1; rm -rf /tmp/x"), Some("deny".to_string()));
        assert_eq!(
            verdict(&rules, "stat -c %n /tmp/x; rm -rf /tmp/x; echo done"),
            Some("deny".to_string())
        );

        // the guard's denies stay absolute, and beat a prompt rule
        assert_eq!(verdict(&rules, "wtype hi"), Some("deny".to_string()));
        assert_eq!(verdict(&rules, "echo x | wtype -"), Some("deny".to_string()));
        assert_eq!(verdict(&rules, "hyprctl dispatch exec kitty"), Some("deny".to_string()));
        assert_eq!(verdict(&rules, "sudo wtype hi"), Some("deny".to_string()));
        // ... while a read-only hyprctl call is deliberately still free
        assert_eq!(verdict(&rules, "hyprctl clients -j"), None);

        // quoting is not a bypass for the *text* patterns, but it is a real
        // limit worth documenting: a program that does its own destructive work
        // from an argument (a script, `find -exec shred`) is not caught here
        assert_eq!(verdict(&rules, "bash myscript.sh"), None);
    }

    /// The whole auto-deny story against a live child: an approval nobody
    /// answers is denied by the 5-minute sweep (Deny frame into the child's
    /// stdin, `approval_resolved` with `allow:false`, warn notification) —
    /// and the failed turn omp then answers with is mapped to the canonical
    /// `error` event, which must reach the stream (broadcast) and the
    /// history ring the panel replays.
    #[tokio::test]
    async fn approval_auto_deny_then_error_reaches_stream() {
        // the sweep writes a warn notification — the shared throwaway test DB
        // (serialized init, same one the http tests use)
        crate::db::test_init_once();

        let orch = Orchestrator::new();
        let mut rx = orch.subscribe();

        // a live child with a writable stdin (cat echoes nothing, dies with
        // the test via kill_on_drop) so the Deny frame really lands
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().expect("spawn cat");
        let stdin = child.stdin.take().expect("cat stdin");
        let proc = Arc::new(Proc {
            child: Mutex::new(child),
            stdin: AsyncMutex::new(stdin),
            argv: vec!["cat".into()],
        });
        *orch.proc.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&proc));

        // an approval nobody answers
        orch.route(&json!({
            "type": "extension_ui_request", "id": "ui_t", "method": "select",
            "title": "Allow tool: bash\necho probe", "options": ["Approve", "Deny"]
        }));
        // ... and five minutes pass (backdate the pending ts)
        let stale = now_ms() - APPROVAL_TIMEOUT_SECS * 1000 - 1;
        if let Some(p) = orch
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .first_mut()
        {
            p.ts = stale;
        }
        // the sweep denies it — the real code path, not a re-implementation
        orch.expire_approvals_now().await;

        // omp reacts to the Deny by failing the turn
        orch.route(&json!({
            "type": "response", "command": "prompt", "success": false,
            "error": "Session aborted: approval denied by timeout"
        }));

        let types: Vec<String> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| e["type"].as_str().map(String::from))
            .collect();
        assert_eq!(
            types,
            vec![
                "approval_request".to_string(),
                "approval_resolved".to_string(),
                "error".to_string(),
            ],
            "deny + the failed turn's error must both reach the stream"
        );
        let hist = orch.history();
        assert_eq!(hist[1]["allow"], json!(false), "resolved is the denial");
        assert!(
            hist[2]["message"].as_str().unwrap().contains("approval denied"),
            "the error carries omp's own reason: {:?}",
            hist[2]
        );
        assert!(orch.pending().is_empty(), "nothing left pending");
    }
}
