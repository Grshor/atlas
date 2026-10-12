//! Терминал atlas: persistent PTYs owned by the server process, one per
//! session, bridged to the `terminal` panel over a websocket.
//!
//! Sessions (keyed by id):
//!   * `proj-<slug>` — one project shell (`$SHELL -l`, cwd = the project path).
//!     Created on demand (focus W / first WS attach) and listed only while it
//!     exists. `POST /api/terminal/{id}/kill` kills and FORGETS it, so the next
//!     attach starts a fresh one.
//!   * `herdr` — the herdr CLIENT (`herdr` alone means «Launch or attach to
//!     the persistent session»), cwd = home. The visible herdr UI: focus H/T
//!     and `POST /api/herdr/focus` ensure it instead of any wezterm window,
//!     and the user can open/kill/restart it like any project shell. Also
//!     created lazily — never at boot.
//!
//! The orchestrator no longer has a PTY here: it is the rpc chat of the
//! Оркестратор panel (`src/orch.rs`). The single brain lives there.
//!
//! Lifecycle: a PTY is spawned lazily on the first WS connect, survives panel
//! and window close (the server owns it) and is respawned by the next connect
//! after its child exits. Exactly one live viewer per session is allowed — a
//! second connect gets `{"type":"busy"}` and is closed; while no viewer is
//! attached the child keeps running and its output is discarded (the broadcast
//! has no receivers). Sessions are PROCESS-LOCAL: they die with the server.

use axum::extract::ws::{Message, WebSocket};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use tokio::sync::broadcast;

/// Applied at spawn; the client's first frame is a `resize` that supersedes it.
const DEFAULT_COLS: u16 = 100;
const DEFAULT_ROWS: u16 = 30;

// ------------------------------------------------------------------ base64 ---

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        out.push(B64[(b0 >> 2) as usize] as char);
        out.push(B64[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Standard alphabet; padding and whitespace are tolerated, anything else is a
/// decode error (None) — a malformed client frame is dropped, never guessed.
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        if matches!(c, b'=' | b'\n' | b'\r' | b' ' | b'\t') {
            continue;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

// -------------------------------------------------------------- pty session ---

/// Thin wrapper over one portable-pty pair: write / resize / blocking read /
/// exit detection. Deliberately independent of `omp` so it can be unit-tested
/// with `/bin/cat`.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Option<Box<dyn Write + Send>>,
    child: Box<dyn Child + Send + Sync>,
    reader: Option<Box<dyn Read + Send>>,
}

impl PtySession {
    pub fn spawn(
        argv: &[String],
        cwd: &Path,
        env: &[(&str, &str)],
        cols: u16,
        rows: u16,
    ) -> Result<PtySession, String> {
        let prog = argv.first().ok_or("пустой argv")?;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {e}"))?;
        let mut cmd = CommandBuilder::new(prog);
        for a in &argv[1..] {
            cmd.arg(a);
        }
        cmd.cwd(cwd);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("spawn {prog}: {e}"))?;
        // the master must not keep the slave open or EOF never arrives
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("pty reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("pty writer: {e}"))?;
        Ok(PtySession {
            master: pair.master,
            writer: Some(writer),
            child,
            reader: Some(reader),
        })
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("resize: {e}"))
    }

    pub fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        match self.writer.as_mut() {
            Some(w) => w.write_all(data).map_err(|e| format!("write: {e}")),
            None => Err("pty writer закрыт".into()),
        }
    }

    /// Hands the readable end to the caller's thread (the server reads from a
    /// dedicated reader thread; tests read directly). The master stays here so
    /// resize/write/kill keep working.
    pub fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
        self.reader.take()
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Some(code) once the child has exited (an unreapable child counts as
    /// gone: -1), None while it still runs.
    pub fn try_exit(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(st)) => Some(st.exit_code() as i32),
            Ok(None) => None,
            Err(_) => Some(-1),
        }
    }

    /// Blocks until the child exits.
    pub fn wait(&mut self) -> i32 {
        self.child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1)
    }

    /// SIGKILL via portable-pty (wezterm's killer), used by kill/restart.
    pub fn kill(&mut self) -> Result<(), String> {
        self.child.kill().map_err(|e| format!("kill: {e}"))
    }
}

// ---------------------------------------------------------------- sessions ---

/// Everything needed to start one session; also the honest answer to "what
/// would `GET /api/terminal/sessions` spawn".
pub struct Spec {
    pub id: String,
    pub label: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
}

/// Session-id slug of a project name: lowercased, runs of anything that is not
/// alphanumeric collapse into a single `-` (`CBQN` → `cbqn`, `omp-graft` →
/// `omp-graft`, `Игры` → `игры`).
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Session id of a project's shell.
pub fn proj_id(name: &str) -> String {
    format!("proj-{}", slug(name))
}

/// Project label of a `proj-<slug>` session id, if the id has that shape.
pub fn proj_slug(id: &str) -> Option<&str> {
    id.strip_prefix("proj-").filter(|s| !s.is_empty())
}

/// The user's login shell, `$SHELL -l` (project sessions always use this).
pub fn shell_argv() -> Vec<String> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    vec![shell, "-l".into()]
}

/// A project shell session: `$SHELL -l` in the project path.
pub fn proj_spec(name: &str, path: &Path) -> Spec {
    Spec {
        id: proj_id(name),
        label: name.to_string(),
        argv: shell_argv(),
        cwd: path.to_path_buf(),
    }
}

/// Session id of the herdr client (the visible herdr UI lives in the atlas
/// terminal panel; there is no wezterm client window any more).
pub const HERDR_SESSION_ID: &str = "herdr";

/// The herdr client session: bare `herdr` («Launch or attach to the persistent
/// session» — verified against `herdr --help`), cwd = home. Spawned lazily on
/// first need (focus H/T, `POST /api/herdr/focus` or the first WS attach),
/// never at boot.
pub fn herdr_spec() -> Spec {
    Spec {
        id: HERDR_SESSION_ID.to_string(),
        label: "herdr".to_string(),
        argv: vec![crate::cfg::HERDR_BIN.clone()],
        cwd: crate::cfg::home(),
    }
}

// ---------------------------------------------------------------- wezterm ---

/// Workspace marker of the atlas-OWNED wezterm instance: every «открыть в
/// wezterm» must land in an instance atlas started itself, launched with
/// `--workspace atlas` (see `wezterm_start_argv`). Discovery probes the
/// per-instance cli sockets and trusts only one reporting this workspace, so
/// tabs opened from atlas chips can never land in the user's personal
/// wezterm window(s).
pub const ATLAS_WORKSPACE: &str = "atlas";

/// Window class / Wayland app_id of the atlas instance — a stable handle for
/// the user's window rules. The isolation itself is the workspace marker plus
/// the explicit socket below; the class is identification, not the mechanism.
pub const ATLAS_CLASS: &str = "atlas-wezterm";

/// Env var that points `wezterm cli` at one concrete instance socket instead
/// of default discovery (which would find the user's own GUI).
pub const WEZTERM_SOCK_ENV: &str = "WEZTERM_UNIX_SOCKET";

/// The atlas-dedicated wezterm instance: its cli socket and the window the
/// «↗» chips open into.
pub struct AtlasInstance {
    pub sock: PathBuf,
    pub window_id: i64,
}

/// Serializes «открыть в wezterm» inside this atlas process: two chips opened
/// in the same instant must not both see "no instance" and launch two GUIs.
pub static OPEN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Directory of wezterm's per-instance cli sockets (`gui-sock-<pid>` inside
/// `$XDG_RUNTIME_DIR/wezterm`; fallback `/run/user/<uid>/wezterm` when the
/// env var is unset). `None` = no runtime dir on this box.
fn wezterm_sockets_dir() -> Option<PathBuf> {
    let rt = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(rt) => PathBuf::from(rt),
        None => PathBuf::from("/run/user").join(uid_of_self()?.to_string()),
    };
    Some(rt.join("wezterm"))
}

/// Real uid from /proc (no libc dependency); `None` if unreadable.
fn uid_of_self() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// `(workspace, window_id)` of every pane one instance socket reports;
/// `None` when the socket doesn't answer (dead/stale socket, no instance).
/// Spawn errors of `bin` itself propagate (missing binary → the caller's 400).
fn socket_panes(bin: &str, sock: &Path) -> std::io::Result<Option<Vec<(String, i64)>>> {
    let out = std::process::Command::new(bin)
        .args(["cli", "--no-auto-start", "list", "--format", "json"])
        .env(WEZTERM_SOCK_ENV, sock)
        .stdin(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let v: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let Some(arr) = v.as_array() else { return Ok(None) };
    Ok(Some(
        arr.iter()
            .filter_map(|p| {
                let ws = p.get("workspace")?.as_str()?.to_owned();
                let win = p.get("window_id").and_then(|w| w.as_i64()).unwrap_or(0);
                Some((ws, win))
            })
            .collect(),
    ))
}

/// Does this socket serve a live instance with a pane in the atlas workspace?
pub fn socket_is_atlas(bin: &str, sock: &Path) -> std::io::Result<bool> {
    Ok(socket_panes(bin, sock)?
        .is_some_and(|panes| panes.iter().any(|(ws, _)| ws == ATLAS_WORKSPACE)))
}

/// The atlas-dedicated instance, found by marker: probe every per-instance
/// socket, trust the one with a pane in the `atlas` workspace. Read-only
/// against the user's own instances (a `cli list` probe each), never spawns
/// anything. `Ok(None)` = no live atlas instance (the caller launches one).
pub fn atlas_wezterm_instance(bin: &str) -> std::io::Result<Option<AtlasInstance>> {
    let Some(dir) = wezterm_sockets_dir() else { return Ok(None) };
    let Ok(rd) = std::fs::read_dir(&dir) else { return Ok(None) };
    let mut socks: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("gui-sock-"))
        })
        .collect();
    socks.sort();
    for sock in socks {
        let Some(panes) = socket_panes(bin, &sock)? else { continue };
        let atlas: Vec<i64> = panes
            .iter()
            .filter(|(ws, _)| ws == ATLAS_WORKSPACE)
            .map(|(_, w)| *w)
            .collect();
        if let Some(window_id) = atlas.iter().min() {
            return Ok(Some(AtlasInstance { sock, window_id: *window_id }));
        }
    }
    Ok(None)
}

/// Expected cli socket of a wezterm GUI atlas just launched: `wezterm start`
/// execs into wezterm-gui keeping the pid, and the per-instance socket is
/// `gui-sock-<pid>` (verified against the installed wezterm).
pub fn wezterm_gui_socket(pid: u32) -> Option<PathBuf> {
    wezterm_sockets_dir().map(|d| d.join(format!("gui-sock-{pid}")))
}

/// argv «открыть в wezterm» when the atlas-OWNED instance is running: a new
/// TAB in ITS window (`--window-id` — the window may be unfocused, so the
/// focused-pane default doesn't answer), with the session's own cwd and cmd —
/// exactly what the embedded PTY spawner would run. The caller passes the
/// instance socket in `WEZTERM_UNIX_SOCKET` (`WEZTERM_SOCK_ENV`).
pub fn wezterm_cli_spawn_argv(bin: &str, window_id: i64, cwd: &Path, cmd: &[String]) -> Vec<String> {
    let mut argv = vec![
        bin.to_string(),
        "cli".into(),
        "--no-auto-start".into(),
        "spawn".into(),
        "--window-id".into(),
        window_id.to_string(),
        "--cwd".into(),
        cwd.to_string_lossy().into_owned(),
        "--".into(),
    ];
    argv.extend_from_slice(cmd);
    argv
}

/// argv «открыть в wezterm» when no atlas instance answers: a fresh process =
/// a new WINDOW that IS the atlas-dedicated instance. `--workspace atlas`
/// marks it for `atlas_wezterm_instance` discovery (and keeps it distinct
/// from the user's own default-workspace windows), `--class` gives it a
/// stable app_id for window rules. The session is the initial window's
/// program, so the first open lands the session right there.
pub fn wezterm_start_argv(bin: &str, cwd: &Path, cmd: &[String]) -> Vec<String> {
    let mut argv = vec![
        bin.to_string(),
        "start".into(),
        "--always-new-process".into(),
        "--workspace".into(),
        ATLAS_WORKSPACE.into(),
        "--class".into(),
        ATLAS_CLASS.into(),
        "--cwd".into(),
        cwd.to_string_lossy().into_owned(),
        "--".into(),
    ];
    argv.extend_from_slice(cmd);
    argv
}

/// The one-line 400 text for `POST /api/terminal/{id}/open` when the wezterm
/// binary cannot run: names the program and the config key that moves it.
pub fn wezterm_missing_error(bin: &str) -> String {
    format!("терминал wezterm не найден («{bin}» не запускается) — путь задаётся ключом binaries.term в config.toml")
}

/// The tail pane of the atlas instance: among the panes of the `atlas`
/// workspace (other workspaces on the same socket are not ours even when
/// their window_id is smaller), the min `window_id` wins — the same window
/// [`atlas_wezterm_instance`] reports and the dock drives — and inside that
/// window the min `pane_id` (the anchor pane; deterministic when the user
/// has split the window). Pure: parses a `wezterm cli list --format json`
/// payload; rows missing any of the three fields are skipped, `None` = no
/// atlas-workspace pane in the payload.
pub fn pane_of_min_window(list: &Value) -> Option<i64> {
    list.as_array()?
        .iter()
        .filter_map(|p| {
            if p.get("workspace")?.as_str()? != ATLAS_WORKSPACE {
                return None;
            }
            Some((
                p.get("window_id")?.as_i64()?,
                p.get("pane_id")?.as_i64()?,
            ))
        })
        .min()
        .map(|(_, pane)| pane)
}

#[derive(Clone)]
enum Ev {
    Data(Vec<u8>),
    Exit(i32),
}

struct Live {
    id: String,
    /// The project's own name — the panel shows it.
    label: String,
    /// The argv actually used at spawn (honest status: omp vs $SHELL -l).
    argv: Vec<String>,
    pty: Mutex<PtySession>,
    tx: broadcast::Sender<Ev>,
    /// One live viewer per session; a second attach gets `busy`.
    viewer: AtomicBool,
}

impl Live {
    fn alive(&self) -> bool {
        let mut p = self.pty.lock().unwrap_or_else(|p| p.into_inner());
        p.try_exit().is_none()
    }
    fn pid(&self) -> Option<u32> {
        self.pty
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pid()
    }
    fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        self.pty
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .resize(cols, rows)
    }
    fn write(&self, data: &[u8]) -> Result<(), String> {
        self.pty
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .write_all(data)
    }
}

/// Registry of live sessions; a poisoned lock must not lose it.
static LIVE: LazyLock<Mutex<BTreeMap<String, Arc<Live>>>> = LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn live_map() -> &'static Mutex<BTreeMap<String, Arc<Live>>> {
    &LIVE
}

/// One row of `GET /api/terminal/sessions`.
#[derive(Clone)]
pub struct SessionInfo {
    pub id: String,
    pub label: String,
    pub cmd: Vec<String>,
    pub alive: bool,
    pub pid: Option<u32>,
}

impl SessionInfo {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "label": self.label,
            "cmd": self.cmd,
            "alive": self.alive,
            "pid": self.pid,
        })
    }
}

fn info_of_live(l: &Arc<Live>) -> SessionInfo {
    SessionInfo {
        id: l.id.clone(),
        label: l.label.clone(),
        cmd: l.argv.clone(),
        alive: l.alive(),
        pid: l.pid(),
    }
}

fn reader_thread(mut reader: Box<dyn Read + Send>, sess: Arc<Live>, tx: broadcast::Sender<Ev>) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // Err = no receivers: output is discarded while detached
                    let _ = tx.send(Ev::Data(buf[..n].to_vec()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let code = {
            let mut p = sess.pty.lock().unwrap_or_else(|p| p.into_inner());
            p.wait()
        };
        let _ = tx.send(Ev::Exit(code));
    });
}

/// The session's PTY, spawned on first use; a dead child is replaced.
fn ensure(spec: &Spec) -> Result<Arc<Live>, String> {
    let mut map = live_map().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(l) = map.get(&spec.id) {
        if l.alive() {
            return Ok(Arc::clone(l));
        }
        log::info!("terminal: session '{}' pty exited — respawning", spec.id);
    }
    map.remove(&spec.id);
    let mut pty = PtySession::spawn(
        &spec.argv,
        &spec.cwd,
        &[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")],
        DEFAULT_COLS,
        DEFAULT_ROWS,
    )?;
    let reader = pty.take_reader();
    let (tx, _rx) = broadcast::channel::<Ev>(1024);
    let live = Arc::new(Live {
        id: spec.id.clone(),
        label: spec.label.clone(),
        argv: spec.argv.clone(),
        pty: Mutex::new(pty),
        tx: tx.clone(),
        viewer: AtomicBool::new(false),
    });
    if let Some(reader) = reader {
        reader_thread(reader, Arc::clone(&live), tx);
    }
    log::info!(
        "terminal: session '{}' spawned ({:?}, cwd {})",
        spec.id,
        spec.argv,
        spec.cwd.display()
    );
    map.insert(spec.id.clone(), Arc::clone(&live));
    Ok(live)
}

/// `GET /api/terminal/sessions` — the project shells that exist right now.
/// Listing never spawns anything.
pub fn list() -> Vec<SessionInfo> {
    let map = live_map().lock().unwrap_or_else(|p| p.into_inner());
    map.values().map(info_of_live).collect()
}

/// One session's row: a `proj-*` session only while it exists.
pub fn session_state(id: &str) -> Option<SessionInfo> {
    let map = live_map().lock().unwrap_or_else(|p| p.into_inner());
    map.get(id).map(info_of_live)
}

#[derive(Debug)]
pub enum KillError {
    /// No such session in this process.
    Unknown,
}

/// Kill and FORGET a project session: the next attach starts a fresh shell.
pub fn kill(id: &str) -> Result<(), KillError> {
    let live = {
        let mut map = live_map().lock().unwrap_or_else(|p| p.into_inner());
        map.remove(id).ok_or(KillError::Unknown)?
    };
    if let Err(e) = live
        .pty
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .kill()
    {
        log::warn!("terminal: kill '{id}': {e}");
    }
    log::info!("terminal: session '{id}' killed and forgotten");
    Ok(())
}

/// Drop the session (killing its child) so the next attach respawns it.
/// `false` = no such session.
pub fn restart(id: &str) -> bool {
    let live = {
        let mut map = live_map().lock().unwrap_or_else(|p| p.into_inner());
        map.remove(id)
    };
    let Some(live) = live else {
        return false;
    };
    if let Err(e) = live
        .pty
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .kill()
    {
        log::warn!("terminal: restart '{id}': {e}");
    }
    true
}

/// Spawn a session on demand (used by focus W). Returns the row that
/// `/api/terminal/sessions` will show right after.
pub fn ensure_info(spec: &Spec) -> Result<SessionInfo, String> {
    let live = ensure(spec)?;
    Ok(info_of_live(&live))
}

struct ViewerGuard(Arc<Live>);

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        self.0.viewer.store(false, Ordering::SeqCst);
    }
}

async fn send_json(socket: &mut WebSocket, v: &Value) -> bool {
    socket.send(Message::Text(v.to_string().into())).await.is_ok()
}

/// WS bridge for `GET /api/terminal/ws/{session_id}`.
pub async fn serve_ws(spec: Spec, mut socket: WebSocket) {
    let sess = match ensure(&spec) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("terminal: spawn '{}' failed: {e}", spec.id);
            let msg = format!("atlas: не удалось запустить терминал: {e}\r\n");
            send_json(
                &mut socket,
                &json!({"type": "out", "data": b64_encode(msg.as_bytes())}),
            )
            .await;
            send_json(&mut socket, &json!({"type": "exit", "code": 127})).await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    if sess.viewer.swap(true, Ordering::SeqCst) {
        send_json(&mut socket, &json!({"type": "busy"})).await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    }
    let _guard = ViewerGuard(Arc::clone(&sess));
    let mut rx = sess.tx.subscribe();

    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(Ev::Data(d)) => {
                    if !send_json(&mut socket, &json!({"type": "out", "data": b64_encode(&d)})).await {
                        break;
                    }
                }
                Ok(Ev::Exit(code)) => {
                    send_json(&mut socket, &json!({"type": "exit", "code": code})).await;
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = socket.recv() => {
                // client gone without the child exiting: the PTY keeps running
                let Some(Ok(msg)) = msg else { break };
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                match frame.get("type").and_then(|t| t.as_str()) {
                    Some("resize") => {
                        let cols = frame.get("cols").and_then(|c| c.as_u64()).unwrap_or(DEFAULT_COLS as u64);
                        let rows = frame.get("rows").and_then(|r| r.as_u64()).unwrap_or(DEFAULT_ROWS as u64);
                        if (1..=u16::MAX as u64).contains(&cols) && (1..=u16::MAX as u64).contains(&rows) {
                            if let Err(e) = sess.resize(cols as u16, rows as u16) {
                                log::warn!("terminal: resize {cols}x{rows}: {e}");
                            }
                        }
                    }
                    Some("in") => {
                        if let Some(data) = frame.get("data").and_then(|d| d.as_str()) {
                            if let Some(bytes) = b64_decode(data) {
                                if let Err(e) = sess.write(&bytes) {
                                    log::warn!("terminal: write: {e}");
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn base64_roundtrip() {
        for case in [
            &b""[..],
            b"a",
            b"ab",
            b"abc",
            b"hello, atlas!\n",
            &[0u8, 255, 16, 128, 7],
        ] {
            let s = b64_encode(case);
            assert_eq!(b64_decode(&s).as_deref(), Some(case), "encoding {s}");
        }
        assert_eq!(b64_encode(b"ab"), "YWI=");
        assert_eq!(b64_encode(b"a"), "YQ==");
        // padding/whitespace tolerated, junk rejected
        assert_eq!(b64_decode("YQ").as_deref(), Some(&b"a"[..]));
        assert_eq!(b64_decode("YQ==").as_deref(), Some(&b"a"[..]));
        assert!(b64_decode("!!!!").is_none());
    }

    #[test]
    fn session_ids_and_slugs_are_stable() {
        assert_eq!(proj_id("atlas"), "proj-atlas");
        assert_eq!(proj_id("CBQN"), "proj-cbqn");
        assert_eq!(proj_id("omp-graft"), "proj-omp-graft");
        assert_eq!(proj_id("Игры"), "proj-игры");
        assert_eq!(proj_id("SølskMap"), "proj-sølskmap");
        assert_eq!(proj_id("  weird  name  "), "proj-weird-name");
        assert_eq!(proj_slug("proj-atlas"), Some("atlas"));
        assert_eq!(proj_slug("orch"), None);
        assert_eq!(proj_slug("proj-"), None);
    }

    #[test]
    fn specs_have_the_documented_shape() {
        let shell = shell_argv();
        assert!(shell[1] == "-l", "project shells are login shells: {shell:?}");

        let proj = proj_spec("CBQN", Path::new("/tmp/x"));
        assert_eq!(proj.id, "proj-cbqn");
        assert_eq!(proj.label, "CBQN", "the label keeps the project's own name");
        assert_eq!(proj.cwd, PathBuf::from("/tmp/x"));
        assert_eq!(proj.argv, shell, "project sessions run $SHELL -l");

        // the herdr client session: bare `herdr` in the user's home
        let h = herdr_spec();
        assert_eq!(h.id, "herdr");
        assert_eq!(h.label, "herdr");
        assert_eq!(
            h.argv,
            vec![crate::cfg::HERDR_BIN.clone()],
            "bare herdr = launch or attach"
        );
        assert_eq!(h.cwd, crate::cfg::home());
        assert!(proj_slug(&h.id).is_none(), "herdr is not a project shell");
    }

    /// Running-atlas-instance branch: a new tab in the atlas window via
    /// `cli spawn --window-id` (explicit target — the window may be
    /// unfocused), cwd + cmd passed through verbatim (pure argv construction
    /// — no wezterm is launched). The socket goes to the child's env, not the
    /// argv (`term::WEZTERM_SOCK_ENV`).
    #[test]
    fn wezterm_argv_running_instance_is_cli_spawn_tab() {
        let argv = wezterm_cli_spawn_argv(
            "wezterm",
            0,
            Path::new("/home/u/Projects/atlas"),
            &["/bin/zsh".into(), "-l".into()],
        );
        assert_eq!(
            argv,
            vec![
                "wezterm", "cli", "--no-auto-start", "spawn", "--window-id", "0", "--cwd",
                "/home/u/Projects/atlas", "--", "/bin/zsh", "-l"
            ]
        );
        // the herdr client keeps its exact attach argv in the wezterm tab too
        let h = herdr_spec();
        let argv = wezterm_cli_spawn_argv("wezterm", 3, &h.cwd, &h.argv);
        assert_eq!(
            argv,
            vec![
                "wezterm", "cli", "--no-auto-start", "spawn", "--window-id", "3", "--cwd",
                h.cwd.to_string_lossy().as_ref(), "--", h.argv[0].as_str()
            ]
        );
    }

    /// No-instance branch: `start --always-new-process` = a new window that
    /// IS the atlas-dedicated instance — `--workspace atlas` marks it for
    /// discovery, `--class` gives it a stable app_id; same cwd + cmd
    /// passthrough (pure argv construction).
    #[test]
    fn wezterm_argv_without_instance_is_start_atlas_window() {
        let argv = wezterm_start_argv("wezterm", Path::new("/tmp"), &["/bin/sleep".into(), "3".into()]);
        assert_eq!(
            argv,
            vec![
                "wezterm", "start", "--always-new-process", "--workspace", "atlas", "--class",
                "atlas-wezterm", "--cwd", "/tmp", "--", "/bin/sleep", "3"
            ]
        );
        assert_eq!(ATLAS_WORKSPACE, "atlas");
        assert_eq!(ATLAS_CLASS, "atlas-wezterm");
        assert_eq!(WEZTERM_SOCK_ENV, "WEZTERM_UNIX_SOCKET");
    }

    #[test]
    fn wezterm_missing_binary_error_names_the_config_key() {
        let msg = wezterm_missing_error("/opt/wezterm/bin/wezterm");
        assert!(msg.contains("binaries.term"), "must name the config key: {msg}");
        assert!(
            msg.contains("/opt/wezterm/bin/wezterm"),
            "must name the configured program: {msg}"
        );
        assert_eq!(msg.lines().count(), 1, "one honest line: {msg}");
    }

    /// Fixture in the shape of `wezterm cli list --format json` (only the
    /// fields the choice reads). A default-workspace row with a SMALLER
    /// window_id must not win; inside one atlas window the min pane_id is
    /// the deterministic anchor.
    #[test]
    fn tail_pane_is_min_window_of_atlas_workspace() {
        let list: Value = serde_json::from_str(
            r#"[
                {"window_id":2,"pane_id":1,"workspace":"default"},
                {"window_id":5,"pane_id":11,"workspace":"atlas"},
                {"window_id":5,"pane_id":4,"workspace":"atlas"},
                {"window_id":3,"pane_id":8,"workspace":"atlas"}
            ]"#,
        )
        .unwrap();
        assert_eq!(pane_of_min_window(&list), Some(8));
    }

    /// No atlas-workspace pane (or an empty/garbage payload) — no pane.
    #[test]
    fn tail_pane_none_without_atlas_workspace() {
        let list: Value = serde_json::from_str(
            r#"[{"window_id":0,"pane_id":0,"workspace":"default"}]"#,
        )
        .unwrap();
        assert_eq!(pane_of_min_window(&list), None);
        assert_eq!(pane_of_min_window(&Value::Array(vec![])), None);
        assert_eq!(pane_of_min_window(&json!({"no":"array"})), None);
    }

    /// Registry mechanics with the real `/bin/cat` PTY: spawn → list → kill
    /// forgets the project session. The orchestrator has NO session here: it
    /// is the rpc chat of the Оркестратор panel.
    #[test]
    fn session_registry_without_orch() {
        // nothing is listed before anything is spawned — not even an orch row
        let spec = Spec {
            id: proj_id("atlas"),
            label: "atlas".into(),
            argv: vec!["/bin/cat".into()],
            cwd: std::env::temp_dir(),
        };
        assert!(session_state(&spec.id).is_none(), "listing must not spawn");
        assert!(session_state("orch").is_none(), "orch is not a terminal session");
        assert!(list().is_empty(), "nothing is listed before a shell exists");
        assert!(matches!(kill("orch"), Err(KillError::Unknown)));
        assert!(!restart("orch"), "orch cannot be restarted as a terminal session");

        let info = ensure_info(&spec).expect("spawn /bin/cat session");
        assert_eq!(info.id, "proj-atlas");
        assert_eq!(info.label, "atlas");
        assert_eq!(info.cmd, vec!["/bin/cat".to_string()]);
        assert!(info.alive);
        assert!(info.pid.is_some());

        let listed = list();
        let ids: Vec<&str> = listed.iter().map(|s| s.id.as_str()).collect();
        assert!(!ids.contains(&"orch"), "no orch row: {ids:?}");
        let proj_row = listed
            .iter()
            .find(|s| s.id == "proj-atlas")
            .expect("created session listed");
        assert_eq!(proj_row.label, "atlas");
        assert!(proj_row.alive);

        // kill forgets the project session (and kills the child)
        kill("proj-atlas").expect("kill the project session");
        assert!(session_state("proj-atlas").is_none());
        assert!(list().iter().all(|s| s.id != "proj-atlas"));
        assert!(matches!(kill("proj-atlas"), Err(KillError::Unknown)));
        assert!(!restart("proj-atlas"), "unknown session cannot be restarted");
    }

    #[test]
    fn pty_session_roundtrip_resize_kill_exit() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let argv = vec!["/bin/cat".to_string()];
            let result = (|| -> Result<(bool, Option<u32>, Option<i32>), String> {
                let mut s = PtySession::spawn(
                    &argv,
                    &std::env::temp_dir(),
                    &[("TERM", "xterm-256color")],
                    80,
                    24,
                )?;
                let pid = s.pid();
                let mut reader = s.take_reader().ok_or("no pty reader")?;
                s.write_all(b"hello-atlas\n")?;
                // the pty echoes the line (and cat repeats it) — read until seen
                let mut buf = [0u8; 512];
                let mut seen: Vec<u8> = Vec::new();
                while !seen.windows(11).any(|w| w == b"hello-atlas") {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => seen.extend_from_slice(&buf[..n]),
                        Err(e) => return Err(format!("read: {e}")),
                    }
                }
                let echoed = seen.windows(11).any(|w| w == b"hello-atlas");
                // resize must not panic
                s.resize(120, 40)?;
                s.kill()?;
                let mut exit = s.try_exit();
                for _ in 0..100 {
                    if exit.is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                    exit = s.try_exit();
                }
                Ok((echoed, pid, exit))
            })();
            let _ = tx.send(result);
        });
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok((echoed, pid, exit))) => {
                assert!(echoed, "pty must echo the written line back");
                assert!(pid.is_some(), "/bin/cat must report a pid");
                assert!(exit.is_some(), "killed child must be detected as exited");
            }
            Ok(Err(e)) => panic!("pty session failed: {e}"),
            Err(e) => panic!("pty session timed out: {e}"),
        }
    }
}
