//! «Этап B» — delegation of board items to workers.
//!
//! Three targets, one state machine (`items.delegation_state`):
//!
//! ```text
//!          ┌─────────── reject (feedback stored) ───────────┐
//!          ▼                                                │
//!   '' ⇄ queued ──delegate──▶ running ──turn done ─────────▶ review
//!                               │                           │
//!                               ├─ turn done, ВОПРОСЫ ──▶ needs-feedback
//!                               ├─ turn done, diff empty ─▶ failed
//!                               ├─ ход упал (exit≠0/EOF) ─▶ failed
//!                               └─ cancel владельцем ─────▶ failed
//!                                                           │
//!                                        accept ────────────┴──▶ done
//! ```
//!
//! `needs-feedback` is terminal-until-answered: the owner answers through
//! reject (the feedback rides into the next brief's «Фидбек» section), reset
//! wipes it, reap_stale leaves it alone; accept and a fresh delegate refuse
//! while the questions are unanswered. `failed` revives through the same
//! reject (→ `queued`, feedback stored, the failed run's own session file
//! stays recorded — the next delegate resumes it); reset still wipes.
//!
//! * `orchestrator` — a DEDICATED rpc child per delegation (same approval
//!   overlay as the panel child), cwd = the item's project directory. It shares
//!   nothing with the panel's interactive orchestrator — not the process, not
//!   the session, not the turn gate — so any number of delegations run at once
//!   and the chat stays free. A headless child has nobody to show an approval
//!   card to: a `prompt` rule fails the call (fail-closed) and the error lands
//!   in the run ring, which the journal and the review dialog display.
//! * `worktree` — `git -C <project> worktree add ~/.atlas-wt/<project>-<slug>
//!   -b task/<slug>`, then a headless `omp -p "<brief>" --cwd <worktree>`.
//! * `dir` — the same headless worker, directly in the project directory.
//!
//! Worker runs are in-process (like the make-task runner): the ring buffer
//! and the exit watcher die with the server. The CHILDREN, however, are
//! systemd transient units (`atlas-deleg-*`) whenever `systemd-run --user` is
//! available — they survive an atlas restart within the session, and cancel
//! goes through `systemctl stop` so the whole worker tree dies. `reap_stale`
//! asks `systemctl --user is-active` before settling an orphaned row; without
//! systemd-run everything is a plain direct spawn and orphaned
//! `queued`/`running` rows are marked `failed` on the next start, as before.
//!
//! Safety: the worker is `omp -p` (headless) with no approval UI, so tool calls
//! run unattended; only the guard's hard deny-rules (`~/.omp/agent/config.yml`:
//! `tools.approval`, `bash.patterns`) still block. The worktree isolation plus
//! the `diff` → `accept` review gate are the human control (see README).

use crate::cfg;
use crate::db;
use crate::omp_rpc;
use crate::runner;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Delegation targets, as the API spells them.
pub const TARGETS: [&str; 3] = ["orchestrator", "worktree", "dir"];
/// Caps of `GET …/diff` (line count and bytes; either trips `truncated`).
pub const DIFF_CAP_LINES: usize = 400;
pub const DIFF_CAP_BYTES: usize = 200_000;
/// Grace between SIGTERM and SIGKILL on cancel.
pub const KILL_GRACE: Duration = Duration::from_secs(5);

/// Supervision of a delegation rpc child: **наблюдение вместо казни**. No
/// timer ever kills a working child — a very long but useful operation (a
/// long think of a reasoning model, a big generation) must not die from
/// silence, and the old timers did exactly that: the 900 s hard cap stopped
/// delegation 12 mid-work, 15 s after its last tool call (max silence 76 s —
/// the idle watchdog never even fired). A run ends only honestly: process
/// exit, turn error, owner cancel, or the empty-work gate.
///
/// Instead, a ~30 s poller OBSERVES {child alive, frame age} and speaks only
/// on transitions: «наблюдатель: тишина N с, процесс жив» into the ring when
/// a live child goes quiet, «кадры пошли» when frames return, and — once per
/// run, when silence crosses `[delegation] silence_warn_secs` (default 300,
/// `0` = never) — ONE warn notification: possibly a long think; the owner
/// watches the live журнал and cancels by hand. No repetition spam.

/// Ring lines persisted into `items.delegation_tail` when a run reaches ANY
/// terminal — the in-memory ring dies with the server, and a restart must not
/// erase the evidence of a run (the 25/26 incidents left nothing behind).
const TAIL_LINES: usize = 50;

/// A failed delegation request, carrying the HTTP shape the handler needs.
#[derive(Debug)]
pub enum Error {
    /// 404 — unknown item.
    NotFound(String),
    /// 409 — busy (already delegated) or nothing to act on.
    Busy(String),
    /// 422 — the target needs a project with a path / bad target.
    NoPath(String),
    /// 422 — anything else the caller got wrong.
    Bad(String),
    /// 500 — filesystem/git/runner failure.
    Io(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Orchestrator,
    Worktree,
    Dir,
}

impl Target {
    pub fn parse(s: &str) -> Option<Target> {
        match s {
            "orchestrator" => Some(Target::Orchestrator),
            "worktree" => Some(Target::Worktree),
            "dir" => Some(Target::Dir),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Target::Orchestrator => "orchestrator",
            Target::Worktree => "worktree",
            Target::Dir => "dir",
        }
    }
}

// ------------------------------------------------------------------ item ----

/// Everything the brief and the target plan need about one board item.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: i64,
    pub title: String,
    pub project: Option<String>,
    pub description: Option<String>,
    /// `items.delegation_state` (`` = never delegated).
    pub state: String,
    pub target: Option<String>,
    /// `items.delegation_ref` (worktree dir / project path / atlas dir).
    pub reference: Option<String>,
    /// Project repo HEAD sha recorded at delegation start (`delegation_head`):
    /// the review gate's base. `None` = legacy row or the cwd is no git repo.
    pub head: Option<String>,
    /// Session file the previous run used (`delegation_session`) — the only
    /// thing a follow-up may resume.
    pub session: Option<String>,
    pub feedback: Option<String>,
    pub steps: Vec<(String, bool)>,
    pub files: Vec<String>,
}

pub struct Loaded {
    pub item: Item,
    pub project_path: Option<PathBuf>,
}

fn load(item_id: i64) -> Result<Loaded, Error> {
    let conn = db::db_static().lock();
    let mut stmt = conn
        .prepare(
            "SELECT title, project, description, delegation_state, delegation_target, \
             delegation_ref, delegation_feedback, delegation_head, delegation_session \
             FROM items WHERE id=?1",
        )
        .map_err(|e| Error::Io(format!("{e}")))?;
    let row = stmt
        .query_row([item_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })
        .map_err(|_| Error::NotFound(format!("item {item_id} not found")))?;
    let steps: Vec<(String, bool)> = conn
        .prepare("SELECT text, done FROM item_steps WHERE item_id=?1 ORDER BY pos, id")
        .and_then(|mut s| {
            s.query_map([item_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    let files: Vec<String> = conn
        .prepare("SELECT path FROM item_files WHERE item_id=?1 ORDER BY sort, rowid")
        .and_then(|mut s| {
            s.query_map([item_id], |r| r.get::<_, String>(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    let project_path = row
        .1
        .as_deref()
        .and_then(|p| project_path(&conn, p));
    Ok(Loaded {
        item: Item {
            id: item_id,
            title: row.0,
            project: row.1,
            description: row.2,
            state: row.3,
            target: row.4,
            reference: row.5,
            head: row.7,
            session: row.8,
            feedback: row.6,
            steps,
            files,
        },
        project_path,
    })
}

async fn load_bg(item_id: i64) -> Result<Loaded, Error> {
    tokio::task::spawn_blocking(move || load(item_id))
        .await
        .map_err(|e| Error::Io(format!("blocking task: {e}")))?
}

/// The `path` of a project row, trimmed; empty/null → None.
fn project_path(conn: &rusqlite::Connection, project: &str) -> Option<PathBuf> {
    conn.query_row("SELECT path FROM projects WHERE name=?1", [project], |r| {
        r.get::<_, Option<String>>(0)
    })
    .ok()
    .flatten()
    .map(|p| p.trim().to_string())
    .filter(|p| !p.is_empty())
    .map(PathBuf::from)
}

// ------------------------------------------------------------------ brief ----

/// The worker's prompt — the exact format the board/UI contract fixes.
/// The «Шаги»/«Файлы»/«Фидбек» sections are dropped when empty.
pub fn compile_brief(item: &Item, cwd: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n", item.title));
    match item
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        Some(d) => out.push_str(d),
        None => out.push_str("(без описания)"),
    }
    out.push('\n');
    // Сиблинги того же проекта — только при живой БД (тесты брифа её не
    // поднимают) и только когда проект у дела есть.
    if let Some(project) = item
        .project
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        if db::is_ready() {
            let conn = db::db_static().lock();
            out.push_str(&sibling_section(&conn, item, project));
        }
    }
    if !item.steps.is_empty() {
        out.push_str("## Шаги\n");
        for (text, done) in &item.steps {
            out.push_str(if *done { "- [x] " } else { "- [ ] " });
            out.push_str(text);
            out.push('\n');
        }
    }
    if !item.files.is_empty() {
        out.push_str("## Файлы (прочти перед работой)\n");
        for f in &item.files {
            out.push_str("- ");
            out.push_str(f);
            out.push('\n');
        }
    }
    if let Some(f) = item
        .feedback
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        out.push_str("## Фидбек к прошлой попытке\n");
        out.push_str(f);
        out.push('\n');
    }
    if cwd.join("graft").exists() {
        out.push_str(
            "В проекте есть graft/ — граф контекста кода. Для ориентации используй \
             навык graft, а не чтение репозитория вслепую.\n",
        );
    }
    out.push_str(&format!(
        "Работай в каталоге {}. Ты работаешь автономно: ответить на вопрос некому — \
         если данных не хватает, остановись, заполни раздел ВОПРОСЫ, ничего не ломай.\n",
        cwd.display()
    ));
    out.push_str(
        "## Проверка\n\
         По ходу работы проверяй как обычно (сборка, точечные тесты). ФИНАЛЬНУЮ \
         верификацию воркер не гоняет — её при приёмке выполняет гейт atlas.\n\
         В ПРОВЕРЕНО перечисли команды гейта: по одной на строку, префикс \"$ \" \
         (найди сам: Makefile-цели, cargo, go, npm/pnpm, pytest…).\n",
    );
    let hints = verify_hints(&item.files);
    if !hints.is_empty() {
        out.push_str("Похоже на:\n");
        for h in &hints {
            out.push_str("- ");
            out.push_str(h);
            out.push('\n');
        }
    }
    out.push_str(
        "Нечего проверять — напиши в ПРОВЕРЕНО, почему.\n",
    );
    out.push_str(
        "## Итог (обязательно)\n\
         Последнее сообщение хода — ровно четыре раздела, один за другим:\n\
         СДЕЛАНО: что сделано по делу\n\
         ИЗМЕНЕНО: перечень изменённых файлов\n\
         ПРОВЕРЕНО: команды проверки для гейта приёмки ($-строки) или почему нечего\n\
         ВОПРОСЫ: вопросы по делу; нечего спрашивать — напиши «нет»\n\
         Без этого блока работа не считается завершённой. Вместо догадки — ВОПРОСЫ.\n",
    );
    out
}

// ------------------------------------------------ sibling run context ----

/// «## Параллельная работа»: up to TWO most recent FINISHED runs of OTHER
/// items of the same project — the заголовок plus the first 600 characters of
/// the итог (whitespace flattened). omp has no session merge; this
/// brief-level slice is the deliberate substitute: a parallel worker sees
/// WHAT siblings concluded, never their full history.
fn sibling_section(conn: &rusqlite::Connection, item: &Item, project: &str) -> String {
    let rows = conn
        .prepare(
            "SELECT title, delegation_final FROM items \
             WHERE project = ?1 AND id != ?2 \
               AND delegation_final IS NOT NULL AND delegation_ended IS NOT NULL \
             ORDER BY delegation_ended DESC LIMIT 2",
        )
        .and_then(|mut s| {
            s.query_map(rusqlite::params![project, item.id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
        });
    let rows = match rows {
        Ok(rows) if !rows.is_empty() => rows,
        _ => return String::new(),
    };
    let mut out = String::from(
        "\n## Параллельная работа\nНедавно законченные другие дела этого проекта — их итоги:\n",
    );
    for (title, final_text) in rows {
        let flat = final_text.split_whitespace().collect::<Vec<_>>().join(" ");
        let clipped: String = flat.chars().take(600).collect();
        out.push_str(&format!("- {title}: {clipped}\n"));
    }
    out
}

/// Verify-command hints for the brief's «Проверка» section, derived from the
/// item's attached file names (Cargo.toml → cargo, go.mod → go…). The worker
/// still discovers the exact commands itself — the hints only point.
fn verify_hints(files: &[String]) -> Vec<String> {
    let has = |name: &str| files.iter().any(|f| f.rsplit('/').next() == Some(name));
    let mut hints = Vec::new();
    if has("Cargo.toml") {
        hints.push("Rust — cargo build && cargo test".into());
    }
    if has("go.mod") {
        hints.push("Go — go build ./... && go test ./...".into());
    }
    if has("package.json") {
        hints.push("JS/TS — тесты пакетным менеджером проекта (npm/pnpm/yarn run test)".into());
    }
    if has("Makefile") {
        hints.push("make — цели test/check, если объявлены".into());
    }
    if has("pyproject.toml") || has("requirements.txt") {
        hints.push("Python — pytest".into());
    }
    hints
}

/// The gate's verify commands the worker listed in ПРОВЕРЕНО: lines with a
/// `$ ` prefix (`- $ cargo test` tolerated), longest-first trimming of the
/// list decoration; everything else in the section is prose and ignored.
/// At most 8 commands, each capped — the gate runs what fits on one screen.
fn gate_commands_from_checked(checked: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in checked.lines() {
        let t = line.trim().trim_start_matches(['-', '*', '#']).trim();
        let Some(cmd) = t.strip_prefix('$').map(str::trim) else {
            continue;
        };
        let cmd = cmd.trim_matches('`').trim();
        if cmd.is_empty() {
            continue;
        }
        out.push(crate::util::short(cmd, 200));
        if out.len() == 8 {
            break;
        }
    }
    out
}

/// The gate's fallback when the worker listed no `$`-commands: the same
/// ecosystem probe as `verify_hints`, but as EXECUTABLE commands — only the
/// ones whose exit code is meaningful without extra arguments. A Makefile
/// does not qualify (unknown targets would fail the gate dishonestly).
fn gate_fallback_commands(files: &[String]) -> Vec<String> {
    let has = |name: &str| files.iter().any(|f| f.rsplit('/').next() == Some(name));
    let mut cmds = Vec::new();
    if has("Cargo.toml") {
        cmds.push("cargo test".into());
    }
    if has("go.mod") {
        cmds.push("go test ./...".into());
    }
    if has("package.json") {
        cmds.push("npm test".into());
    }
    if has("pyproject.toml") || has("requirements.txt") {
        cmds.push("pytest".into());
    }
    cmds
}

/// Parse the worker's final-message contract out of the turn's assistant
/// text: the four mandatory headers (`СДЕЛАНО:` / `ИЗМЕНЕНО:` / `ПРОВЕРЕНО:` /
/// `ВОПРОСЫ:`), each section running to the next header. A header is a line
/// whose trimmed form (tolerating `#`/`*` decoration) starts with the token.
/// `None` = no contract block at all — the model ignored the brief; the
/// review card then shows the raw tail and the diff decides the state.
pub fn parse_final_block(text: &str) -> Option<FinalReport> {
    const TOKENS: [&str; 4] = ["СДЕЛАНО", "ИЗМЕНЕНО", "ПРОВЕРЕНО", "ВОПРОСЫ"];
    // content clean-up: same-line remainder after the token (`:`, markdown
    // bold), and the per-line trim used for section bodies
    let clean = |s: &str| {
        s.trim_start_matches(':').trim().trim_matches('*').trim().to_string()
    };
    let mut report: Option<FinalReport> = None;
    let mut section: Option<usize> = None;
    for line in text.lines() {
        // header detection tolerates decoration: `#`, bullets, `**bold**`
        let t = line
            .trim()
            .trim_start_matches(['#', '*', '-', ' '])
            .trim();
        let header = TOKENS
            .iter()
            .position(|tok| t.starts_with(tok))
            .map(|i| (i, clean(&t[TOKENS[i].len()..])));
        if let Some((i, rest)) = header {
            // a (repeated) header STARTS its section: same-line content, then
            // following lines until the next header
            let rep = report.get_or_insert_with(FinalReport::default);
            *rep.section_mut(i) = rest;
            section = Some(i);
            continue;
        }
        if let (Some(rep), Some(i)) = (&mut report, section) {
            let s = rep.section_mut(i);
            let body = clean(t);
            if !body.is_empty() {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(&body);
            }
        }
    }
    report
}

/// The four sections of the final-message contract.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FinalReport {
    pub done: String,
    pub changed: String,
    pub verified: String,
    pub questions: String,
}

impl FinalReport {
    fn section_mut(&mut self, i: usize) -> &mut String {
        match i {
            0 => &mut self.done,
            1 => &mut self.changed,
            2 => &mut self.verified,
            _ => &mut self.questions,
        }
    }

    /// The brief tells the worker to write «нет» when there is nothing to ask —
    /// that honest «нет» (any case, with or without the period) is NOT a
    /// question; an empty section isn't either. Anything else needs an answer.
    pub fn questions_empty(&self) -> bool {
        let first = self.questions.trim().lines().next().unwrap_or("").trim();
        let norm = first
            .trim_matches(['.', '!', '—', '-', '«', '»', ' '])
            .to_lowercase();
        matches!(
            norm.as_str(),
            "" | "нет" | "no" | "none" | "n/a" | "нет вопросов" | "вопросов нет"
        )
    }
}

// ------------------------------------------------------- slug and worktree ----

/// `<item id>-<4 hex chars of the title hash>` — stable for item+title, safe
/// in a path and a git ref (no spaces, no slashes).
pub fn slug(id: i64, title: &str) -> String {
    format!("{id}-{}", short_hash(title))
}

/// FNV-1a 32 of the title, first four hex chars.
fn short_hash(s: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")[..4].to_string()
}

/// The transient unit of one delegation: `atlas-deleg-<item>-<hash4 of
/// title>` — the same stable item+title derivation as [`slug`], and a valid
/// unit name by construction: ASCII only (an integer id and hex hash — the
/// title itself never lands in the name), no slashes, no spaces. `reap_stale`
/// rebuilds it from the DB row to query systemd for orphaned workers.
pub fn unit_name(item_id: i64, title: &str) -> String {
    format!("atlas-deleg-{}", slug(item_id, title))
}

/// `<worktree root>/<project>-<slug>` (root: config `paths.worktrees`,
/// default `~/.atlas-wt`).
pub fn worktree_dir(project: &str, id: i64, title: &str) -> PathBuf {
    cfg::worktree_root().join(format!("{project}-{}", slug(id, title)))
}

/// Marker file that makes a worktree self-describing: importers ask the file
/// instead of parsing the dir name back (FRAGILE §2).
pub const DELEG_MARKER: &str = ".atlas-deleg.json";

/// Write `.atlas-deleg.json` into a fresh worktree: the item and project it
/// serves, plus the branch when one is already known. Worktree creation is
/// single-writer per dir (`git worktree add` either won or failed the start),
/// so a plain write is atomic enough; a failure only drops this worktree back
/// to the legacy name-template recognition, so it is logged, not fatal.
fn write_deleg_marker(dir: &Path, item: i64, project: &str, branch: Option<&str>) {
    let mut marker = json!({ "item": item, "project": project });
    if let Some(b) = branch {
        marker["branch"] = json!(b);
    }
    if let Err(e) = std::fs::write(dir.join(DELEG_MARKER), marker.to_string()) {
        log::warn!(
            "delegate: {} не записан: {e}",
            dir.join(DELEG_MARKER).display()
        );
    }
}

// ------------------------------------------------------------------ plan ----

/// Where a delegation will work, resolved BEFORE anything is started (the
/// preview endpoint returns exactly this).
#[derive(Debug)]
struct Plan {
    cwd: PathBuf,
    /// `delegation_ref`: worktree dir (worktree), project path (dir), atlas dir
    /// (orchestrator — where the rpc child runs).
    reference: String,
    /// `task/<slug>` for the worktree target.
    branch: Option<String>,
    /// The project checkout a worktree is added to.
    project_path: Option<PathBuf>,
}

fn no_path(item: &Item, target: Target) -> Error {
    match item.project.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => Error::NoPath(format!(
            "у проекта '{p}' нет пути — цель «{}» некуда разворачивать",
            target.as_str()
        )),
        None => Error::NoPath(format!(
            "у дела {} нет проекта — цель «{}» требует путь проекта",
            item.id,
            target.as_str()
        )),
    }
}

fn plan(target: Target, loaded: &Loaded) -> Result<Plan, Error> {
    let item = &loaded.item;
    match target {
        Target::Orchestrator => {
            // A delegation gets its own detached rpc child, so it needs a real
            // working directory: the item's project checkout — never the atlas
            // repo the panel's chat child happens to live in. That mistake made
            // the review diff empty and the work invisible.
            let path = loaded
                .project_path
                .clone()
                .ok_or_else(|| no_path(item, target))?;
            Ok(Plan {
                reference: path.to_string_lossy().into_owned(),
                cwd: path,
                branch: None,
                project_path: loaded.project_path.clone(),
            })
        }
        Target::Dir => {
            let path = loaded
                .project_path
                .clone()
                .ok_or_else(|| no_path(item, target))?;
            Ok(Plan {
                reference: path.to_string_lossy().into_owned(),
                cwd: path.clone(),
                branch: None,
                project_path: Some(path),
            })
        }
        Target::Worktree => {
            let path = loaded
                .project_path
                .clone()
                .ok_or_else(|| no_path(item, target))?;
            let dir = worktree_dir(item.project.as_deref().unwrap_or(""), item.id, &item.title);
            Ok(Plan {
                reference: dir.to_string_lossy().into_owned(),
                cwd: dir,
                branch: Some(format!("task/{}", slug(item.id, &item.title))),
                project_path: Some(path),
            })
        }
    }
}

// ------------------------------------------------------------------ jobs ----

/// One live delegation: the run handle plus the flag the cancel path raises so
/// the exit/turn watcher does not overwrite the «failed» cancel state.
struct Job {
    run: Arc<runner::Run>,
    cancelled: Arc<AtomicBool>,
    /// The child's transient unit (`atlas-deleg-…`) in systemd spawn mode;
    /// `None` = plain direct spawn, the pre-systemd teardown applies.
    unit: Option<String>,
}

/// The newest job per item id (kept after it finishes so `/run` can still show
/// the ring).
static JOBS: LazyLock<Mutex<HashMap<i64, Arc<Job>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn jobs() -> &'static Mutex<HashMap<i64, Arc<Job>>> {
    &JOBS
}

fn job(item_id: i64) -> Option<Arc<Job>> {
    runner::lock(jobs()).get(&item_id).cloned()
}

fn put_job(item_id: i64, job: Arc<Job>) {
    runner::lock(jobs()).insert(item_id, job);
}

/// The SIGTERM step of a delegation teardown. Unit mode goes through
/// `systemctl --user stop`: `KillMode=control-group` signals the whole tree,
/// grandchildren included — a bare SIGTERM to the worker pid would orphan
/// them. Direct mode SIGTERMs the pid, exactly as before.
fn stop_child(unit: Option<&str>, pid: u32) {
    match unit {
        Some(u) => {
            omp_rpc::unit_stop(u);
        }
        None => {
            runner::sigterm(pid);
        }
    }
}

/// The unit-mode twin of `runner::spawn` (which owns the direct mode and pins
/// `Run::new(child.id())`): spawn the systemd-run invocation, resolve the
/// REAL worker pid from the unit, and wire ring readers + exit watcher to
/// that pid — so the cancel/watchdog machinery keeps working off `run.pid`.
/// The exit code is the invocation's, which `--pipe` propagates from the
/// service (systemd-run lingers as the fd holder until the service's stdio
/// closes, so the ring sees the worker's output, not the journal).
fn spawn_unit_worker(
    unit: &str,
    run_id: i64,
    argv: &[String],
    cwd: &Path,
    on_exit: impl FnOnce(i32) + Send + 'static,
) -> std::io::Result<Arc<runner::Run>> {
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let Some(pid) = omp_rpc::main_pid(unit) else {
        let _ = child.kill();
        return Err(std::io::Error::other(format!(
            "юнит {unit} не получил процесс (MainPID=0)"
        )));
    };
    let run = runner::Run::with_id(run_id, pid);
    if let Some(out) = child.stdout.take() {
        runner::spawn_reader(out, Arc::clone(&run));
    }
    if let Some(err) = child.stderr.take() {
        runner::spawn_reader(err, Arc::clone(&run));
    }
    let monitored = Arc::clone(&run);
    std::thread::spawn(move || {
        let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        monitored.finish(code);
        on_exit(code);
    });
    Ok(run)
}

/// The transient unit for a new delegation — `Some` in systemd spawn mode,
/// `None` for exactly the old direct spawn.
fn new_unit(item_id: i64, title: &str) -> Option<String> {
    matches!(*omp_rpc::SPAWN_MODE, omp_rpc::SpawnMode::Systemd)
        .then(|| unit_name(item_id, title))
}

/// The session pin for one delegation run. FRESH by default — its own
/// per-run session dir (the run's dossier folder: everything about one run
/// lives together), never the project's latest session (that auto-continue
/// drowned item 24 in three weeks of history). Resume only as a true
/// follow-up: the owner's feedback exists AND the previous run's session
/// file is recorded. No size cap: the follow-up resumes its own run's
/// history, never alien sessions. Fresh sessions are minted by omp INSIDE
/// the dir (omp 18.9: no `--session <file>`) — [`spawn_session_resolver`]
/// pins what actually landed into `delegation_session` for the next resume.
fn session_decision(
    feedback: Option<&str>,
    stored: Option<&str>,
    run_session: &Path,
) -> omp_rpc::Session {
    let has_feedback = feedback.map(|f| !f.trim().is_empty()).unwrap_or(false);
    if has_feedback {
        if let Some(p) = stored.map(Path::new).filter(|p| p.exists()) {
            return omp_rpc::Session::Resume(p.to_path_buf());
        }
    }
    omp_rpc::Session::Fresh(run_session.to_path_buf())
}

/// The per-run session pin of one delegation — a PLACEHOLDER inside the
/// run's dossier dir: omp 18.9 mints fresh sessions inside a `--session-dir`
/// under its own name, so the file name here is advisory; build_argv passes
/// the dir, and [`spawn_session_resolver`] pins the real file into
/// `delegation_session` right after the spawn. Distinct per run so a "fresh"
/// delegation truly starts clean.
fn run_session_path(item_id: i64, _title: &str, run_id: i64) -> PathBuf {
    runs_dir(item_id, run_id).join("session.jsonl")
}

/// Pin the session file omp actually created for a FRESH spawn into
/// `delegation_session` (guarded by run_id): a detached one-shot thread a
/// couple of seconds after the spawn. Without this, the next follow-up would
/// try to resume a placeholder path omp never wrote.
fn spawn_session_resolver(item_id: i64, run_id: i64, dir: PathBuf, started: std::time::SystemTime) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        match omp_rpc::resolve_fresh_session(&dir, started) {
            Some(p) => {
                let conn = db::db_static().lock();
                if let Err(e) = conn.execute(
                    "UPDATE items SET delegation_session=?1 \
                     WHERE id=?2 AND delegation_run_id=?3",
                    rusqlite::params![p.to_string_lossy(), item_id, run_id],
                ) {
                    log::warn!("delegate: item {item_id}: session pin write failed: {e}");
                }
            }
            None => log::warn!(
                "delegate: item {item_id}: свежая сессия не найдена в {} — follow-up начнёт новую",
                dir.display()
            ),
        }
    });
}

// ------------------------------------------ fork of the project session ----

/// A cwd → its omp dirslug name under the sessions root. micelium's
/// `dispatch.slugDir` (dispatch.go) replaces every `/` with `-`; the files on
/// disk refine that in one point: a cwd UNDER the home drops the home prefix
/// but keeps the leading separator — `/home/gwushor/Projects/atlas` lives in
/// `-Projects-atlas`, never in `-home-gwushor-Projects-atlas` (verified
/// against `~/.omp/agent/sessions`: `-Work`, `-vault`, `-Projects`, `-tmp`,
/// `--run-media-…`). Every cwd outside the home keeps micelium's plain
/// replace (`/tmp/x` → `-tmp-x`).
fn dirslug(home: &str, cwd: &str) -> String {
    let rel = match cwd.strip_prefix(home) {
        Some(rest) if !rest.is_empty() && rest.starts_with('/') => rest,
        _ => cwd,
    };
    rel.replace('/', "-")
}

/// The session-level facts of one omp session file (its `type:"session"`
/// line) — a minimal mirror of micelium's `omplog.ParseHeader`.
struct SessionHead {
    id: String,
    cwd: String,
    title: String,
}

/// Read only the session-level fields (first lines, the `type:"session"`
/// record) — oversized or odd lines are skipped, never fatal.
fn parse_session_head(file: &Path) -> Option<SessionHead> {
    let f = std::fs::File::open(file).ok()?;
    let mut id = None;
    let (mut cwd, mut title) = (String::new(), String::new());
    for line in std::io::BufReader::new(f).lines().take(100) {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v.get("type").and_then(|t| t.as_str()) == Some("session") {
            id = v.get("id").and_then(|i| i.as_str()).map(String::from);
            cwd = v
                .get("cwd")
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .into();
            title = v
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .into();
            break;
        }
    }
    id.map(|id| SessionHead { id, cwd, title })
}

/// The project's CURRENT session: the newest `*.jsonl` (mtime first) in the
/// cwd's dirslug that carries a session id — micelium's `newestInSlug`
/// semantics (dispatch.go), one slug dir, no scan of foreign slugs.
fn find_project_session(root: &Path, home: &str, cwd: &str) -> Option<(PathBuf, SessionHead)> {
    let dir = root.join(dirslug(home, cwd));
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| {
            let e = e.ok()?;
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                return None;
            }
            let m = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((m, p))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files
        .into_iter()
        .find_map(|(_, p)| parse_session_head(&p).map(|h| (p, h)))
}

/// n random hex characters (/dev/urandom — no new dependency).
fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; (n + 1) / 2];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .unwrap_or_else(|e| panic!("rand source: {e}"));
    b.iter().map(|x| format!("{x:02x}")).collect::<String>()[..n].to_string()
}

/// An omp-shaped session id: 8-4-4-4-12 hex with a time-ordered prefix —
/// micelium's `newSessionID` (fork.go) mirrored exactly.
fn new_session_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    format!(
        "{:08x}-{}-{}-{}-{}",
        (ms >> 8) & 0xffff_ffff,
        rand_hex(4),
        rand_hex(4),
        rand_hex(4),
        rand_hex(12)
    )
}

fn write_jsonl_line(out: &mut impl std::io::Write, line: &str) -> Result<(), String> {
    out.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    out.write_all(b"\n").map_err(|e| e.to_string())
}

/// Copy the project session into the run's dossier dir under a NEW session id
/// — micelium's `seededFork` (fork.go), tip variant: a FULL copy, no
/// truncation. Lines 1/2 are rewritten the way omp itself writes a lineage: a
/// fresh `title` («Fork: …») and a `session` record with the new id plus
/// `previousSessionFiles` back to the original; every other record keeps its
/// bytes except the carried `"sessionId"` — OLD → NEW. The original file is
/// never touched. 20 MB+ sources still copy, with a warn.
fn fork_session_file(src: &Path, old: &SessionHead, dest_dir: &Path) -> Result<PathBuf, String> {
    if let Ok(m) = std::fs::metadata(src) {
        if m.len() > 20 << 20 {
            log::warn!(
                "delegate: сессия проекта {} — {} МБ, копирую целиком",
                src.display(),
                m.len() >> 20
            );
        }
    }
    let new_id = new_session_id();
    // micelium's name: <UTC stamp, ':'→'-'>_<id>.jsonl — the shape omp mints.
    let dest = dest_dir.join(format!(
        "{}_{new_id}.jsonl",
        cfg::util::now_iso_utc().replace(':', "-")
    ));
    let fork_title = if old.title.starts_with("Fork: ") {
        old.title.clone()
    } else {
        format!("Fork: {}", old.title)
    };
    let now = cfg::util::now_iso_utc();
    let title_line = json!({
        "type": "title", "v": 1, "title": fork_title, "source": "auto", "updatedAt": now,
    })
    .to_string();
    let session_line = json!({
        "type": "session", "version": 3, "id": new_id, "timestamp": now,
        "cwd": old.cwd, "title": fork_title,
        "previousSessionFiles": [src.to_string_lossy()],
    })
    .to_string();
    let src_token = format!("\"sessionId\":\"{}\"", old.id);
    let dst_token = format!("\"sessionId\":\"{new_id}\"");

    let tmp = dest_dir.join(format!(".{new_id}.jsonl.tmp"));
    let write = || -> Result<(), String> {
        let input = std::fs::File::open(src).map_err(|e| e.to_string())?;
        let mut out = std::io::BufWriter::new(
            std::fs::File::create(&tmp).map_err(|e| e.to_string())?,
        );
        write_jsonl_line(&mut out, &title_line)?;
        write_jsonl_line(&mut out, &session_line)?;
        for line in std::io::BufReader::new(input).lines() {
            let line = line.map_err(|e| e.to_string())?;
            if line.is_empty() {
                continue;
            }
            // the original's own identity lines die here — micelium's scanner
            // drops them the same way (entry lines only); the fork speaks
            // with the fresh title/session pair written above
            if line.starts_with("{\"type\":\"title\"") || line.starts_with("{\"type\":\"session\"") {
                continue;
            }
            let line = if line.contains(&src_token) {
                line.replace(&src_token, &dst_token)
            } else {
                line
            };
            write_jsonl_line(&mut out, &line)?;
        }
        out.flush().map_err(|e| e.to_string())
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(dest)
}

/// The fork point of a FRESH delegation start: the project's current omp
/// session (its dirslug, newest file) becomes the run's session — a seeded
/// copy under a new id, resumed by path. `None` = no project session (a new
/// project) or the copy failed — the caller keeps today's fresh
/// `--session-dir` spawn. The original session is never modified.
fn fork_project_session(plan_cwd: &Path, dest_dir: &Path) -> Option<PathBuf> {
    let root = cfg::sessions_dir();
    let home = cfg::home();
    let home = home.to_string_lossy();
    let cwd = plan_cwd.to_string_lossy();
    let (src, head) = find_project_session(&root, &home, &cwd)?;
    match fork_session_file(&src, &head, dest_dir) {
        Ok(p) => {
            log::info!(
                "delegate: свежий старт — вилка сессии проекта {} → {}",
                src.display(),
                p.display()
            );
            Some(p)
        }
        Err(e) => {
            log::warn!("delegate: вилка сессии проекта не удалась ({e}) — стартую чистую");
            None
        }
    }
}

/// The `--model <m>` argv segment for a delegation worker, from
/// `[delegation] model`; empty (the default) omits the flag — omp's default
/// model applies, so the config-dev instance is unaffected.
fn model_args() -> Vec<String> {
    let m = cfg::config().delegation_model.trim();
    if m.is_empty() {
        Vec::new()
    } else {
        vec!["--model".to_string(), m.to_string()]
    }
}

// ------------------------------------------------------- reviewer ----

/// The fresh-context reviewer model, from `[delegation] reviewer`. Empty (the
/// default) = the reviewer is OFF — the owner's judgment stays the only gate;
/// a configured model adds a second opinion without a veto.
fn reviewer_model() -> Option<String> {
    let m = cfg::config().delegation_reviewer.trim().to_string();
    (!m.is_empty()).then_some(m)
}

/// The reviewer's prompt: a cold second pair of eyes over the frozen run.
/// The reviewer works in the SAME directory the worker did (delegation_ref),
/// reads the diff against the recorded start HEAD itself, and answers the
/// fixed VERDICT contract. The machine gate's results ride in the prompt —
/// the reviewer treats them as the truth about «проверено» and runs nothing
/// itself. Advisory only — the verdict never changes state.
fn review_prompt(head: &str, final_report: Option<&str>, gate_summary: Option<&str>) -> String {
    let report = final_report
        .map(|t| cap(t, 80, 6_000).0)
        .unwrap_or_else(|| "(отчёт воркера не сохранился)".to_string());
    let gate = gate_summary
        .map(|g| cap(g, 40, 2_000).0)
        .unwrap_or_else(|| "не проводилась".to_string());
    format!(
        "Ты — независимый ревьюер кода. Ты НЕ продолжаешь работу автора изменений: \
у тебя холодный контекст, оценивай только то, что видно в репозитории.\n\n\
Прогон воркера шёл в этом репозитории; стартовая точка — git HEAD {head}.\n\
Изменения воркера: `git diff {head}` (покрывает и коммиты, и незакоммиченное), \
новые файлы могут быть untracked — смотри `git status --porcelain`.\n\n\
Машинная верификация (гейт atlas; это истина о «проверено», сам проверки НЕ гоняй):\n{gate}\n\n\
Отчёт воркера (контракт СДЕЛАНО/ИЗМЕНЕНО/ПРОВЕРЕНО/ВОПРОСЫ):\n{report}\n\n\
Задача:\n\
1. Прочитай diff и проверь, отвечает ли он отчёту воркера.\n\
2. Проверь очевидные риски: правки не по делу, мусорные файлы, незакрытые проблемы; \
упавшая верификация гейта — блок.\n\n\
Ответ строго в формате, первая строка — вердикт, без markdown-заборов:\n\
VERDICT: accept   (или block)\n\
Затем до 5 строк причин.\n"
    )
}

/// The verdict word of the reviewer's answer: the first line whose trimmed
/// form (tolerating `#`/`*` decoration, like the final-block parser) starts
/// with `VERDICT:`/`ВЕРДИКТ:` (models drift to Cyrillic despite the
/// contract); the first word after the colon wins. `None` = no verdict
/// line — the answer still lands in the column, the card shows it verbatim.
fn review_verdict(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim().trim_start_matches(['#', '*']).trim();
        let rest = t
            .strip_prefix("VERDICT:")
            .or_else(|| t.strip_prefix("ВЕРДИКТ:"));
        if let Some(rest) = rest {
            return rest
                .split_whitespace()
                .next()
                .map(|w| w.trim_matches(['.', ',']).to_lowercase());
        }
    }
    None
}

/// The fresh-context reviewer of a settled run: a SECOND headless `omp -p`
/// session — cold context, never the worker's session, never a continuation —
/// re-reads the frozen diff and answers VERDICT into `delegation_review`.
/// Spawned whenever a run settles in `review` and `[delegation] reviewer`
/// names a model; its own thread, its own process, no timers (the owner sees
/// the answer in the card / the notification whenever it lands). Advisory:
/// no state change, no veto — accept/reject stay the owner's.
fn spawn_reviewer(item_id: i64, run_id: i64) {
    let Some(model) = reviewer_model() else { return };
    log::info!("delegate: item {item_id}: запуск ревьюера ({model}) по прогону r{run_id}");
    std::thread::Builder::new()
        .name(format!("atlas-review-{item_id}-r{run_id}"))
        .spawn(move || run_reviewer(item_id, run_id, model))
        .map(|_| ())
        .unwrap_or_else(|e| log::warn!("delegate: reviewer thread spawn failed: {e}"));
}

/// The reviewer's body: read THIS run's frozen base from the DB (guarded by
/// run_id + state), run the cold `omp -p`, write the answer back under the
/// same guard — a newer run or an already-accepted item is never touched.
fn run_reviewer(item_id: i64, run_id: i64, model: String) {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = {
        let conn = db::db_static().lock();
        conn.query_row(
            "SELECT delegation_ref, delegation_head, delegation_final FROM items \
             WHERE id=?1 AND delegation_run_id=?2 AND delegation_state='review'",
            rusqlite::params![item_id, run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()
    };
    // no recorded base (dir/HEAD) — nothing to review against, honestly off
    let Some((Some(dir), Some(head), final_text)) = row else {
        log::info!("delegate: item {item_id}: ревьюер пропущен — прогон r{run_id} уже не в review или нет базы");
        return;
    };
    // the gate's verdict is the truth about «проверено»: the reviewer reads it
    // and does NOT run the checks itself (no double verification)
    let gate_summary = read_gate_summary(item_id, run_id);
    let prompt = review_prompt(&head, final_text.as_deref(), gate_summary.as_deref());
    // fresh review session inside the run's dossier dir (omp 18.9 mints the
    // file itself; a one-shot review never resumes anything)
    let session_dir = runs_dir(item_id, run_id);
    let _ = std::fs::create_dir_all(&session_dir);
    let argv = vec![
        cfg::OMP_BIN.clone(),
        "--model".to_string(),
        model.clone(),
        "-p".to_string(),
        prompt,
        "--session-dir".to_string(),
        session_dir.to_string_lossy().into_owned(),
        "--cwd".to_string(),
        dir.clone(),
    ];
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();
    let (ok, text) = match out {
        Ok(o) => {
            let mut t = String::from_utf8_lossy(&o.stdout).into_owned();
            if t.trim().is_empty() {
                t = String::from_utf8_lossy(&o.stderr).into_owned();
            }
            (o.status.success(), t)
        }
        Err(e) => (false, format!("omp не запустился: {e}")),
    };
    let mut body = cap(text.trim(), 80, 4_000).0;
    if body.is_empty() {
        body = "(ревьюер ответил пустотой)".to_string();
    } else if !ok {
        body = format!("(ревьюер упал)\n{body}");
    }
    let stored = {
        let conn = db::db_static().lock();
        conn.execute(
            "UPDATE items SET delegation_review=?1, updated_at=?2 \
             WHERE id=?3 AND delegation_run_id=?4 AND delegation_state='review'",
            rusqlite::params![body, crate::util::now(), item_id, run_id],
        )
        .unwrap_or(0)
    };
    if stored == 0 {
        log::info!("delegate: item {item_id}: ответ ревьюера не записан — прогон r{run_id} уже не в review");
        return;
    }
    let verdict = review_verdict(&body).unwrap_or_else(|| "без вердикта".into());
    log::info!("delegate: item {item_id}: второе мнение ревьюера готово — {verdict}");
    // the dossier copy (the DB column above is the review-card showcase)
    write_artifact(
        &runs_dir(item_id, run_id),
        "review.md",
        &format!("# второе мнение ревьюера ({model}) — {verdict}\n\n{body}\n"),
    );
    let conn = db::db_static().lock();
    let project: Option<String> = conn
        .query_row("SELECT project FROM items WHERE id=?1", [item_id], |r| r.get(0))
        .unwrap_or(None);
    let _ = db::insert_notification(
        &conn,
        "info",
        project.as_deref(),
        &format!("делегация дела {item_id}: второе мнение ревьюера — {verdict}"),
    );
    drop(conn);
    // the second signal may complete the picture (gate ok + accept → close,
    // block → retry): whichever lands last decides
    try_autopilot(item_id, run_id);
}

// ------------------------------------------------------------- gate ----

/// One machine-check result of the acceptance gate.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GateStep {
    pub command: String,
    pub ok: bool,
    pub tail: String,
}

/// The gate report: what ran, did it all pass. Lives in the run dossier
/// (`gate-results.json`) and shows up in /run and on the review card.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GateReport {
    pub ran: usize,
    pub ok: bool,
    pub results: Vec<GateStep>,
}

/// Per-command budget of the gate; a stuck check fails its slot, the gate
/// moves on. The whole run is bounded by the step count times this.
const GATE_STEP_TIMEOUT: Duration = Duration::from_secs(300);

/// Read the run's gate report (the persisted form in the dossier); absent —
/// the gate has not run yet (or never ran for this run).
fn read_gate_report(item_id: i64, run_id: i64) -> Option<GateReport> {
    let text = std::fs::read_to_string(
        runs_dir(item_id, run_id).join("gate-results.json"),
    )
    .ok()?;
    serde_json::from_str(&text).ok()
}

/// One-line summary of the gate report for the reviewer's prompt.
fn read_gate_summary(item_id: i64, run_id: i64) -> Option<String> {
    let r = read_gate_report(item_id, run_id)?;
    if r.ran == 0 {
        return Some("не проводилась (воркер не перечислил команды)".to_string());
    }
    let mut s = format!("{} из {} проверок прошло:", r.results.iter().filter(|x| x.ok).count(), r.ran);
    for step in &r.results {
        s.push_str(&format!(
            "\n{} {}{}",
            if step.ok { "ok:" } else { "FAIL:" },
            step.command,
            if step.ok {
                String::new()
            } else {
                format!(" — {}", crate::util::short(&step.tail, 160))
            }
        ));
    }
    Some(s)
}

/// The machine gate of an accepted-ready run: runs the verify commands the
/// worker listed in ПРОВЕРЕНО (`$`-lines; ecosystem fallback when none) in
/// the run's directory, writes `gate-results.json` into the dossier, pushes
/// a `gate` event for the open review card, and ONLY THEN hands the run to
/// the reviewer (whose prompt carries the results — no double verification:
/// the worker reports, the gate measures, the reviewer judges).
fn spawn_gate(item_id: i64, run_id: i64) {
    std::thread::Builder::new()
        .name(format!("atlas-gate-{item_id}-r{run_id}"))
        .spawn(move || {
            let report = run_gate(item_id, run_id);
            let failed = report.as_ref().map(|r| r.ran > 0 && !r.ok).unwrap_or(false);
            if let Some(r) = report {
                write_artifact(
                    &runs_dir(item_id, run_id),
                    "gate-results.json",
                    &serde_json::to_string(&r).unwrap_or_default(),
                );
                crate::http::web::events::publish(
                    &serde_json::json!({"type": "gate", "id": item_id, "ok": r.ok, "ran": r.ran}),
                );
                if failed {
                    let conn = db::db_static().lock();
                    let project: Option<String> = conn
                        .query_row("SELECT project FROM items WHERE id=?1", [item_id], |row| row.get(0))
                        .unwrap_or(None);
                    let first_fail = r
                        .results
                        .iter()
                        .find(|s| !s.ok)
                        .map(|s| crate::util::short(&s.command, 120))
                        .unwrap_or_default();
                    let _ = db::insert_notification(
                        &conn,
                        "warn",
                        project.as_deref(),
                        &format!("делегация дела {item_id}: верификация гейта упала — {first_fail}"),
                    );
                }
            }
            // the reviewer judges with the gate's numbers in hand
            spawn_reviewer(item_id, run_id);
            // both signals may already be in: green → close, red → retry
            try_autopilot(item_id, run_id);
        })
        .map(|_| ())
        .unwrap_or_else(|e| log::warn!("delegate: gate thread spawn failed: {e}"));
}

/// The gate's body: read THIS run's base (guarded by run_id + state), run the
/// checks, return the report. `None` = nothing to gate on (already moved on /
/// no base) — no report is written.
fn run_gate(item_id: i64, run_id: i64) -> Option<GateReport> {
    let row: Option<(Option<String>, Option<String>)> = {
        let conn = db::db_static().lock();
        conn.query_row(
            "SELECT delegation_ref, delegation_final FROM items \
             WHERE id=?1 AND delegation_run_id=?2 AND delegation_state='review'",
            rusqlite::params![item_id, run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
    };
    let Some((Some(dir), final_text)) = row else {
        log::info!("delegate: item {item_id}: гейт пропущен — прогон r{run_id} уже не в review");
        return None;
    };
    let checked = final_text
        .as_deref()
        .and_then(|t| parse_final_block(t))
        .map(|rep| rep.verified)
        .unwrap_or_default();
    let mut commands = gate_commands_from_checked(&checked);
    if commands.is_empty() {
        let files: Vec<String> = {
            let conn = db::db_static().lock();
            conn.prepare("SELECT path FROM item_files WHERE item_id=?1")
                .and_then(|mut st| {
                    st.query_map(rusqlite::params![item_id], |r| r.get::<_, String>(0))
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                })
                .unwrap_or_default()
        };
        commands = gate_fallback_commands(&files);
    }
    if commands.is_empty() {
        log::info!("delegate: item {item_id}: гейт без команд — воркер не перечислил проверок, файловых подсказок нет");
        return Some(GateReport { ran: 0, ok: true, results: Vec::new() });
    }
    let mut results = Vec::new();
    for cmd in &commands {
        let (ok, tail) = gate_run_one(&dir, cmd);
        log::info!("delegate: item {item_id}: гейт {} — {cmd}", if ok { "ok" } else { "FAIL" });
        results.push(GateStep { command: cmd.clone(), ok, tail });
    }
    let ok = results.iter().all(|s| s.ok);
    Some(GateReport { ran: results.len(), ok, results })
}

/// Run one gate command in `dir` through `sh -c` with a per-step budget;
/// returns (ok, last bytes of merged output). Output goes to temp files, not
/// pipes — a chatty `cargo test` cannot deadlock the reader. Spawn failure
/// counts as a fail (an honest «команда не выполнилась», not a silent pass).
fn gate_run_one(dir: &str, cmd: &str) -> (bool, String) {
    let tmp = std::env::temp_dir().join(format!(
        "atlas-gate-{}-{}",
        std::process::id(),
        crate::util::now().to_bits()
    ));
    let out_path = tmp.with_extension("out");
    let err_path = tmp.with_extension("err");
    let mk = |p: &std::path::Path| std::fs::File::create(p);
    let (Ok(out_f), Ok(err_f)) = (mk(&out_path), mk(&err_path)) else {
        return (false, "не удалось создать временные файлы вывода".into());
    };
    let child = std::process::Command::new("sh")
        .args(["-c", cmd])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out_f))
        .stderr(std::process::Stdio::from(err_f))
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            let _ = std::fs::remove_file(&err_path);
            return (false, format!("не запустилась: {e}"));
        }
    };
    let deadline = Instant::now() + GATE_STEP_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&out_path);
                let _ = std::fs::remove_file(&err_path);
                return (false, format!("ошибка ожидания: {e}"));
            }
        }
    };
    let mut out = std::fs::read_to_string(&out_path).unwrap_or_default();
    if let Ok(e) = std::fs::read_to_string(&err_path) {
        out.push_str(&e);
    }
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);
    match status {
        Some(st) => {
            let tail_len = out.len().min(1_500);
            let tail = out[out.len() - tail_len..].to_string();
            (st.success(), tail)
        }
        None => (false, format!("таймаут {} с", GATE_STEP_TIMEOUT.as_secs())),
    }
}

// -------------------------------------------------------- autopilot ----

/// Auto-retries of one delegation cycle: исходный прогон + 2 автоповтора,
/// потом эскалация к владельцу.
pub const DELEGATION_RETRIES: u32 = 2;

/// What the autopilot wants done with a run that reached `review`. Pure data —
/// [`try_autopilot`] turns it into DB writes and notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AutoAction {
    /// gate ok + reviewer accept (or no reviewer) — close the item.
    Accept,
    /// machine report as feedback, back to the worker.
    Retry,
    /// the signals disagree or nothing new — the owner decides.
    Wait,
    /// retries exhausted / no machine verdict — escalate, stop the cycle.
    Escalate,
}

/// The autopilot's decision table. `gate_ok: Option<bool>` — None = the gate
/// has not reported yet; `review` — None = the reviewer has not answered yet
/// (or is off), Some(None) = answered without a parseable verdict.
fn autopilot_decision(
    gate_ok: Option<bool>,
    reviewer_on: bool,
    review: Option<Option<String>>,
    attempts: u32,
) -> AutoAction {
    let Some(gate_ok) = gate_ok else { return AutoAction::Wait };
    if reviewer_on && review.is_none() {
        return AutoAction::Wait;
    }
    let verdict = review.flatten();
    let green = gate_ok && (verdict.as_deref() == Some("accept") || !reviewer_on);
    if green {
        return AutoAction::Accept;
    }
    if attempts >= DELEGATION_RETRIES {
        return AutoAction::Escalate;
    }
    AutoAction::Retry
}

/// The autopilot's single entry point, called from the gate thread and the
/// reviewer thread after each writes its result: whichever lands last has
/// both signals and decides. Guarded by run_id + state everywhere — a newer
/// run or an already-settled item is never touched. Off by default
/// (`[delegation] autopilot`); a manual reject zeroes attempts («рули я»).
fn try_autopilot(item_id: i64, run_id: i64) {
    if !cfg::config().delegation_autopilot {
        return;
    }
    let snap: Option<(String, i64, Option<String>)> = {
        let conn = db::db_static().lock();
        conn.query_row(
            "SELECT delegation_state, delegation_attempts, delegation_review FROM items \
             WHERE id=?1 AND delegation_run_id=?2 AND delegation_state='review'",
            rusqlite::params![item_id, run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()
    };
    let Some((_, attempts, review)) = snap else { return };
    let gate = read_gate_report(item_id, run_id);
    let reviewer_on = reviewer_model().is_some();
    let review_ready = review.as_deref().map(|t| review_verdict(t));
    let attempts = attempts.max(0) as u32;
    match autopilot_decision(gate.as_ref().map(|g| g.ok), reviewer_on, review_ready, attempts) {
        AutoAction::Wait => {}
        AutoAction::Accept => auto_accept(item_id, run_id, gate.as_ref()),
        AutoAction::Retry => auto_reject(item_id, run_id, attempts + 1, gate.as_ref(), review.as_deref()),
        AutoAction::Escalate => auto_escalate(item_id, run_id),
    }
}

/// Авто-акцепт: gate ok + reviewer accept (или ревьюера нет) → дело закрыто
/// без мержа; worktree остаётся владельцу. Guarded by run_id + state.
fn auto_accept(item_id: i64, run_id: i64, gate: Option<&GateReport>) {
    let conn = db::db_static().lock();
    let changed = conn
        .execute(
            "UPDATE items SET delegation_state='done', delegation_ended=?1, updated_at=?1, \
             delegation_attempts=0 WHERE id=?2 AND delegation_run_id=?3 AND delegation_state='review'",
            rusqlite::params![crate::util::now(), item_id, run_id],
        )
        .unwrap_or(0);
    drop(conn);
    if changed == 0 {
        return; // уже закрыт/перезапущен — второй сигнал опоздал
    }
    publish_run(item_id, "done");
    let via = match gate {
        Some(g) if g.ran > 0 => "гейт ✓, ревьюер ✓".to_string(),
        _ => "гейт без команд, ревьюер ✓".to_string(),
    };
    let conn = db::db_static().lock();
    let project: Option<String> = conn
        .query_row("SELECT project FROM items WHERE id=?1", [item_id], |r| r.get(0))
        .unwrap_or(None);
    let _ = db::insert_notification(
        &conn,
        "info",
        project.as_deref(),
        &format!("делегация дела {item_id}: принято автопилотом ({via})"),
    );
    log::info!("delegate: item {item_id}: автопилот закрыл дело ({via})");
}

/// Авто-reject: machine report as the worker's feedback, the counter moves,
/// the run goes back to `queued` — the autopilot tick re-delegates it.
fn auto_reject(item_id: i64, run_id: i64, new_attempts: u32, gate: Option<&GateReport>, review: Option<&str>) {
    let feedback = match (gate, review) {
        (Some(g), _) if g.ran > 0 && !g.ok => {
            let mut f = String::from("гейт приёмки упал:\n");
            for s in &g.results {
                f.push_str(&format!(
                    "{} {}{}\n",
                    if s.ok { "✓" } else { "✗" },
                    s.command,
                    if s.ok { String::new() } else { format!(" — {}", crate::util::short(&s.tail, 200)) }
                ));
            }
            f
        }
        (_, Some(r)) if review_verdict(r).as_deref() == Some("block") => {
            format!("ревьюер отклонил:\n{}", cap(r, 40, 1_200).0)
        }
        _ => "машинная проверка не дала результата — смотри досье прогона".to_string(),
    };
    let conn = db::db_static().lock();
    let changed = conn
        .execute(
            "UPDATE items SET delegation_state='queued', delegation_feedback=?1, \
             delegation_attempts=?2, updated_at=?3 \
             WHERE id=?4 AND delegation_run_id=?5 AND delegation_state='review'",
            rusqlite::params![feedback, new_attempts as i64, crate::util::now(), item_id, run_id],
        )
        .unwrap_or(0);
    let project: Option<String> = conn
        .query_row("SELECT project FROM items WHERE id=?1", [item_id], |r| r.get(0))
        .unwrap_or(None);
    drop(conn);
    if changed == 0 {
        return;
    }
    publish_run(item_id, "queued");
    let conn = db::db_static().lock();
    let _ = db::insert_notification(
        &conn,
        "warn",
        project.as_deref(),
        &format!(
            "делегация дела {item_id}: автопилот вернул воркеру (попытка {new_attempts}/{DELEGATION_RETRIES})"
        ),
    );
    log::info!("delegate: item {item_id}: автопилот вернул дело воркеру (попытка {new_attempts}/{DELEGATION_RETRIES})");
}

/// Эскалация: попытки исчерпаны или машина без вердикта — дело остаётся в
/// review, владелец решает. One-shot: the marker (`attempts = limit + 1`)
/// silences the second signal's call.
fn auto_escalate(item_id: i64, run_id: i64) {
    let conn = db::db_static().lock();
    let changed = conn
        .execute(
            "UPDATE items SET delegation_attempts=?1, updated_at=?2 \
             WHERE id=?3 AND delegation_run_id=?4 AND delegation_state='review'",
            rusqlite::params![(DELEGATION_RETRIES + 1) as i64, crate::util::now(), item_id, run_id],
        )
        .unwrap_or(0);
    let project: Option<String> = conn
        .query_row("SELECT project FROM items WHERE id=?1", [item_id], |r| r.get(0))
        .unwrap_or(None);
    drop(conn);
    if changed == 0 {
        return;
    }
    let conn = db::db_static().lock();
    let _ = db::insert_notification(
        &conn,
        "warn",
        project.as_deref(),
        &format!(
            "делегация дела {item_id}: автопилот исчерпал {DELEGATION_RETRIES} попыток — решение за тобой (карточка ревью)"
        ),
    );
}

/// The autopilot tick: every 20 s re-delegate queued items that the autopilot
/// itself queued (attempts > 0, target recorded, at least 5 s since the
/// reject). Manual rejects zero the attempts and never auto-start. `start`
/// re-checks every guard (running/needs-feedback → 409), so a stale tick is
/// harmless.
pub fn spawn_autopilot() {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(20));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if !cfg::config().delegation_autopilot {
                continue;
            }
            let queue: Vec<(i64, Option<String>)> = {
                let conn = db::db_static().lock();
                conn.prepare(
                    "SELECT id, delegation_target FROM items \
                     WHERE delegation_state='queued' AND delegation_attempts>0 \
                     AND delegation_attempts<=?1 AND delegation_target IS NOT NULL \
                     AND delegation_ended IS NOT NULL AND ?2 - delegation_ended >= 5",
                )
                .and_then(|mut st| {
                    st.query_map(
                        rusqlite::params![DELEGATION_RETRIES as i64, crate::util::now()],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
                    )
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                })
                .unwrap_or_default()
            };
            for (id, target) in queue {
                let Some(t) = target.as_deref().and_then(Target::parse) else { continue };
                match start(id, t).await {
                    Ok(_) => log::info!("delegate: автопилот перезапустил дело {id} ({})", t.as_str()),
                    Err(e) => {
                        let msg = match e {
                            Error::NotFound(m) | Error::Busy(m) | Error::NoPath(m) | Error::Bad(m) | Error::Io(m) => m,
                        };
                        log::warn!("delegate: автопилот не смог перезапустить дело {id}: {msg}");
                    }
                }
            }
        }
    });
}

// ------------------------------------------------------- run observer ----

/// The poll tick of the run observer — also the minimum silence a tick can
/// honestly report (a shorter gap is normal model pacing, not an event).
const SILENCE_TICK: Duration = Duration::from_secs(30);

/// What one observer tick wants said. Pure data — the thread turns it into
/// ring lines / a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObsEvent {
    /// alive→silent: the child is alive but produced no frames for at least
    /// one tick — one ring line goes out. `warn: true` marks the same tick
    /// ALSO firing the one-shot notification (silence crossed the threshold).
    WentSilent { secs: u64, warn: bool },
    /// silence crossed the warn threshold while already silent — the one-shot
    /// notification only, no new ring line (no spam).
    Warn { secs: u64 },
    /// silent→alive: frames returned after a reported silence.
    Resumed { secs: u64 },
}

/// Observation state of the delegation run poller — pure, so tests drive it
/// with injected instants and an injectable alive flag instead of real
/// sleeps. NO kill decision exists here: the observer never stops a run, it
/// only tells the owner what the child is doing. The frame reader calls
/// [`RunObserver::frame`] for EVERY mirrored frame (text deltas, tool
/// starts/results, `agent_end`).
struct RunObserver {
    last_frame: Instant,
    /// The silence reported at the previous tick — «кадры пошли» names it.
    prev_silence: Duration,
    /// Currently in a reported-silence stretch.
    silent: bool,
    /// The per-run warn notification already went out (one per run, ever).
    warned: bool,
    /// `0` disables the warn notification entirely.
    warn_after: Duration,
}

impl RunObserver {
    fn new(warn_after: Duration, now: Instant) -> RunObserver {
        RunObserver {
            last_frame: now,
            prev_silence: Duration::ZERO,
            silent: false,
            warned: false,
            warn_after,
        }
    }

    /// A frame arrived — the child demonstrably does something.
    fn frame(&mut self, now: Instant) {
        self.last_frame = now;
    }

    /// The current silence age — failure messages append it as context
    /// («на момент провала: тишина N с»).
    fn silence_now(&self, now: Instant) -> Duration {
        now.duration_since(self.last_frame)
    }

    /// One poll tick. `alive` = the child process still exists (injected so
    /// tests need no real process); a dead child is the exit watcher's
    /// business — the observer stays quiet about it.
    fn tick(&mut self, now: Instant, alive: bool) -> Option<ObsEvent> {
        if !alive {
            return None;
        }
        let silence = self.silence_now(now);
        if silence >= SILENCE_TICK {
            self.prev_silence = silence;
            if !self.silent {
                self.silent = true;
                let warn =
                    !self.warned && self.warn_after > Duration::ZERO && silence >= self.warn_after;
                if warn {
                    self.warned = true;
                }
                return Some(ObsEvent::WentSilent { secs: silence.as_secs(), warn });
            }
            if !self.warned && self.warn_after > Duration::ZERO && silence >= self.warn_after {
                self.warned = true;
                return Some(ObsEvent::Warn { secs: silence.as_secs() });
            }
            None
        } else if self.silent {
            self.silent = false;
            Some(ObsEvent::Resumed { secs: self.prev_silence.as_secs() })
        } else {
            None
        }
    }
}

// ------------------------------------------------------------------- db -----

/// The last non-empty ring line, trimmed and capped — the one-line «что воркер
/// делал» evidence a failure message appends, so «упало» is never the whole
/// story.
fn last_action(tail: &[String]) -> Option<String> {
    let mut l = tail.iter().rev().find(|l| !l.trim().is_empty())?.trim().to_string();
    if l.chars().count() > 120 {
        l = l.chars().take(120).collect::<String>() + "…";
    }
    Some(l)
}

/// The observer's context line for a failure message: how silent the child
/// was when it died («на момент провала: тишина N с») — a silence below one
/// tick is ordinary pacing and says nothing.
fn silence_note(silence: Duration) -> String {
    if silence >= SILENCE_TICK {
        format!("; на момент провала: тишина {} с", silence.as_secs())
    } else {
        String::new()
    }
}

/// A delegation start row: state running + target/ref/run id, start HEAD, the
/// session file used, timestamps fresh; run-scoped evidence reset (a previous
/// run's tail/final must not leak into this one's `/run`).
fn db_start(
    conn: &rusqlite::Connection,
    item_id: i64,
    target: Target,
    reference: &str,
    run_id: i64,
    meta: &StartMeta,
) -> rusqlite::Result<usize> {
    let now = crate::util::now();
    conn.execute(
        "UPDATE items SET delegation_state='running', delegation_target=?1, delegation_ref=?2, \
         delegation_run_id=?3, delegation_started=?4, delegation_ended=NULL, delegation_exit=NULL, \
         delegation_head=?5, delegation_session=?6, delegation_final=NULL, delegation_tail=NULL, \
         delegation_cmds=NULL, delegation_review=NULL, updated_at=?4 WHERE id=?7",
        rusqlite::params![
            target.as_str(),
            reference,
            run_id,
            now,
            meta.head.as_deref(),
            meta.session.as_deref(),
            item_id
        ],
    )
}

/// What a delegation start records about its base: the review gate's HEAD and
/// the session file this run uses.
#[derive(Debug, Clone, Default)]
struct StartMeta {
    head: Option<String>,
    session: Option<String>,
}

/// A terminal-ish state write: `state` + ended timestamp + exit code, plus the
/// run's evidence (ring tail, final message, command ledger) when the caller
/// has it — `None` keeps what is stored (reap/cancel must not erase the
/// previous evidence).
fn db_finish(
    conn: &rusqlite::Connection,
    item_id: i64,
    state: &str,
    exit: Option<i64>,
    tail: Option<&str>,
    final_text: Option<&str>,
    cmds: Option<&str>,
) -> rusqlite::Result<usize> {
    let now = crate::util::now();
    conn.execute(
        "UPDATE items SET delegation_state=?1, delegation_ended=?2, delegation_exit=?3, \
         delegation_tail=COALESCE(?5, delegation_tail), \
         delegation_final=COALESCE(?6, delegation_final), \
         delegation_cmds=COALESCE(?7, delegation_cmds), updated_at=?2 WHERE id=?4",
        rusqlite::params![state, now, exit, item_id, tail, final_text, cmds],
    )
}

fn db_set_state(conn: &rusqlite::Connection, item_id: i64, state: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE items SET delegation_state=?1, updated_at=?2 WHERE id=?3",
        rusqlite::params![state, crate::util::now(), item_id],
    )
}

fn db_set_feedback(
    conn: &rusqlite::Connection,
    item_id: i64,
    feedback: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE items SET delegation_feedback=?1, updated_at=?2 WHERE id=?3",
        rusqlite::params![feedback, crate::util::now(), item_id],
    )
}

/// Run one DB mutation on the blocking pool (the connection is a shared mutex
/// and handlers are async).
async fn db_ex<F>(f: F) -> Result<(), Error>
where
    F: FnOnce(&rusqlite::Connection) -> rusqlite::Result<()> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let conn = db::db_static().lock();
        f(&conn)
    })
    .await
    .map_err(|e| Error::Io(format!("blocking task: {e}")))?
    .map_err(|e| Error::Io(format!("{e}")))
}

/// git through `run_cmd` (argv array, no shell); (ok, merged output).
async fn git(args: &[&str], timeout_secs: u64) -> (bool, String) {
    let argv: Vec<String> = std::iter::once("git".to_string())
        .chain(args.iter().map(|a| a.to_string()))
        .collect();
    crate::util::run_cmd(&argv, Duration::from_secs(timeout_secs)).await
}

/// HEAD sha of the repo at `dir`; `None` outside a repo or with no commits —
/// the start reference simply stays unknown and the review gate falls back.
pub(crate) async fn git_head(dir: &Path) -> Option<String> {
    let (ok, out) = git(&["-C", &dir.to_string_lossy(), "rev-parse", "HEAD"], 15).await;
    let sha = out.trim();
    if ok && !sha.is_empty() {
        Some(sha.to_string())
    } else {
        None
    }
}

/// git STDOUT whatever the exit code: `git diff --no-index` exits 1 when there
/// IS a difference — the diff text is exactly what is wanted there.
async fn git_output(args: &[&str], timeout_secs: u64) -> String {
    let argv: Vec<String> = std::iter::once("git".to_string())
        .chain(args.iter().map(|a| a.to_string()))
        .collect();
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output()).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------- preview ----

/// `POST …/delegate/preview` — compile the brief, start nothing. `resume`
/// shows the session a true follow-up would continue (feedback + recorded
/// session file ≤ the cap); `null` = the run starts fresh.
pub async fn preview(item_id: i64, target: Target) -> Result<Value, Error> {
    let loaded = load_bg(item_id).await?;
    let plan = plan(target, &loaded)?;
    let item = &loaded.item;
    let session = session_decision(
        item.feedback.as_deref(),
        item.session.as_deref(),
        &run_session_path(item.id, &item.title, 0),
    );
    let resume = match &session {
        omp_rpc::Session::Resume(p) => Some(p.to_string_lossy().into_owned()),
        omp_rpc::Session::Fresh(_) => None,
    };
    Ok(json!({
        "ok": true,
        "brief": compile_brief(item, &plan.cwd),
        "cwd": plan.cwd.to_string_lossy(),
        // сессия, которую продолжит воркер (null — свежую)
        "resume": resume,
    }))
}

// ------------------------------------------------------------------ start ----

/// `POST …/delegate` — start one delegation of `item_id` to `target`.
pub async fn start(item_id: i64, target: Target) -> Result<Value, Error> {
    let loaded = load_bg(item_id).await?;
    // `queued` (a rejected item waiting for its follow-up) delegates fine —
    // that IS the feedback loop: reject → фидбек → следующий бриф. Only a
    // live run and an unanswered question block a start.
    if loaded.item.state == "running" {
        return Err(Error::Busy(format!(
            "дело {item_id} уже делегировано (состояние 'running')"
        )));
    }
    // questions still wait for an answer: re-delegation goes through
    // reject (the feedback lands in the next brief's «Фидбек» section)
    if loaded.item.state == "needs-feedback" {
        return Err(Error::Busy(format!(
            "дело {item_id}: воркер ждёт ответа на вопросы — верни с фидбеком (reject)"
        )));
    }
    let plan = plan(target, &loaded)?;
    match target {
        Target::Orchestrator => start_orchestrator(loaded.item, plan).await,
        Target::Worktree => start_process(loaded.item, target, plan, true).await,
        Target::Dir => start_process(loaded.item, target, plan, false).await,
    }
}

/// The shared headless-worker start (worktree and dir targets).
async fn start_process(
    item: Item,
    target: Target,
    plan: Plan,
    worktree: bool,
) -> Result<Value, Error> {
    let mut project_checkout = None;
    if worktree {
        if plan.cwd.exists() {
            return Err(Error::Busy(format!(
                "ворктри уже существует: {}",
                plan.cwd.display()
            )));
        }
        let project = plan
            .project_path
            .clone()
            .ok_or_else(|| no_path(&item, target))?;
        if let Some(parent) = plan.cwd.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Io(format!("{}: {e}", parent.display())))?;
        }
        let branch = plan.branch.clone().expect("worktree plan carries a branch");
        let project_s = project.to_string_lossy().into_owned();
        let dir_s = plan.cwd.to_string_lossy().into_owned();
        let (ok, out) = git(&["-C", &project_s, "worktree", "add", &dir_s, "-b", &branch], 60).await;
        if !ok {
            return Err(Error::Bad(format!("git worktree add: {out}")));
        }
        write_deleg_marker(
            &plan.cwd,
            item.id,
            item.project.as_deref().unwrap_or(""),
            plan.branch.as_deref(),
        );
        project_checkout = Some(project_s);
    }

    let brief = compile_brief(&item, &plan.cwd);
    // Fresh per-run session dir (the run's dossier folder): without a pin omp
    // continues the project's LATEST session (item 24 paid 223K tokens of
    // alien context for one flag). omp 18.9 mints fresh sessions inside a
    // --session-dir under its own name; the resolver pins the real file into
    // delegation_session right after the spawn so a follow-up can resume it.
    let run_id = runner::next_id();
    let run_session = run_session_path(item.id, &item.title, run_id);
    let session_dir = run_session
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg::data_dir());
    let _ = std::fs::create_dir_all(&session_dir);
    let spawned_at = std::time::SystemTime::now();
    let session = session_decision(item.feedback.as_deref(), item.session.as_deref(), &run_session);
    // FRESH + the project's own omp session on disk → a seeded fork of it
    // (copy under a new id, resume by path): the worker starts from the
    // project's context, the original stays untouched. No project session —
    // today's fresh `--session-dir` spawn, resolver pins omp's file as before.
    let session = if matches!(session, omp_rpc::Session::Fresh(_)) {
        match fork_project_session(&plan.cwd, &session_dir) {
            Some(p) => omp_rpc::Session::Resume(p),
            None => session,
        }
    } else {
        session
    };
    let session_path = match &session {
        omp_rpc::Session::Resume(p) => p.to_string_lossy().into_owned(),
        omp_rpc::Session::Fresh(_) => String::new(),
    };
    let mut omp_argv = vec![cfg::OMP_BIN.clone()];
    omp_argv.extend(model_args());
    omp_argv.extend([
        "-p".to_string(),
        brief,
        match &session {
            omp_rpc::Session::Resume(_) => "--resume".to_string(),
            omp_rpc::Session::Fresh(_) => "--session-dir".to_string(),
        },
        match &session {
            omp_rpc::Session::Resume(p) => p.to_string_lossy().into_owned(),
            omp_rpc::Session::Fresh(_) => session_dir.to_string_lossy().into_owned(),
        },
        "--cwd".to_string(),
        plan.cwd.to_string_lossy().into_owned(),
    ]);
    let unit = new_unit(item.id, &item.title);
    let argv = match &unit {
        Some(u) => omp_rpc::systemd_run_argv(u, &omp_argv),
        None => omp_argv,
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    let (id, project) = (item.id, item.project.clone());
    let project_for_exit = project.clone();
    // The run row exists BEFORE the child: while the worker runs, the board
    // and `/run` see it, and a spawn failure settles an honest `failed` row
    // instead of nothing at all. The review gate's base — the repo HEAD the
    // worker starts from (a fresh worktree sits exactly on the project's HEAD;
    // a non-repo simply records nothing).
    let head = git_head(&plan.cwd).await;
    write_start(
        id,
        target,
        &plan.reference,
        run_id,
        StartMeta {
            head,
            // fresh: omp mints its own file — the resolver pins it right after
            session: match &session {
                omp_rpc::Session::Resume(_) => Some(session_path),
                omp_rpc::Session::Fresh(_) => None,
            },
        },
        project.clone(),
        item.title.clone(),
    )
    .await?;
    let spawn_err = |e: String| -> Error {
        settle_spawn_failed(id, project.as_deref(), &e);
        Error::Io(e)
    };
    let run = match &unit {
        Some(u) => spawn_unit_worker(u, run_id, &argv, &plan.cwd, move |code| {
            on_process_exit(id, project_for_exit.as_deref(), code, &flag);
        })
        .map_err(|e| spawn_err(format!("не удалось запустить omp (юнит {u}): {e}")))?,
        None => runner::spawn_with_id(run_id, &argv, &plan.cwd, move |code| {
            on_process_exit(id, project_for_exit.as_deref(), code, &flag);
        })
        .map_err(|e| spawn_err(format!("не удалось запустить omp: {e}")))?,
    };
    debug_assert_eq!(run.id, run_id, "spawn must adopt the pre-reserved run id");
    if matches!(session, omp_rpc::Session::Fresh(_)) {
        spawn_session_resolver(id, run_id, session_dir, spawned_at);
    }
    let unit_mark = match &unit {
        Some(u) => format!(" [unit {u}]"),
        None => String::new(),
    };
    put_job(
        item.id,
        Arc::new(Job {
            run: Arc::clone(&run),
            cancelled,
            unit,
        }),
    );
    log::info!(
        "delegate: item {} → {} (run {run_id}){}{}",
        item.id,
        plan.cwd.display(),
        match &project_checkout {
            Some(p) => format!(" worktree of {p}"),
            None => String::new(),
        },
        unit_mark
    );
    Ok(json!({"ok": true, "run_id": run_id, "state": "running", "ref": plan.reference}))
}

/// The orchestrator target: a DEDICATED rpc child per delegation, working in
/// the item's project directory (the plan resolved it). It shares nothing with
/// the panel's interactive orchestrator — not the process, not the session, not
/// the turn gate — so delegations run in parallel and the chat stays free.
///
/// The child carries the same approval overlay as the panel child: benign
/// commands run free, a `prompt` rule has nobody to show a card to and fails
/// the call (fail-closed), the guard's denies block outright. The turn is
/// watched in-process: frames mirror into the run ring, which the journal
/// modal and the review dialog render — an autonomous worker's questions are
/// text there, and «вернуть с фидбеком» is the answer channel. On `agent_end`
/// the item goes to `review` and the idle child is torn down (SIGTERM), so rpc
/// processes do not pile up.
async fn start_orchestrator(item: Item, plan: Plan) -> Result<Value, Error> {
    let brief = compile_brief(&item, &plan.cwd);
    // Fresh per-run session by default (its own session dir — the run's
    // dossier folder, never the project's latest session). A true follow-up —
    // feedback exists AND the previous run's session file is recorded —
    // resumes THAT file. omp 18.9 mints fresh sessions inside a
    // --session-dir; the resolver pins the real file after the spawn.
    let run_id = runner::next_id();
    let run_session = run_session_path(item.id, &item.title, run_id);
    let session_dir = run_session
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg::data_dir());
    let _ = std::fs::create_dir_all(&session_dir);
    let spawned_at = std::time::SystemTime::now();
    let session = session_decision(item.feedback.as_deref(), item.session.as_deref(), &run_session);
    // FRESH + the project's own omp session on disk → a seeded fork of it
    // (copy under a new id, resume by path): the worker starts from the
    // project's context, the original stays untouched. No project session —
    // today's fresh `--session-dir` spawn, resolver pins omp's file as before.
    let session = if matches!(session, omp_rpc::Session::Fresh(_)) {
        match fork_project_session(&plan.cwd, &session_dir) {
            Some(p) => omp_rpc::Session::Resume(p),
            None => session,
        }
    } else {
        session
    };
    let session_path = match &session {
        omp_rpc::Session::Resume(p) => p.to_string_lossy().into_owned(),
        omp_rpc::Session::Fresh(_) => String::new(),
    };
    let resume = match &session {
        omp_rpc::Session::Resume(p) => Some(p.to_string_lossy().into_owned()),
        omp_rpc::Session::Fresh(_) => None,
    };
    let mut argv = omp_rpc::build_argv(
        &[
            "--config".to_string(),
            cfg::orch_overlay_file().to_string_lossy().into_owned(),
        ],
        &session,
        &plan.cwd,
    );
    argv.extend(model_args());
    let unit = new_unit(item.id, &item.title);
    let argv = match &unit {
        Some(u) => omp_rpc::systemd_run_argv(u, &argv),
        None => argv,
    };
    // The run row exists BEFORE the child spawns: while the turn runs, the
    // board and `/run` see it — the silent «бриф → тишина» runs of 25/26 were
    // invisible precisely because the row came last. A write_start failure
    // now has nothing to kill; a spawn failure settles the honest row instead.
    // The review gate's base — the project repo's HEAD right now.
    let head = git_head(&plan.cwd).await;
    let watch_head = head.clone(); // this run's start base, for the turn watcher
    write_start(
        item.id,
        Target::Orchestrator,
        &plan.reference,
        run_id,
        StartMeta {
            head,
            // fresh: omp mints its own file — the resolver pins it right after
            session: match &session {
                omp_rpc::Session::Resume(_) => Some(session_path),
                omp_rpc::Session::Fresh(_) => None,
            },
        },
        item.project.clone(),
        item.title.clone(),
    )
    .await?;
    let spawn_failed = |detail: String| -> Error {
        settle_spawn_failed(item.id, item.project.as_deref(), &detail);
        Error::Io(detail)
    };
    let mut child = match omp_rpc::spawn_sync(&argv, &plan.cwd) {
        Ok(c) => c,
        Err(e) => {
            return Err(spawn_failed(format!("не удалось запустить omp rpc: {e}")));
        }
    };
    if matches!(session, omp_rpc::Session::Fresh(_)) {
        spawn_session_resolver(item.id, run_id, session_dir, spawned_at);
    }
    // unit mode: systemd-run is the direct child (the `--pipe` fd holder);
    // the pid that cancel operates on is the unit's real MainPID.
    let pid = match &unit {
        Some(u) => match omp_rpc::main_pid(u) {
            Some(p) => p,
            None => {
                let _ = child.kill();
                return Err(spawn_failed(format!(
                    "systemd-юнит {u} не получил процесс (MainPID=0)"
                )));
            }
        },
        None => child.id(),
    };
    let unit_mark = match &unit {
        Some(u) => format!(" [unit {u}]"),
        None => String::new(),
    };
    let run = runner::Run::with_id(run_id, pid);
    let cancelled = Arc::new(AtomicBool::new(false));
    let end_unit = unit.clone();
    put_job(
        item.id,
        Arc::new(Job {
            run: Arc::clone(&run),
            cancelled: Arc::clone(&cancelled),
            unit,
        }),
    );
    log::info!(
        "delegate: item {} → rpc child {pid} in {}{unit_mark}",
        item.id,
        plan.cwd.display()
    );

    // Наблюдение вместо казни: no timer kills a working child. A ~30 s
    // poller watches {child alive, frame age} and speaks only on
    // transitions — a ring line when a live child goes quiet and when the
    // frames return, and ONE warn notification per run when silence crosses
    // `[delegation] silence_warn_secs` («возможно, долгий thinking»). The
    // frame reader calls `frame()` for EVERY mirrored frame (text deltas,
    // tool starts/results, `agent_end`) before dispatching it. A run ends
    // only honestly: process exit, turn error, owner cancel, empty work.
    let warn_secs = cfg::config().delegation_silence_warn_secs;
    let obs = Arc::new(Mutex::new(RunObserver::new(
        Duration::from_secs(warn_secs),
        Instant::now(),
    )));
    {
        let obs_w = Arc::clone(&obs);
        let run_w = Arc::clone(&run);
        let cancelled_w = Arc::clone(&cancelled);
        let item_w = item.id;
        let project_w = item.project.clone();
        let pid_w = pid;
        std::thread::spawn(move || {
            // the child-existence probe: /proc, no signal, no new dependency;
            // a false «alive» on a just-reaped pid is harmless — the exit
            // watcher owns the settle, the observer only narrates
            let alive = move || std::path::Path::new(&format!("/proc/{pid_w}")).exists();
            loop {
                std::thread::sleep(SILENCE_TICK);
                if !run_w.active() || cancelled_w.load(Ordering::SeqCst) {
                    return;
                }
                let ev = runner::lock(&obs_w).tick(Instant::now(), alive());
                let (line, note) = match ev {
                    None => continue,
                    Some(ObsEvent::WentSilent { secs, warn }) => (
                        Some(format!("наблюдатель: тишина {secs} с, процесс жив")),
                        warn.then(|| {
                            format!(
                                "делегация дела {item_w}: долго без кадров ({secs} с) — возможно, долгий thinking; живой журнал — на карточке дела, отмена — вручную"
                            )
                        }),
                    ),
                    Some(ObsEvent::Warn { secs }) => (
                        None,
                        Some(format!(
                            "делегация дела {item_w}: долго без кадров ({secs} с) — возможно, долгий thinking; живой журнал — на карточке дела, отмена — вручную"
                        )),
                    ),
                    Some(ObsEvent::Resumed { secs }) => {
                        (Some(format!("наблюдатель: кадры пошли (тишина была {secs} с)")), None)
                    }
                };
                if let Some(l) = line {
                    run_w.push_line(l);
                }
                if let Some(text) = note {
                    let conn = db::db_static().lock();
                    if let Err(e) = db::insert_notification(&conn, "warn", project_w.as_deref(), &text) {
                        log::warn!("delegate: silence-warn notification failed: {e}");
                    }
                }
            }
        });
    }

    let stdout = child.stdout.take().expect("piped stdout");
    let mut stdin = child.stdin.take().expect("piped stdin");
    drop(child.stderr.take()); // model-retry noise; the ring carries the turn
    let frames = omp_rpc::Frames::spawn(stdout);

    let watch_item = item.id;
    let watch_project = item.project.clone();
    let watch_resume = resume.clone();
    let watch_ref = plan.reference.clone();
    let obs_r = Arc::clone(&obs);
    std::thread::spawn(move || {
        let obs = obs_r;
        // stdin stays open for the whole turn — the rpc host treats EOF as a
        // hang-up; it is dropped after `agent_end` so the idle child exits.
        // Handshake: wait for the host's `{"type":"ready",…}` (up to
        // [`omp_rpc::READY_WAIT`]), mirroring any earlier frame through the
        // same dispatch; a silent host cannot wedge the start — the prompt
        // goes out anyway.
        let mut mirror = Mirror::default();
        let deadline = Instant::now() + omp_rpc::READY_WAIT;
        loop {
            match frames.next_within(deadline) {
                omp_rpc::Next::Frame(f) => {
                    runner::lock(&obs).frame(Instant::now());
                    if f.get("type").and_then(|t| t.as_str()) == Some("ready") {
                        break;
                    }
                    mirror_frame(&mut mirror, &run, &f);
                }
                omp_rpc::Next::Timeout | omp_rpc::Next::Eof => break,
            }
        }
        let frame = json!({"type": "prompt", "message": brief});
        let _ = omp_rpc::write_frame(&mut stdin, &frame);
        run.push_line(match &watch_resume {
            Some(f) => format!("— продолжаю сессию проекта: {f} —"),
            None => "— ход запущен (свежая сессия) —".to_string(),
        });

        while let Some(f) = frames.next() {
            if cancelled.load(Ordering::SeqCst) {
                break;
            }
            runner::lock(&obs).frame(Instant::now());
            mirror_frame(&mut mirror, &run, &f);
            if mirror.ended {
                break;
            }
        }
        drop(stdin); // EOF → the idle rpc host exits
        if !mirror.partial.is_empty() {
            run.push_line(std::mem::take(&mut mirror.partial));
        }
        if cancelled.load(Ordering::SeqCst) {
            return; // cancel() already settled the item and the run
        }
        // finish() picks the settle owner: the reader thread and the
        // exit watcher race honestly, first finish wins.
        if mirror.ended {
            // completion contract: the final block decides the state. Questions
            // → needs-feedback (terminal-until-answered); no questions and no
            // visible change → failed; otherwise → review.
            let report = parse_final_block(&mirror.text);
            let has_questions = report
                .as_ref()
                .map(|r| !r.questions_empty())
                .unwrap_or(false);
            let (state, level, text) = if has_questions {
                (
                    "needs-feedback",
                    "warn",
                    format!(
                        "делегация дела {watch_item}: воркер задал вопросы — ответь через «вернуть с фидбеком»"
                    ),
                )
            } else if !worker_made_changes(Path::new(&watch_ref), watch_head.as_deref()) {
                (
                    "failed",
                    "err",
                    format!("делегация дела {watch_item}: воркер ничего не сделал"),
                )
            } else {
                (
                    "review",
                    "info",
                    format!("делегация дела {watch_item}: готово к ревью"),
                )
            };
            if run.finish(0) {
                settle_terminal(
                    watch_item,
                    watch_project.as_deref(),
                    state,
                    level,
                    text,
                    Some(0),
                    &cancelled,
                    run.tail_lines(TAIL_LINES),
                    Some(mirror.text),
                    Some(cmds_json(&mirror.cmds)),
                );
                if state == "review" {
                    spawn_gate(watch_item, run.id);
                }
            }
        } else if run.finish(-1) {
            // the child died without `agent_end` (crash, EOF) — the plain
            // old failed path; the last ring line and the observer's silence
            // age are the «что происходило» context the owner gets
            let tail = run.tail_lines(TAIL_LINES);
            let action = last_action(&tail)
                .map(|l| format!("; последнее действие: {l}"))
                .unwrap_or_default();
            let silence = runner::lock(&obs).silence_now(Instant::now());
            settle_terminal(
                watch_item,
                watch_project.as_deref(),
                "failed",
                "err",
                format!(
                    "делегация дела {watch_item}: ход оркестратора упал{action}{}",
                    silence_note(silence)
                ),
                None,
                &cancelled,
                tail,
                None,
                // what the worker managed to run before dying is evidence too
                Some(cmds_json(&mirror.cmds)),
            );
        }
        // the idle (or crashed) child must not pile up
        std::thread::sleep(Duration::from_millis(300));
        stop_child(end_unit.as_deref(), pid);
    });
    Ok(json!({"ok": true, "run_id": run_id, "state": "running", "ref": plan.reference}))
}

/// One ledgered tool call of a run: the command text (for `bash` —
/// `args.command`; other tools — name + compact args, capped) and whether the
/// call ended with an error. The persisted proof-of-work: «ПРОВЕРЕНО» stops
/// being a claim and becomes a checkable fact (start/end rpc frames are
/// paired by `toolCallId` — see the golden-test wire contract).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CmdRec {
    pub command: String,
    pub error: bool,
}

/// The ledgered command text of a `tool_execution_start`: bash carries the
/// real command line in `args.command`; every other tool is recorded as
/// `name {compact args}` so the owner still sees what was attempted.
fn cmd_text(name: &str, args: Option<&Value>) -> String {
    if name == "bash" {
        if let Some(c) = args.and_then(|a| a.get("command")).and_then(|c| c.as_str()) {
            return crate::util::short(c, 200);
        }
    }
    match args {
        Some(a) => crate::util::short(&format!("{name} {a}"), 200),
        None => name.to_string(),
    }
}

/// The ring-mirror state of one watched turn.
#[derive(Default)]
struct Mirror {
    /// Assistant text streamed since the last flush into the ring.
    partial: String,
    /// The assistant text of the whole turn, tail-capped — the final-message
    /// contract (`parse_final_block`) is parsed from it on `agent_end`.
    text: String,
    /// The one-time «nobody can answer an approval» warning is out.
    warned: bool,
    /// `agent_end` seen — the turn is over.
    ended: bool,
    /// The tool-call ledger (proof-of-work): one `CmdRec` per
    /// `tool_execution_start`, error flag set by the paired end frame.
    cmds: Vec<CmdRec>,
    /// Open calls: `toolCallId` → index into `cmds`; the end frame consumes
    /// its entry (an end without a start is ignored — nothing to pair).
    open_calls: std::collections::HashMap<String, usize>,
}

/// How much of the turn's assistant text is kept for the final-block parse —
/// the contract block is at the END, so only the tail matters.
const TURN_TEXT_CAP: usize = 32_000;

/// Append one streamed delta to the turn text, keeping the tail within
/// [`TURN_TEXT_CAP`] (dropped from the front, on a char boundary).
fn push_turn_text(m: &mut Mirror, delta: &str) {
    m.text.push_str(delta);
    if m.text.len() > TURN_TEXT_CAP {
        let mut drop = m.text.len() - TURN_TEXT_CAP;
        while !m.text.is_char_boundary(drop) {
            drop += 1;
        }
        m.text.drain(..drop);
    }
}

/// Mirror one rpc frame into the run ring — the delegation's policy on top of
/// the shared transport. Streamed text flushes on line boundaries, a tool
/// call lands as `· name args`, an errored call as `! result`, the approval
/// card (which nobody can answer here) warns once, `agent_end` ends the turn.
fn mirror_frame(m: &mut Mirror, run: &runner::Run, f: &Value) {
    match f.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "message_update" => {
            // wire shape (omp v18.x rpc): the delta lives INSIDE
            // `assistantMessageEvent`, typed (`text_delta` / `thinking_delta`);
            // only text goes into the ring and the turn text — thinking would
            // flood both (measured on the dev instance, 10.10.2026)
            let ev = f.get("assistantMessageEvent").unwrap_or(&Value::Null);
            let text = ev.get("type").and_then(|t| t.as_str()) == Some("text_delta");
            if text {
                if let Some(d) = ev.get("delta").and_then(|d| d.as_str()) {
                    push_turn_text(m, d);
                    m.partial.push_str(d);
                    while let Some(i) = m.partial.find('\n') {
                        let l: String = m.partial.drain(..=i).collect();
                        run.push_line(l.trim_end().to_string());
                    }
                }
            }
        }
        "tool_execution_start" => {
            if !m.partial.is_empty() {
                run.push_line(std::mem::take(&mut m.partial));
            }
            let name = f.get("toolName").and_then(|t| t.as_str()).unwrap_or("tool");
            let mut detail = f.get("args").map(|a| a.to_string()).unwrap_or_default();
            if detail.chars().count() > 220 {
                detail = detail.chars().take(220).collect::<String>() + "…";
            }
            run.push_line(format!("· {name} {detail}"));
            // ledger: record the call, remember its id for the paired end
            let idx = m.cmds.len();
            m.cmds.push(CmdRec { command: cmd_text(name, f.get("args")), error: false });
            if let Some(id) = f.get("toolCallId").and_then(|t| t.as_str()) {
                if !id.is_empty() {
                    m.open_calls.insert(id.to_string(), idx);
                }
            }
        }
        "tool_execution_end" => {
            let is_err = f.get("isError").and_then(|e| e.as_bool()).unwrap_or(false);
            // ledger: settle the paired call (unknown/duplicate id — ignore)
            if let Some(id) = f.get("toolCallId").and_then(|t| t.as_str()) {
                if let Some(idx) = m.open_calls.remove(id) {
                    if let Some(rec) = m.cmds.get_mut(idx) {
                        rec.error = is_err;
                    }
                }
            }
            if is_err {
                if !m.partial.is_empty() {
                    run.push_line(std::mem::take(&mut m.partial));
                }
                let text = f
                    .pointer("/result/content/0/text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("вызов завершился ошибкой");
                let mut msg = text.to_string();
                if msg.chars().count() > 300 {
                    msg = msg.chars().take(300).collect::<String>() + "…";
                }
                run.push_line(format!("! {msg}"));
            }
        }
        "extension_ui_request"
            if f.get("method").and_then(|x| x.as_str()) == Some("select") =>
        {
            if !m.warned {
                m.warned = true;
                run.push_line(
                    "! автономный ход: подтверждение показать некому — такой вызов будет отклонён"
                        .to_string(),
                );
            }
        }
        "agent_end" => {
            // non-streaming hosts may never send text_delta — the turn's
            // assistant text then arrives right here, in the frame itself;
            // the final-message contract is parsed from `m.text` either way
            if m.text.trim().is_empty() {
                if let Some(text) = last_assistant_text(f) {
                    push_turn_text(m, &text);
                }
            }
            m.ended = true;
        }
        _ => {}
    }
}

/// The last non-empty assistant message's text parts from an `agent_end`
/// frame (`messages: [{role, content: [{type:"text", text}…]}…]`).
fn last_assistant_text(f: &Value) -> Option<String> {
    let msgs = f.get("messages")?.as_array()?;
    for m in msgs.iter().rev() {
        if m.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let mut out = String::new();
        if let Some(parts) = m.get("content").and_then(|c| c.as_array()) {
            for p in parts {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                        out.push_str(t);
                    }
                }
            }
        }
        if !out.trim().is_empty() {
            return Some(out);
        }
    }
    None
}

/// The start row + its info notification, in one blocking DB step. Called
/// BEFORE the child spawns — the running row is visible while it runs.
async fn write_start(
    item_id: i64,
    target: Target,
    reference: &str,
    run_id: i64,
    meta: StartMeta,
    project: Option<String>,
    title: String,
) -> Result<(), Error> {
    let reference = reference.to_string();
    db_ex(move |conn| {
        db_start(conn, item_id, target, &reference, run_id, &meta)?;
        let text = format!(
            "делегация дела {item_id} запущена ({}) — {}",
            target.as_str(),
            crate::util::short(&title, 60)
        );
        db::insert_notification(conn, "info", project.as_deref(), &text)?;
        publish_run(item_id, "running");
        Ok(())
    })
    .await
}

/// The process target's exit: review on 0, failed otherwise (+ notification,
/// which names the last ring action — a bare «упало (exit N)» is not enough
/// to decide what to do next).
pub(crate) fn on_process_exit(
    item_id: i64,
    project: Option<&str>,
    code: i32,
    cancelled: &AtomicBool,
) {
    if cancelled.load(Ordering::SeqCst) {
        return; // the cancel path already wrote the «failed» state
    }
    let tail = job(item_id).map(|j| j.run.tail_lines(TAIL_LINES)).unwrap_or_default();
    let (state, level, text) = if code == 0 {
        ("review", "info", format!("делегация дела {item_id}: готово к ревью (exit 0)"))
    } else {
        let action = last_action(&tail)
            .map(|l| format!("; последнее действие: {l}"))
            .unwrap_or_default();
        ("failed", "err", format!("делегация дела {item_id}: упало (exit {code}){action}"))
    };
    // a process worker has no parsed final message — the contract block is an
    // rpc-turn artifact; no rpc frames — no command ledger either
    settle_terminal(item_id, project, state, level, text, Some(code as i64), cancelled, tail, None, None);
    if state == "review" {
        let run_id = job(item_id).map(|j| j.run.id).unwrap_or(0);
        if run_id > 0 {
            spawn_gate(item_id, run_id);
        }
    }
}

/// The shared terminal transition of a run (skipped after a cancel): state +
/// exit + the persisted ring tail + the notification, one DB lock. The tail
/// is what survives a server restart — every terminal writes it. `cmds` is
/// the proof-of-work ledger of rpc runs (`None` = process targets/legacy —
/// keep what is stored).
fn settle_terminal(
    item_id: i64,
    project: Option<&str>,
    state: &str,
    level: &str,
    text: String,
    exit: Option<i64>,
    cancelled: &AtomicBool,
    tail: Vec<String>,
    final_text: Option<String>,
    cmds: Option<String>,
) {
    if cancelled.load(Ordering::SeqCst) {
        return;
    }
    let conn = db::db_static().lock();
    if let Err(e) = db_finish(&conn, item_id, state, exit, Some(&tail_json(&tail)), final_text.as_deref(), cmds.as_deref()) {
        log::warn!("delegate: turn state write failed: {e}");
    }
    if let Err(e) = db::insert_notification(&conn, level, project, &text) {
        log::warn!("delegate: turn notification failed: {e}");
    }
    drop(conn);
    publish_run(item_id, state);
    // Run dossier as files (EAC gate-log pattern): the columns above are the
    // review-card showcase, the directory is the full dossier. The run id
    // comes from the live job — a settle without one (the unit test drives
    // this handler jobless) writes nothing, which keeps tests off the real
    // data_dir.
    let dossier_run = job(item_id).map(|j| j.run.id);
    if let Some(run_id) = dossier_run {
        let dir = runs_dir(item_id, run_id);
        let mut tail_text = tail.join("\n");
        if !tail_text.is_empty() {
            tail_text.push('\n');
        }
        write_artifact(&dir, "tail.txt", &tail_text);
        if let Some(f) = &final_text {
            write_artifact(&dir, "final.md", f);
        }
        if let Some(c) = &cmds {
            write_artifact(&dir, "cmds.json", c);
        }
    }
    // review: the acceptance gate measures first (verify commands, dossier,
    // gate event), the reviewer judges after — with the gate's numbers.
    if state == "review" {
        if let Some(run_id) = dossier_run {
            spawn_gate(item_id, run_id);
        }
    }
}

/// The persisted form of a ring tail: a JSON array of lines.
fn tail_json(tail: &[String]) -> String {
    serde_json::to_string(tail).unwrap_or_else(|_| "[]".to_string())
}

/// The persisted form of the tool-call ledger: a JSON array of CmdRec.
fn cmds_json(cmds: &[CmdRec]) -> String {
    serde_json::to_string(cmds).unwrap_or_else(|_| "[]".to_string())
}

/// Каталог артефактов одного прогона — `<data_dir>/runs/<item_id>-r<run_id>/`.
/// The DB columns are the review-card showcase; this directory is the full
/// dossier (pattern EAC gate-logs): `tail.txt`, `final.md`, `cmds.json`,
/// `review.md`. Never auto-pruned — ручная уборка.
pub fn runs_dir(item_id: i64, run_id: i64) -> PathBuf {
    cfg::data_dir().join("runs").join(format!("{item_id}-r{run_id}"))
}

/// Best-effort artifact write: a failure logs and moves on — the DB showcase
/// never depends on the files landing.
fn write_artifact(dir: &Path, name: &str, body: &str) {
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join(name), body)
    };
    if let Err(e) = write() {
        log::warn!("delegate: артефакт {} не записан: {e}", dir.join(name).display());
    }
}

/// Push one `run` event onto the board bus: the panel re-reads the item
/// (debounced) and any open review dialog for this item refetches evidence.
/// Fire-and-forget — a mutation never waits for a watcher.
fn publish_run(item_id: i64, state: &str) {
    crate::http::web::events::publish(&serde_json::json!({"type": "run", "id": item_id, "state": state}));
}

/// The row already says `running`, but no child could be spawned — settle it
/// honestly instead of lying forever (write_start now precedes the spawn).
fn settle_spawn_failed(item_id: i64, project: Option<&str>, detail: &str) {
    let conn = db::db_static().lock();
    if let Err(e) = db_finish(&conn, item_id, "failed", None, None, None, None) {
        log::warn!("delegate: spawn-failure state write failed: {e}");
    }
    publish_run(item_id, "failed");
    let text = format!(
        "делегация дела {item_id}: воркер не запустился — {}",
        crate::util::short(detail, 200)
    );
    if let Err(e) = db::insert_notification(&conn, "err", project, &text) {
        log::warn!("delegate: spawn-failure notification failed: {e}");
    }
}

/// The worker made no visible change in `dir`: the tree is clean AND nothing
/// was committed since the recorded start. Not decidable outside a git repo
/// or without a start base — the answer there is "assume it did" (the review
/// card shows the truth); only a clean tree with a known base may say «ничего
/// не сделал». Sync by design: it runs on the turn-watcher thread at settle
/// time, over a local repo.
fn worker_made_changes(dir: &Path, start: Option<&str>) -> bool {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output();
    let Ok(out) = status else { return true };
    if !out.status.success() || !out.stderr.is_empty() {
        return true; // not a repo / git failed — honest unknown
    }
    if !out.stdout.is_empty() {
        return true; // dirty tree: staged, unstaged or untracked work
    }
    match start {
        Some(base) => std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["log", &format!("{base}..HEAD"), "--oneline"])
            .output()
            .map(|o| o.status.success() && !o.stdout.is_empty())
            .unwrap_or(true),
        None => true, // no base — emptiness cannot be proven
    }
}

// ----------------------------------------------------------------- cancel ----

/// `POST …/delegate/cancel` — stop the worker or the delegation's rpc child:
/// `systemctl --user stop` for a transient-unit child (SIGTERM to the whole
/// cgroup, grandchildren included) or a bare SIGTERM otherwise — SIGKILL
/// after the grace in both modes — and settle the item as `failed`.
pub async fn cancel(item_id: i64) -> Result<Value, Error> {
    let Some(job) = job(item_id).filter(|j| j.run.active()) else {
        return Err(Error::Busy("ничего не выполняется".to_string()));
    };
    job.cancelled.store(true, Ordering::SeqCst);
    let pid = job.run.pid;
    let unit = job.unit.clone();
    let run = Arc::clone(&job.run);
    // the tail outlives the in-memory ring (a restart must not erase it)
    let tail = tail_json(&run.tail_lines(TAIL_LINES));
    tokio::task::spawn_blocking(move || match unit.as_deref() {
        Some(u) => omp_rpc::stop_unit_graceful(u, pid, || run.active(), KILL_GRACE),
        None => omp_rpc::kill_graceful(pid, || run.active(), KILL_GRACE),
    });
    // close the ring here so /run stops reporting "active" at once
    job.run.finish(-1);
    let project = tokio::task::spawn_blocking(move || {
        let conn = db::db_static().lock();
        let project: Option<String> = conn
            .query_row("SELECT project FROM items WHERE id=?1", [item_id], |r| r.get(0))
            .unwrap_or(None);
        let text = format!("делегация дела {item_id}: отменено");
        let _ = db_finish(&conn, item_id, "failed", None, Some(&tail), None, None);
        if let Err(e) = db::insert_notification(&conn, "warn", project.as_deref(), &text) {
            log::warn!("delegate: cancel notification failed: {e}");
        }
        publish_run(item_id, "failed");
    })
    .await;
    if let Err(e) = project {
        log::warn!("delegate: cancel task failed: {e}");
    }
    Ok(json!({"ok": true}))
}

// ------------------------------------------------------------------- run -----

/// `GET …/run` — the run's state + ring tail. The in-memory ring is the live
/// tail while the server remembers the run; once it is gone (restart), the
/// tail persisted at the terminal takes over, so the evidence survives.
pub async fn run_status(item_id: i64) -> Result<Value, Error> {
    let (state, started, ended, exit, run_id, attempts, stored_tail, stored_final, stored_cmds, stored_review) =
        tokio::task::spawn_blocking(move || {
            let conn = db::db_static().lock();
            conn.query_row(
                "SELECT delegation_state, delegation_started, delegation_ended, delegation_exit, \
                 delegation_run_id, delegation_attempts, delegation_tail, delegation_final, \
                 delegation_cmds, delegation_review FROM items WHERE id=?1",
                [item_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<f64>>(1)?,
                        r.get::<_, Option<f64>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, Option<String>>(7)?,
                        r.get::<_, Option<String>>(8)?,
                        r.get::<_, Option<String>>(9)?,
                    ))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    Error::NotFound(format!("item {item_id} not found"))
                }
                e => Error::Io(format!("{e}")),
            })
        })
        .await
        .map_err(|e| Error::Io(format!("blocking task: {e}")))??;
    let (lines, truncated) = match job(item_id) {
        Some(j) => (j.run.lines_json(), j.run.truncated()),
        None => (persisted_tail(&stored_tail), false),
    };
    let duration_s = started.map(|s| {
        let end = ended.unwrap_or_else(crate::util::now);
        ((end - s).max(0.0) * 1000.0).round() / 1000.0
    });
    // proof-of-work: count + fails + last 20 calls of the persisted ledger;
    // None (no ledger) keeps the key absent rather than lying with zeros
    let cmds = persisted_cmds(&stored_cmds).map(|list| {
        let count = list.len();
        let fails = list.iter().filter(|c| c.error).count();
        let skip = count.saturating_sub(20);
        json!({"count": count, "fails": fails, "last": &list[skip..]})
    });
    // the run's on-disk dossier (tail/final/cmds/review); only when it exists
    let artifacts_dir = run_id
        .map(|rid| runs_dir(item_id, rid))
        .filter(|p| p.is_dir())
        .map(|p| p.to_string_lossy().into_owned());
    // the acceptance gate's report (null = not run yet / no commands)
    let gate = run_id.and_then(|rid| read_gate_report(item_id, rid));
    Ok(json!({
        "state": state,
        "lines": lines,
        "exit": exit,
        "started": started,
        "duration_s": duration_s,
        "truncated": truncated,
        // the worker's final-message contract block (null while running)
        "final": stored_final,
        // the tool-call ledger summary (null = process target / legacy row)
        "cmds": cmds,
        // the fresh-context reviewer's answer (null = reviewer off / not ready)
        "review": stored_review,
        // the on-disk run dossier (null = not written yet / legacy run)
        "artifacts_dir": artifacts_dir,
        // the machine gate: {ran, ok, results:[{command, ok, tail}]}
        "gate": gate,
        // autopilot retry counter of the current cycle (0 = ручной режим)
        "attempts": attempts,
    }))
}

/// Decode the persisted tool-call ledger; anything undecodable is honestly
/// absent (no ledger).
fn persisted_cmds(stored: &Option<String>) -> Option<Vec<CmdRec>> {
    stored
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<CmdRec>>(s).ok())
}

/// Decode the persisted ring tail (a JSON array of lines written at the run's
/// terminal); anything undecodable is an honest empty tail.
fn persisted_tail(stored: &Option<String>) -> Vec<Value> {
    stored
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
        .unwrap_or_default()
}

// ------------------------------------------------------------------ diff -----

/// Cap text by lines then bytes; `true` = something was dropped.
pub fn cap(text: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    let mut out = String::new();
    let mut truncated = false;
    for (i, line) in text.lines().enumerate() {
        if i >= max_lines || out.len() + line.len() + 1 > max_bytes {
            truncated = true;
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, truncated)
}

/// `GET …/diff` — the worker's changes relative to the START HEAD recorded at
/// delegation start (`git diff <start>` covers committed AND uncommitted work
/// — a worker that commits like item 24 is as visible as one that leaves the
/// tree dirty), plus the worker's commit list (`git log <start>..HEAD`), plus
/// every UNTRACKED file as its own hunk (a worker that only adds files leaves
/// them untracked). Never a bare `git diff HEAD`: that is blind to committed
/// work and mixes in foreign edits made after the start. Rows without a
/// recorded start (legacy) fall back to the old behavior and say so with
/// `stale:true`. Honest `ok:false` when there is nothing to diff or the ref
/// is no git repo.
pub async fn diff(item_id: i64) -> Result<Value, Error> {
    let loaded = load_bg(item_id).await?;
    let item = &loaded.item;
    let reference = match item.target.as_deref() {
        Some("worktree") | Some("dir") => item.reference.clone(),
        // the orchestrator turn works in the item's project checkout
        Some("orchestrator") => loaded
            .project_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        _ => None,
    };
    let Some(reference) = reference else {
        return Ok(json!({"ok": false, "ref": Value::Null, "detail": "делегации не было"}));
    };
    if !Path::new(&reference).exists() {
        return Ok(json!({"ok": false, "ref": reference, "detail": "каталог не найден"}));
    }
    let (in_repo, _) = git(&["-C", &reference, "rev-parse", "--is-inside-work-tree"], 15).await;
    if !in_repo {
        return Ok(json!({"ok": false, "ref": reference, "detail": "не git-репозиторий"}));
    }
    // The diff base: the recorded start HEAD, or a legacy bare HEAD.
    let stale = item.head.is_none();
    let base = item.head.clone().unwrap_or_else(|| "HEAD".to_string());
    let (stat_ok, stat_raw) = git(&["-C", &reference, "diff", &base, "--stat"], 30).await;
    let (diff_ok, diff_raw) = git(&["-C", &reference, "diff", &base], 30).await;
    // an unborn HEAD (no commits yet) is not an error for the review gate
    let mut stat = if stat_ok { stat_raw } else { String::new() };
    let mut text = if diff_ok { diff_raw } else { String::new() };

    // the worker's commits since the start — the review card lists them above
    // the diff; a legacy row has no base, so there is no honest commit range
    let mut commits = String::new();
    if !stale {
        let (log_ok, log_raw) = git(
            &["-C", &reference, "log", &format!("{base}..HEAD"), "--oneline"],
            30,
        )
        .await;
        if log_ok {
            commits = log_raw;
        }
    }

    let (_, porcelain) = git(
        &["-C", &reference, "status", "--porcelain", "--untracked-files=all"],
        30,
    )
    .await;
    let mut untracked: Vec<String> = porcelain
        .lines()
        .filter_map(|l| l.strip_prefix("?? ").map(|p| p.trim().to_string()))
        .collect();
    untracked.sort();
    for path in &untracked {
        let s = git_output(
            &["-C", &reference, "diff", "--no-index", "--stat", "--", "/dev/null", path],
            30,
        )
        .await;
        let d = git_output(
            &["-C", &reference, "diff", "--no-index", "--", "/dev/null", path],
            30,
        )
        .await;
        push_chunk(&mut stat, &s);
        push_chunk(&mut text, &d);
    }

    let (stat, stat_cut) = cap(&stat, DIFF_CAP_LINES, DIFF_CAP_BYTES);
    let (text, diff_cut) = cap(&text, DIFF_CAP_LINES, DIFF_CAP_BYTES);
    let (commits, commit_cut) = cap(&commits, DIFF_CAP_LINES, DIFF_CAP_BYTES);
    Ok(json!({
        "ok": true,
        "ref": reference,
        "start": item.head,
        "stale": stale,
        "stat": stat,
        "diff": text,
        "commits": commits,
        "truncated": stat_cut || diff_cut || commit_cut,
    }))
}

/// Append one diff chunk, guaranteeing a newline boundary between hunks.
fn push_chunk(out: &mut String, chunk: &str) {
    if chunk.trim().is_empty() {
        return;
    }
    out.push_str(chunk);
    if !out.ends_with('\n') {
        out.push('\n');
    }
}

// ---------------------------------------------------------------- accept -----

/// `POST …/accept` — the review gate. `merge:false` just closes the item;
/// `merge:true` (worktree targets only) merges `task/<slug>` into the project
/// branch, aborts on conflict (state stays `review`) and removes the worktree.
pub async fn accept(item_id: i64, merge: bool) -> Result<Value, Error> {
    let loaded = load_bg(item_id).await?;
    if loaded.item.state != "review" {
        return Err(Error::Busy(format!(
            "дело {item_id} не на ревью (состояние '{}')",
            loaded.item.state
        )));
    }
    let item = &loaded.item;
    let is_worktree = item.target.as_deref() == Some("worktree");
    let project = item.project.clone();

    if merge {
        if !is_worktree {
            return Err(Error::Bad(
                "merge доступен только для цели worktree".to_string(),
            ));
        }
        let wt = item
            .reference
            .clone()
            .ok_or_else(|| Error::Bad("у дела нет ворктри".to_string()))?;
        let checkout = loaded
            .project_path
            .clone()
            .ok_or_else(|| no_path(item, Target::Worktree))?;
        let checkout_s = checkout.to_string_lossy().into_owned();
        let branch = format!("task/{}", slug(item.id, &item.title));
        // `merge` merges the BRANCH, so work a worker left uncommitted would be
        // lost — the pending worktree state (the very work `/diff` showed on
        // review) is committed onto `task/<slug>` first, under an honest atlas
        // message. A failure here keeps the item on review instead of dropping
        // the work silently.
        let (_, pending) = git(&["-C", &wt, "status", "--porcelain"], 30).await;
        if !pending.trim().is_empty() {
            let (added, add_out) = git(&["-C", &wt, "add", "-A"], 60).await;
            let msg = format!(
                "atlas: делегация дела {} — {}",
                item.id,
                crate::util::short(&item.title, 60)
            );
            let (committed, out) = if added {
                git(&["-C", &wt, "commit", "-m", &msg], 60).await
            } else {
                (false, add_out)
            };
            if !committed {
                let note = format!(
                    "воркер оставил незакоммиченные правки — закоммитить не удалось: {}",
                    crate::util::short(out.trim(), 200)
                );
                let text = format!("делегация дела {item_id}: {note}");
                db_ex(move |conn| {
                    db::insert_notification(conn, "warn", project.as_deref(), &text).map(|_| ())
                })
                .await?;
                return Ok(json!({"ok": false, "detail": note}));
            }
        }
        let (ok, out) = git(&["-C", &checkout_s, "merge", "--no-edit", &branch], 60).await;
        if !ok {
            let _ = git(&["-C", &checkout_s, "merge", "--abort"], 30).await;
            let text = format!(
                "делегация дела {item_id}: конфликт слияния — решай вручную (ворктри остался: {wt}); {}",
                crate::util::short(out.trim(), 200)
            );
            db_ex(move |conn| {
                db::insert_notification(conn, "warn", project.as_deref(), &text).map(|_| ())
            })
            .await?;
            return Ok(json!({"ok": false, "detail": "конфликт слияния — решай вручную"}));
        }
        let (removed, rm_out) = git(
            &["-C", &checkout_s, "worktree", "remove", "--force", &wt],
            60,
        )
        .await;
        if !removed {
            log::warn!("delegate: worktree remove failed: {rm_out}");
        }
        let title = crate::util::short(&item.title, 60);
        db_ex(move |conn| {
            db_set_state(conn, item_id, "done")?;
            let text = format!("делегация дела {item_id}: принято и слито — {title}");
            db::insert_notification(conn, "info", project.as_deref(), &text)?;
            publish_run(item_id, "done");
            Ok(())
        })
        .await?;
        return Ok(json!({"ok": true, "merged": true}));
    }

    let title = crate::util::short(&item.title, 60);
    let wt = item.reference.clone();
    db_ex(move |conn| {
        db_set_state(conn, item_id, "done")?;
        // the contract's wording: the worktree target names where it stays
        let text = match wt {
            Some(p) => format!("делегация дела {item_id}: принято, ворктри остался: {p}"),
            None => format!("делегация дела {item_id}: принято — {title}"),
        };
        db::insert_notification(conn, "info", project.as_deref(), &text)?;
        publish_run(item_id, "done");
        Ok(())
    })
    .await?;
    Ok(json!({"ok": true}))
}

// ---------------------------------------------------------------- reject -----

/// `POST …/reject` — store the feedback and put the item back to `queued`, so
/// the next delegate compiles it into the «Фидбек» section of the brief. The
/// answer channel of the state machine: review and `needs-feedback` answer
/// through it, and a `failed` run revives the same way — feedback stored, the
/// failed run's own session file stays recorded (`db_start` wrote it,
/// `db_finish` kept it), so the next delegate resumes that session via
/// `session_decision`.
pub async fn reject(item_id: i64, feedback: String) -> Result<Value, Error> {
    let feedback = feedback.trim().to_string();
    if feedback.is_empty() {
        return Err(Error::Bad("feedback обязателен".to_string()));
    }
    let loaded = load_bg(item_id).await?;
    // exactly three states may return with feedback; everything else refuses
    // honestly — a live queued/running delegation is a 409 (cancel first),
    // an item that was never delegated has nothing to return
    let state = loaded.item.state.as_str();
    if !matches!(state, "review" | "needs-feedback" | "failed") {
        return Err(match state {
            "queued" | "running" => Error::Busy(format!(
                "сначала отмени текущую делегацию (состояние '{}')",
                loaded.item.state
            )),
            _ => Error::Bad(format!(
                "дело {item_id}: делегации не было (состояние '{}') — возвращать нечего",
                loaded.item.state
            )),
        });
    }
    let project = loaded.item.project.clone();
    db_ex(move |conn| {
        db_set_state(conn, item_id, "queued")?;
        db_set_feedback(conn, item_id, &feedback)?;
        // ручной reject = «рули я»: автопилот молчит, пока владелец не скажет
        conn.execute(
            "UPDATE items SET delegation_attempts=0 WHERE id=?1",
            rusqlite::params![item_id],
        )?;
        let text = format!(
            "делегация дела {item_id}: отклонено — {}",
            crate::util::short(&feedback, 120)
        );
        db::insert_notification(conn, "warn", project.as_deref(), &text)?;
        publish_run(item_id, "queued");
        Ok(())
    })
    .await?;
    Ok(json!({"ok": true}))
}

// -----------------------------------------------------------------  reap -----

/// Server start: the ring and the exit watcher of a run die with the process,
/// so a `queued`/`running` row from a previous life would lie forever. In
/// systemd mode the CHILD may still be alive — transient units survive an
/// atlas restart within the user session — so each stale row first asks
/// `systemctl --user is-active` for its `atlas-deleg-*` unit (the name is a
/// pure function of the row's id+title) and genuinely live workers are left
/// `running`. Confirmed-gone rows — and everything on a box without
/// systemd-run — settle as `failed` and say so once, as before.
/// `needs-feedback` is NOT stale: it is terminal-until-answered, the owner's
/// answer (or a reset) moves it — a restart changes nothing there.
pub fn reap_stale() {
    // first touch of the spawn mode: runs the feature-detect and the one
    // active-mode log line at startup
    let mode = *omp_rpc::SPAWN_MODE;
    let conn = db::db_static().lock();
    let stale: Vec<(i64, String)> = conn
        .prepare("SELECT id, title FROM items WHERE delegation_state IN ('queued','running')")
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    if stale.is_empty() {
        return;
    }
    let mut reaped: Vec<i64> = Vec::new();
    let mut live: usize = 0;
    for (id, title) in &stale {
        if mode == omp_rpc::SpawnMode::Systemd
            && omp_rpc::unit_is_active(&unit_name(*id, title)) == Some(true)
        {
            live += 1; // the worker outlived the server in this session
            continue;
        }
        reaped.push(*id);
    }
    for id in &reaped {
        if let Err(e) = db_finish(&conn, *id, "failed", None, None, None, None) {
            log::warn!("delegate: reap item {id}: {e}");
        }
    }
    if reaped.is_empty() {
        log::info!("delegate: {live} застрявших дел ещё живут в юнитах atlas-deleg-*");
        return;
    }
    let mut text = format!(
        "делегации прерваны перезапуском сервера: {} (состояние failed)",
        reaped.len()
    );
    if live > 0 {
        text.push_str(&format!(
            "; ещё {live} воркер(ов) живы в systemd-юнитах atlas-deleg-*"
        ));
    }
    let _ = db::insert_notification(&conn, "warn", None, &text);
    log::warn!("delegate: {text}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> Item {
        Item {
            id: 7,
            title: "починить сборку".into(),
            project: Some("lamplife".into()),
            description: Some("  релиз падает после обновления  ".into()),
            state: String::new(),
            target: None,
            reference: None,
            head: None,
            session: None,
            feedback: None,
            steps: vec![],
            files: vec![],
        }
    }

    /// The brief is the worker's whole contract: title, description (or the
    /// honest placeholder), the autonomy line that now points at ВОПРОСЫ, and
    /// the mandatory final-message block LAST; the optional sections appear
    /// ONLY when they carry content.
    #[test]
    fn brief_without_optional_sections() {
        let it = item();
        let brief = compile_brief(&it, Path::new("/tmp/wt"));
        assert_eq!(
            brief,
            "# починить сборку\n\
             релиз падает после обновления\n\
             Работай в каталоге /tmp/wt. Ты работаешь автономно: ответить на вопрос некому — \
             если данных не хватает, остановись, заполни раздел ВОПРОСЫ, ничего не ломай.\n\
             ## Проверка\n\
             По ходу работы проверяй как обычно (сборка, точечные тесты). ФИНАЛЬНУЮ \
             верификацию воркер не гоняет — её при приёмке выполняет гейт atlas.\n\
             В ПРОВЕРЕНО перечисли команды гейта: по одной на строку, префикс \"$ \" \
             (найди сам: Makefile-цели, cargo, go, npm/pnpm, pytest…).\n\
             Нечего проверять — напиши в ПРОВЕРЕНО, почему.\n\
             ## Итог (обязательно)\n\
             Последнее сообщение хода — ровно четыре раздела, один за другим:\n\
             СДЕЛАНО: что сделано по делу\n\
             ИЗМЕНЕНО: перечень изменённых файлов\n\
             ПРОВЕРЕНО: команды проверки для гейта приёмки ($-строки) или почему нечего\n\
             ВОПРОСЫ: вопросы по делу; нечего спрашивать — напиши «нет»\n\
             Без этого блока работа не считается завершённой. Вместо догадки — ВОПРОСЫ.\n"
        );
        assert!(!brief.contains("## Шаги"));
        assert!(!brief.contains("## Файлы"));
        assert!(!brief.contains("## Фидбек"));
        // the old «прими разумное допущение» is gone: guessing was the item-23 harm
        assert!(!brief.contains("допущение"), "{brief}");
        // the role split is explicit: the worker does NOT run the final check
        assert!(brief.contains("не гоняет — её при приёмке выполняет гейт"), "{brief}");
    }

    #[test]
    fn brief_falls_back_to_the_placeholder_description() {
        let mut it = item();
        it.description = None;
        assert!(compile_brief(&it, Path::new("/p")).contains("\n(без описания)\n"));
        it.description = Some("   \n  ".into());
        assert!(
            compile_brief(&it, Path::new("/p")).contains("\n(без описания)\n"),
            "blank descriptions are «(без описания)» too"
        );
    }

    #[test]
    fn brief_carries_steps_files_and_feedback() {
        let mut it = item();
        it.steps = vec![
            ("первый".into(), true),
            ("второй".into(), false),
            ("третий".into(), false),
        ];
        it.files = vec!["/abs/Cargo.toml".into(), "/abs/two.rs".into()];
        it.feedback = Some("тесты не запускались".into());
        let brief = compile_brief(&it, Path::new("/home/u/.atlas-wt/lamplife-7-ab12"));
        assert!(
            brief.starts_with(
                "# починить сборку\n\
                 релиз падает после обновления\n\
                 ## Шаги\n\
                 - [x] первый\n\
                 - [ ] второй\n\
                 - [ ] третий\n\
                 ## Файлы (прочти перед работой)\n\
                 - /abs/Cargo.toml\n\
                 - /abs/two.rs\n\
                 ## Фидбек к прошлой попытке\n\
                 тесты не запускались\n"
            ),
            "{brief}"
        );
        // the contract block is still the LAST thing the worker reads
        assert!(brief.ends_with("Без этого блока работа не считается завершённой. Вместо догадки — ВОПРОСЫ.\n"), "{brief}");
        // verify hints are derived from the attached files' basenames
        assert!(brief.contains("Rust — cargo build && cargo test"), "{brief}");
        assert!(!brief.contains("go test ./..."), "{brief}");
    }

    /// Session pinning: fresh per-run by default; resume only as a true
    /// follow-up (feedback + recorded session file within the size cap);
    /// oversized or missing files fall back to fresh, loudly.
    #[test]
    fn session_decision_resumes_only_true_follow_ups() {
        let dir = std::env::temp_dir().join(format!("deleg-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("small.jsonl");
        std::fs::write(&small, "бриф\nответ\n").unwrap();
        let big = dir.join("big.jsonl");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(6 * 1024 * 1024).unwrap();
        drop(f);
        let fresh = PathBuf::from("/data/atlas-deleg-7-ab12-r9.jsonl");

        // no feedback, blank feedback, no recorded session, missing file → fresh
        for (fb, stored) in [
            (None, Some(small.to_string_lossy().as_ref() as &str)),
            (Some("   "), Some(small.to_string_lossy().as_ref())),
            (Some("правь"), None),
            (Some("правь"), Some(dir.join("gone.jsonl").to_string_lossy().as_ref())),
        ] {
            match session_decision(fb, stored, &fresh) {
                omp_rpc::Session::Fresh(p) => assert_eq!(p, fresh, "fb={fb:?} stored={stored:?}"),
                other => panic!("expected fresh for fb={fb:?} stored={stored:?}: {other:?}"),
            }
        }

        // feedback + recorded file → resume THAT file (no size cap: the
        // follow-up resumes its own run's history, never alien sessions)
        match session_decision(Some("правь"), Some(small.to_string_lossy().as_ref()), &fresh) {
            omp_rpc::Session::Resume(p) => assert_eq!(p, small),
            other => panic!("expected resume: {other:?}"),
        }
        match session_decision(Some("правь"), Some(big.to_string_lossy().as_ref()), &fresh) {
            omp_rpc::Session::Resume(p) => assert_eq!(p, big, "a big OWN session resumes too"),
            other => panic!("expected resume of the big own session: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The per-run session PIN (a placeholder inside the run's dossier dir —
    /// omp 18.9 mints fresh sessions inside a --session-dir under its own
    /// name) is distinct per run and deterministic.
    #[test]
    fn run_session_path_is_per_run_and_deterministic() {
        let a = run_session_path(7, "дело", 1);
        let b = run_session_path(7, "дело", 2);
        assert_ne!(a, b, "each run gets its own session pin");
        assert_eq!(a, run_session_path(7, "дело", 1), "stable for the same run");
        assert_eq!(a.parent(), Some(runs_dir(7, 1).as_path()), "pin lives in the dossier dir");
        assert!(a.ends_with("session.jsonl"), "{a:?}");
    }

    /// The model pin: an empty `[delegation] model` (the default — and the
    /// config-dev reality) contributes NO argv, so omp's default applies.
    #[test]
    fn model_args_omit_the_flag_without_a_configured_model() {
        // unit tests never run init_config, so config() is the defaults —
        // whose delegation_model is "" (see the cfg tests for the parse side)
        assert!(model_args().is_empty());
    }
    /// Verify hints map the item's files to the right ecosystems, dedupe on
    /// pyproject/requirements, and stay silent for unrelated files.
    #[test]
    fn verify_hints_point_at_the_ecosystem() {
        let h = verify_hints(&["/repo/Cargo.toml".into(), "/repo/src/main.rs".into()]);
        assert_eq!(h, ["Rust — cargo build && cargo test"]);
        let h = verify_hints(&["go.mod".into(), "package.json".into(), "Makefile".into()]);
        assert_eq!(h.len(), 3);
        assert!(h.iter().any(|x| x.contains("go test")));
        let h = verify_hints(&["pyproject.toml".into(), "requirements.txt".into()]);
        assert_eq!(h, ["Python — pytest"], "the two python markers dedupe");
        assert!(verify_hints(&["/repo/заметка.md".into()]).is_empty());
    }

    /// The final-message contract parser: four headers, each section up to the
    /// next one, tolerant of decoration and of same-line content, `None` when
    /// the block is absent.
    #[test]
    fn final_block_parser_reads_all_four_sections() {
        let full = "какой-то текст хода\n\
                    СДЕЛАНО: поправил формулу\n\
                    ИЗМЕНЕНО: parsers/sentimentscore.go\n\
                    ПРОВЕРЕНО: go test ./... — ok\n\
                    ВОПРОСЫ: что делать с КВ?\n\
                    и второй вопрос\n";
        let r = parse_final_block(full).expect("contract present");
        assert_eq!(r.done, "поправил формулу");
        assert_eq!(r.changed, "parsers/sentimentscore.go");
        assert_eq!(r.verified, "go test ./... — ok");
        assert_eq!(r.questions, "что делать с КВ?\nи второй вопрос");

        // same-line content only; empty ВОПРОСЫ means no questions
        let quiet = "СДЕЛАНО: сделано\nИЗМЕНЕНО: один файл\nПРОВЕРЕНО: тесты зелёные\nВОПРОСЫ: нет\n";
        let r = parse_final_block(quiet).expect("contract present");
        assert_eq!(r.questions, "нет");

        // markdown decoration and # headers tolerated
        let marked = "- **СДЕЛАНО:** x\n**ИЗМЕНЕНО:** y\n# ПРОВЕРЕНО: z\nВОПРОСЫ:\n- вопрос раз\n";
        let r = parse_final_block(marked).expect("contract present");
        assert_eq!(r.done, "x");
        assert_eq!(r.changed, "y");
        assert_eq!(r.verified, "z");
        assert_eq!(r.questions, "вопрос раз");

        // a near-miss token does not open a section
        let r = parse_final_block("СДЕЛАННОЕ дело\n").is_none();
        assert!(r, "no СДЕЛАНО header — no block");

        // absent block → None (the model ignored the contract)
        assert_eq!(parse_final_block("я закончил, всё хорошо\n"), None);
        assert_eq!(parse_final_block(""), None);

        // text BEFORE the block is ignored; the block may start mid-text
        let late = "промежуточные размышления\nСДЕЛАНО: a\nВОПРОСЫ: b\n";
        let r = parse_final_block(late).expect("contract present");
        assert_eq!(r.done, "a");
        assert_eq!(r.questions, "b");

        // a section header repeated later RESTARTS that section
        let repeated = "СДЕЛАНО: первое\nСДЕЛАНО: второе\n";
        let r = parse_final_block(repeated).expect("contract present");
        assert_eq!(r.done, "второе");

        // «нет» (the brief's own no-questions spelling, any punctuation/case)
        // is NOT a question; real questions are
        for quiet in ["нет", "Нет.", "НЕТ", "нет вопросов", "«нет»", ""] {
            let r = parse_final_block(&format!(
                "СДЕЛАНО: x\nИЗМЕНЕНО: y\nПРОВЕРЕНО: z\nВОПРОСЫ: {quiet}\n"
            ))
            .unwrap();
            assert!(r.questions_empty(), "«{quiet}» must read as no questions");
        }
        let asked = parse_final_block(
            "СДЕЛАНО: x\nИЗМЕНЕНО: y\nПРОВЕРЕНО: z\nВОПРОСЫ: что за КВ?\n",
        )
        .unwrap();
        assert!(!asked.questions_empty());
    }

    /// Если в каталоге проекта есть `graft/` — бриф велит ориентироваться
    /// навыком graft (граф контекста кода установлен, но воркеры о нём не знают,
    /// пока им не сказать).
    #[test]
    fn brief_points_at_graft_when_the_project_has_one() {
        let dir = std::env::temp_dir().join(format!("graft-brief-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("graft")).expect("mkdir graft");
        let brief = compile_brief(&item(), &dir);
        assert!(brief.contains("В проекте есть graft/"), "{brief}");
        assert!(brief.contains("навык graft"), "{brief}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn blank_feedback_is_dropped() {
        let mut it = item();
        it.feedback = Some("  \n ".into());
        assert!(!compile_brief(&it, Path::new("/p")).contains("## Фидбек"));
    }

    /// The slug must be stable for item+title and safe in a path and a git
    /// ref; the worktree lives under `~/.atlas-wt/<project>-<slug>`.
    #[test]
    fn slug_and_worktree_dir_are_derived() {
        let a = slug(7, "починить сборку");
        assert_eq!(a, slug(7, "починить сборку"), "stable");
        assert_eq!(a.len(), 2 + 4, "id + dash + 4 hex chars: {a}");
        assert!(a.starts_with("7-"));
        assert!(a[2..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, slug(8, "починить сборку"), "the id is part of it");
        assert_ne!(a, slug(7, "другое дело"), "the title hash is part of it");
        assert!(
            !slug(7, "two  spaces/slash").contains([' ', '/']),
            "never a space or slash: paths and refs"
        );

        let dir = worktree_dir("lamplife", 7, "починить сборку");
        assert_eq!(dir, cfg::worktree_root().join(format!("lamplife-{a}")));
        assert!(dir.starts_with(cfg::worktree_root()));
        assert_eq!(
            dir.file_name().unwrap().to_string_lossy(),
            format!("lamplife-{a}")
        );
    }

    /// The transient unit name is a pure function of item+title: stable
    /// (reap_stale rebuilds it from the DB row), distinct per item and per
    /// title, and a VALID unit name — ASCII only (id + hex hash; the title
    /// itself, Cyrillic included, never lands in the name), no spaces, no
    /// slashes, systemd-run appends `.service`.
    #[test]
    fn delegation_unit_name_is_valid_and_stable() {
        let a = unit_name(7, "починить сборку");
        assert_eq!(a, unit_name(7, "починить сборку"), "stable");
        assert_ne!(a, unit_name(7, "другое дело"), "title matters");
        assert_ne!(a, unit_name(8, "починить сборку"), "item id matters");
        assert!(a.starts_with("atlas-deleg-7-"), "{a}");
        assert_eq!(a["atlas-deleg-7-".len()..].len(), 4, "hash4 suffix: {a}");
        assert!(a.is_ascii(), "ascii only: {a}");
        assert!(!a.contains(['/', ' ']), "no slashes or spaces: {a}");
    }

    /// The orchestrator delegation works in the item's PROJECT directory and
    /// nowhere else: `delegation_ref` names the same path (the review gate
    /// diffs exactly it), and a project without a path is an honest error —
    /// the atlas repo the panel child lives in must never be a fallback.
    #[test]
    fn orchestrator_plan_is_the_project_dir() {
        let with_path = Loaded {
            item: item(),
            project_path: Some(PathBuf::from("/home/u/Projects/lamplife")),
        };
        let p = plan(Target::Orchestrator, &with_path).expect("project with a path");
        assert_eq!(p.cwd, PathBuf::from("/home/u/Projects/lamplife"));
        assert_eq!(p.reference, "/home/u/Projects/lamplife");
        assert_eq!(p.branch, None);

        let mut orphan_item = item();
        orphan_item.project = Some("phone-access".into());
        let orphan = Loaded { item: orphan_item, project_path: None };
        let e = plan(Target::Orchestrator, &orphan).expect_err("no path → no delegation");
        assert!(matches!(e, Error::NoPath(_)), "{e:?}");
    }

    #[test]
    fn diff_cap_keeps_short_text_and_marks_the_cut() {
        let short = "a\nb\nc\n";
        assert_eq!(cap(short, 400, 200_000), (short.to_string(), false));
        // the line cap trips before the byte cap
        let long: String = (0..10).map(|i| format!("line {i}\n")).collect();
        let (out, cut) = cap(&long, 3, 1_000_000);
        assert!(cut);
        assert_eq!(out, "line 0\nline 1\nline 2\n");
        // ... and the byte cap can trip first too
        let (out, cut) = cap(&long, 400, 8);
        assert!(cut);
        assert_eq!(out, "line 0\n");
        // empty input is neither truncated nor a panic
        assert_eq!(cap("", 400, 200_000), (String::new(), false));
    }

    #[test]
    fn target_parsing_and_spelling_round_trip() {
        for t in [Target::Orchestrator, Target::Worktree, Target::Dir] {
            assert_eq!(Target::parse(t.as_str()), Some(t));
        }
        assert_eq!(Target::parse("remote"), None);
        assert_eq!(TARGETS, ["orchestrator", "worktree", "dir"]);
    }

    /// The run observer, driven by injected instants and an injectable alive
    /// flag (no real sleeps, no real process): speaks on TRANSITIONS only —
    /// one ring line alive→silent, one silent→alive — and fires the warn
    /// notification exactly once per run at the threshold. NO kill decision
    /// exists: timers never stop a delegation.
    #[test]
    fn run_observer_speaks_on_transitions_and_warns_once() {
        let warn_after = Duration::from_secs(300);
        let t0 = Instant::now();
        let mut o = RunObserver::new(warn_after, t0);

        // a healthy run: ticks change nothing
        o.frame(t0 + Duration::from_secs(10));
        assert_eq!(o.tick(t0 + Duration::from_secs(30), true), None, "silence 20 s: pacing");
        // the model goes quiet: the first tick past one tick of silence is
        // the alive→silent transition (one ring line); the 300 s warn
        // threshold is far
        assert_eq!(o.tick(t0 + Duration::from_secs(60), true), Some(ObsEvent::WentSilent { secs: 50, warn: false }));
        // ... and silence continuing is NOT repeated (no spam)
        assert_eq!(o.tick(t0 + Duration::from_secs(90), true), None);

        // crossing the warn threshold while already silent: the one-shot
        // notification only, no new ring line
        assert_eq!(o.tick(t0 + Duration::from_secs(320), true), Some(ObsEvent::Warn { secs: 310 }));
        assert_eq!(o.tick(t0 + Duration::from_secs(350), true), None, "warn fires once");

        // frames return: the silent→alive transition names the silence that
        // just ended
        o.frame(t0 + Duration::from_secs(360));
        assert_eq!(o.tick(t0 + Duration::from_secs(380), true), Some(ObsEvent::Resumed { secs: 340 }));

        // a second silence: ring lines again, but the warn never repeats
        assert_eq!(o.tick(t0 + Duration::from_secs(410), true), Some(ObsEvent::WentSilent { secs: 50, warn: false }));
        assert_eq!(o.tick(t0 + Duration::from_secs(680), true), None, "one warn per run, ever");
    }

    /// `silence_warn_secs = 0` means «never warn» — transitions still speak
    /// in the ring, no notification ever fires.
    #[test]
    fn zero_warn_threshold_never_warns() {
        let t0 = Instant::now();
        let mut o = RunObserver::new(Duration::ZERO, t0);
        let ev = o.tick(t0 + SILENCE_TICK, true);
        assert_eq!(ev, Some(ObsEvent::WentSilent { secs: 30, warn: false }));
        assert_eq!(o.tick(t0 + Duration::from_secs(86_400), true), None);
    }

    /// A dead child is the exit watcher's business: the observer stays
    /// quiet, and `silence_now` is what failure messages quote as context.
    #[test]
    fn observer_ignores_dead_children_and_measures_silence() {
        let warn_after = Duration::from_secs(300);
        let t0 = Instant::now();
        let mut o = RunObserver::new(warn_after, t0);
        assert_eq!(o.tick(t0 + SILENCE_TICK * 10, false), None, "dead child: no events");
        assert_eq!(o.silence_now(t0 + SILENCE_TICK * 10), SILENCE_TICK * 10);
        // a fresh run's silence is below the reporting tick — context-worthy
        // failure messages suppress it
        let o = RunObserver::new(warn_after, t0);
        assert_eq!(o.silence_now(t0 + Duration::from_secs(5)), Duration::from_secs(5));
        assert_eq!(silence_note(Duration::from_secs(5)), "");
        assert_eq!(
            silence_note(Duration::from_secs(42)),
            "; на момент провала: тишина 42 с"
        );
    }

    /// `last_action` — the «что воркер делал» evidence for a kill message:
    /// the LAST non-empty ring line, trimmed, capped at 120 chars; an
    /// all-empty tail honestly reports nothing.
    #[test]
    fn last_action_reads_the_last_meaningful_ring_line() {
        assert_eq!(last_action(&[]), None);
        assert_eq!(last_action(&["  ".to_string(), "\n".to_string()]), None);
        let tail = vec![
            "— ход запущен —".to_string(),
            "  ".to_string(),
            "· edit {\"new_string\":\"\\tif items\"…}".to_string(),
            "правлю матрицу".to_string(),
        ];
        assert_eq!(last_action(&tail).as_deref(), Some("правлю матрицу"));

        let long = "x".repeat(300);
        let got = last_action(&[long]).expect("non-empty");
        assert!(got.chars().count() == 121, "120 chars + ellipsis: {got}");
        assert!(got.ends_with('…'));
    }

    /// The persisted-tail decode: a JSON array of lines in, lines out; garbage
    /// and absence decode to an honest empty tail.
    #[test]
    fn persisted_tail_decodes_json_or_says_nothing() {
        assert!(persisted_tail(&None).is_empty());
        assert!(persisted_tail(&Some("not json".into())).is_empty());
        let tail = tail_json(&["один".to_string(), "два".to_string()]);
        assert_eq!(
            persisted_tail(&Some(tail)),
            vec![serde_json::json!("один"), serde_json::json!("два")]
        );
    }

    /// The ledger pairs start/end frames by `toolCallId` (the golden-test wire
    /// contract): the end frame settles the error flag, an end without a start
    /// is ignored — no panic, no phantom record.
    #[test]
    fn mirror_ledger_pairs_calls_and_records_errors() {
        let run = runner::Run::with_id(2_700_001, 0);
        let mut m = Mirror::default();
        mirror_frame(&mut m, &run, &serde_json::json!({
            "type": "tool_execution_start", "toolCallId": "t1", "toolName": "bash",
            "args": {"command": "cargo test --offline"}
        }));
        mirror_frame(&mut m, &run, &serde_json::json!({
            "type": "tool_execution_start", "toolCallId": "t2", "toolName": "read",
            "args": {"path": "/tmp/x"}
        }));
        mirror_frame(&mut m, &run, &serde_json::json!({
            "type": "tool_execution_end", "toolCallId": "t1", "isError": false,
            "result": {"content": [{"type": "text", "text": "ok"}]}
        }));
        mirror_frame(&mut m, &run, &serde_json::json!({
            "type": "tool_execution_end", "toolCallId": "t2", "isError": true,
            "result": {"content": [{"type": "text", "text": "no such file"}]}
        }));
        mirror_frame(&mut m, &run, &serde_json::json!({
            "type": "tool_execution_end", "toolCallId": "ghost", "isError": true
        }));
        assert_eq!(m.cmds.len(), 2, "start frames only");
        assert_eq!(m.cmds[0], CmdRec { command: "cargo test --offline".into(), error: false });
        assert_eq!(m.cmds[1], CmdRec { command: "read {\"path\":\"/tmp/x\"}".into(), error: true });
    }

    /// The ledgered text: bash carries the real command, others name+compact
    /// args, everything capped.
    #[test]
    fn cmd_text_prefers_bash_command_and_caps() {
        assert_eq!(cmd_text("bash", Some(&serde_json::json!({"command": "ls -la"}))), "ls -la");
        assert_eq!(cmd_text("bash", Some(&serde_json::json!({}))), "bash {}");
        assert_eq!(cmd_text("read", Some(&serde_json::json!({"path": "/a"}))), "read {\"path\":\"/a\"}");
        assert_eq!(cmd_text("read", None), "read");
        let long = "x".repeat(500);
        let got = cmd_text("bash", Some(&serde_json::json!({"command": long})));
        assert!(got.chars().count() <= 201, "capped, got {}", got.chars().count());
    }

    /// The persisted ledger decodes when it is JSON; absence and garbage mean
    /// «no ledger», never zeros.
    #[test]
    fn persisted_cmds_decode_or_absent() {
        assert!(persisted_cmds(&None).is_none());
        assert!(persisted_cmds(&Some("[".into())).is_none());
        let stored = cmds_json(&[CmdRec { command: "ls".into(), error: false }]);
        let got = persisted_cmds(&Some(stored)).unwrap();
        assert_eq!(got.len(), 1);
        assert!(!got[0].error);
    }

    /// The reviewer's VERDICT contract: the first `VERDICT:` line wins, the
    /// first word after the colon is the verdict (`#`/`*` decoration and
    /// trailing punctuation tolerated); no line — no verdict.
    #[test]
    fn review_verdict_reads_the_first_verdict_line() {
        assert_eq!(
            review_verdict("разбор...\nVERDICT: accept — отвечает отчёту\nпричины"),
            Some("accept".into())
        );
        assert_eq!(review_verdict("VERDICT: block."), Some("block".into()));
        assert_eq!(review_verdict("* VERDICT: block, потому что тесты красные"), Some("block".into()));
        assert_eq!(review_verdict("# VERDICT: ACCEPT"), Some("accept".into()), "case folded");
        assert_eq!(review_verdict("работа окончена\nВЕРДИКТ: accept\nпричины"), Some("accept".into()), "cyrillic header tolerated");
        assert_eq!(review_verdict("нет вердикта, только текст"), None);
    }

    /// The reviewer's prompt carries the frozen base and the worker's report —
    /// the two facts a cold context cannot get anywhere else.
    #[test]
    fn review_prompt_carries_base_and_report() {
        let p = review_prompt("abc123", Some("СДЕЛАНО: поправил\nПРОВЕРЕНО: $ cargo test"), None);
        assert!(p.contains("abc123"), "start HEAD in the prompt");
        assert!(p.contains("СДЕЛАНО: поправил"), "worker report in the prompt");
        assert!(p.contains("VERDICT"), "the answer contract in the prompt");
        // a missing report degrades to a stated placeholder, never an empty hole
        let p2 = review_prompt("abc123", None, None);
        assert!(p2.contains("не сохранился"));
        // an oversized report is capped, the prompt stays bounded
        let big = "x".repeat(100_000);
        let p3 = review_prompt("h", Some(&big), None);
        assert!(p3.len() < 20_000, "capped prompt, got {}", p3.len());
    }

    /// The reviewer's fresh session is minted INSIDE the run's dossier dir
    /// (omp 18.9 `--session-dir`), one-shot, never resumed.
    #[test]
    fn review_session_lives_in_the_run_dossier() {
        let dir = runs_dir(7, 1);
        let _ = std::fs::remove_dir_all(&dir);
        // run_reviewer creates the dir before spawning: same call shape
        std::fs::create_dir_all(&dir).unwrap();
        let probe = dir.join("review-session.jsonl");
        std::fs::write(&probe, "placeholder\n").unwrap();
        // the dir itself is what --session-dir receives
        assert_eq!(probe.parent(), Some(dir.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without a configured `[delegation] reviewer` (unit tests run on the
    /// defaults) the reviewer stays off — spawn_reviewer becomes a no-op and
    /// today's behavior is preserved.
    #[test]
    fn reviewer_is_off_without_a_configured_model() {
        assert!(reviewer_model().is_none());
    }

    /// The run dossier: artifacts land in the per-run directory; a broken
    /// path logs (best-effort), never panics.
    #[test]
    fn run_artifacts_land_in_the_run_dir() {
        let dir = std::env::temp_dir().join(format!("atlas-artifacts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_artifact(&dir, "tail.txt", "один\nдва\n");
        write_artifact(&dir, "final.md", "СДЕЛАНО: всё");
        assert_eq!(std::fs::read_to_string(dir.join("tail.txt")).unwrap(), "один\nдва\n");
        assert_eq!(std::fs::read_to_string(dir.join("final.md")).unwrap(), "СДЕЛАНО: всё");
        // a path that cannot become a directory: warn, move on
        let file_guard = dir.join("nope");
        std::fs::write(&file_guard, "x").unwrap();
        write_artifact(&file_guard, "tail.txt", "y");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The dossier path is scoped by item AND run — two runs of one item (or
    /// two items) never share a directory.
    #[test]
    fn runs_dir_is_item_run_scoped() {
        assert_eq!(runs_dir(7, 2), cfg::data_dir().join("runs").join("7-r2"));
        assert_ne!(runs_dir(7, 2), runs_dir(7, 3));
        assert_ne!(runs_dir(7, 2), runs_dir(8, 2));
    }

    /// The gate reads the worker's `$`-lines out of ПРОВЕРЕНО: list
    /// decoration tolerated, prose ignored, at most 8 commands.
    #[test]
    fn gate_commands_read_dollar_lines() {
        let checked = "cargo test — зелёные\n\
                       $ cargo test\n\
                       - $ go test ./... --race\n\
                       * $  `npm test`\n\
                       вывод выше не команды\n";
        assert_eq!(
            gate_commands_from_checked(checked),
            ["cargo test", "go test ./... --race", "npm test"]
        );
        assert!(gate_commands_from_checked("никаких команд, только слова").is_empty());
        let eight: Vec<String> = (0..10).map(|i| format!("$ cmd{i}")).collect();
        assert_eq!(gate_commands_from_checked(&eight.join("\n")).len(), 8, "capped");
    }

    /// The gate's fallback: only ecosystems whose check has a meaningful exit
    /// code without extra arguments — a Makefile's targets are unknown, so no
    /// guess there.
    #[test]
    fn gate_fallback_maps_ecosystems() {
        assert_eq!(gate_fallback_commands(&["/r/Cargo.toml".into()]), ["cargo test"]);
        assert_eq!(
            gate_fallback_commands(&["/r/go.mod".into(), "/r/package.json".into()]),
            ["go test ./...", "npm test"]
        );
        assert!(gate_fallback_commands(&["/r/Makefile".into()]).is_empty(), "make targets unknown");
        assert_eq!(
            gate_fallback_commands(&["/r/pyproject.toml".into()]),
            ["pytest"]
        );
    }

    /// The reviewer's prompt carries the gate's report and forbids the
    /// reviewer from re-running checks — no double verification.
    #[test]
    fn review_prompt_carries_gate_and_forbids_rerun() {
        let p = review_prompt("h", Some("ПРОВЕРЕНО: $ cargo test"), Some("2 из 2 проверок прошло:\nok: cargo test"));
        assert!(p.contains("2 из 2 проверок прошло"), "gate report in the prompt");
        assert!(p.contains("НЕ гоняй"), "no re-run instruction");
        let p2 = review_prompt("h", None, None);
        assert!(p2.contains("не проводилась"), "absent gate stated honestly");
    }

    /// A short gate command runs and reports its exit; a failing one is a
    /// fail with output tail; a bogus one does not panic the gate.
    #[test]
    fn gate_run_one_reports_honestly() {
        let dir = std::env::temp_dir().join(format!("atlas-gate-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (ok, _) = gate_run_one(dir.to_string_lossy().as_ref(), "true");
        assert!(ok);
        let (ok, tail) = gate_run_one(dir.to_string_lossy().as_ref(), "echo boom >&2; exit 3");
        assert!(!ok);
        assert!(tail.contains("boom"), "{tail}");
        let (ok, tail) = gate_run_one(dir.to_string_lossy().as_ref(), "nonsuchcmd_atlas_gate");
        assert!(!ok);
        assert!(!tail.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The autopilot decision table: green closes, red retries until the
    /// limit then escalates, half-arrived signals wait.
    #[test]
    fn autopilot_decision_matrix() {
        use AutoAction::*;
        let on = true;
        let off = false;
        let acc = || Some(Some("accept".to_string()));
        let blk = || Some(Some("block".to_string()));
        let none = || Some(None);

        // gate not in yet — wait, whatever else says
        assert_eq!(autopilot_decision(None, on, acc(), 0), Wait);
        assert_eq!(autopilot_decision(None, off, None, 0), Wait);
        // reviewer still thinking — wait
        assert_eq!(autopilot_decision(Some(true), on, None, 0), Wait);

        // green: gate ok + reviewer accept, or no reviewer at all
        assert_eq!(autopilot_decision(Some(true), on, acc(), 0), Accept);
        assert_eq!(autopilot_decision(Some(true), off, None, 0), Accept);
        assert_eq!(autopilot_decision(Some(true), off, blk(), 5), Accept, "reviewer off — its verdict ignored");

        // red: gate fail or reviewer block → retry, then escalate
        assert_eq!(autopilot_decision(Some(false), off, None, 0), Retry);
        assert_eq!(autopilot_decision(Some(true), on, blk(), 1), Retry);
        assert_eq!(autopilot_decision(Some(false), on, blk(), 2), Escalate);
        assert_eq!(autopilot_decision(Some(true), on, none(), 2), Escalate, "no parseable verdict escalates");
        assert_eq!(autopilot_decision(Some(false), on, none(), 1), Retry);
    }

    /// The machine check behind «воркер ничего не сделал»: dirty tree counts,
    /// commits since the recorded start count, a clean tree with a known base
    /// counts as nothing; anything unverifiable (no repo, no base, git broken)
    /// defaults to "made changes" — only a provable no-op fails the worker.
    #[test]
    fn worker_made_changes_needs_provable_nothing() {
        let dir = std::env::temp_dir().join(format!("deleg-made-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap()
        };
        // not a repo yet → unverifiable → assume work
        assert!(worker_made_changes(&dir, None));
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@invalid"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "base\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "base"]);
        let start = String::from_utf8(run(&["rev-parse", "HEAD"]).stdout).unwrap();

        // clean tree, no commits since start → provable nothing
        assert!(!worker_made_changes(&dir, Some(start.trim())));

        // dirty tree counts...
        std::fs::write(dir.join("f.txt"), "base\nedit\n").unwrap();
        assert!(worker_made_changes(&dir, Some(start.trim())));
        std::fs::write(dir.join("f.txt"), "base\n").unwrap();

        // ...untracked files count...
        std::fs::write(dir.join("new.txt"), "hi\n").unwrap();
        assert!(worker_made_changes(&dir, Some(start.trim())));
        std::fs::remove_file(dir.join("new.txt")).unwrap();

        // ...and a commit since the start counts
        run(&["add", "-A"]);
        std::fs::write(dir.join("g.txt"), "g\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "worker"]);
        assert!(worker_made_changes(&dir, Some(start.trim())));

        // no recorded base → emptiness cannot be proven → assume work
        assert!(worker_made_changes(&dir, None));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `agent_end` fallback: the turn text for the contract parse may come
    /// from the frame's own message list when the host streams no deltas —
    /// the LAST assistant message wins, thinking/tool parts are skipped.
    #[test]
    fn last_assistant_text_reads_the_frame_messages() {
        let f = serde_json::json!({
            "type": "agent_end",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "бриф"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "думаю"},
                    {"type": "text", "text": "промежуточное"}
                ]},
                {"role": "toolResult", "content": [{"type": "text", "text": "вывод"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "финал"},
                    {"type": "text", "text": "СДЕЛАНО: всё\nВОПРОСЫ: нет"}
                ]}
            ]
        });
        assert_eq!(
            last_assistant_text(&f).as_deref(),
            Some("СДЕЛАНО: всё\nВОПРОСЫ: нет")
        );
        // no messages / no assistant text → None, mirror keeps whatever streamed
        assert_eq!(last_assistant_text(&serde_json::json!({"type": "agent_end"})), None);
        assert_eq!(
            last_assistant_text(&serde_json::json!({"messages": [
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "тихо"}]}
            ]})),
            None
        );
    }

    /// The state machine's exit step, driven directly (the waiter thread calls
    /// exactly this): exit 0 → `review`, any other code → `failed` with the
    /// code recorded, a notification each — and a `cancelled` run writes
    /// nothing, because the cancel path already settled it.
    #[test]
    fn process_exit_settles_review_or_failed() {
        crate::db::test_init_once();
        let id = {
            let conn = db::db_static().lock();
            conn.execute(
                "INSERT INTO items(title,quadrant,created_at,updated_at,delegation_state) \
                 VALUES('exit-тест','q1',1.0,1.0,'running')",
                [],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let state = || -> (String, Option<i64>, Option<f64>) {
            let conn = db::db_static().lock();
            conn.query_row(
                "SELECT delegation_state, delegation_exit, delegation_ended FROM items WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        };
        // The shared test DB is written by sibling tests in parallel — look for
        // THIS item's notification, not simply the newest row.
        let note = || -> (String, String) {
            let conn = db::db_static().lock();
            conn.query_row(
                "SELECT level, text FROM notifications \
                 WHERE text LIKE '%делегация дела ' || ?1 || ':%' ORDER BY id DESC LIMIT 1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };

        // exit 0 → review, ended timestamp set, info notification
        on_process_exit(id, Some("lamplife"), 0, &AtomicBool::new(false));
        let (s, exit, ended) = state();
        assert_eq!((s.as_str(), exit), ("review", Some(0)));
        assert!(ended.unwrap_or(0.0) > 0.0, "ended timestamp written");
        let (level, text) = note();
        assert_eq!(level, "info");
        assert!(text.contains("готово к ревью"), "{text}");

        // a non-zero exit → failed with the code, err notification
        {
            let conn = db::db_static().lock();
            conn.execute("UPDATE items SET delegation_state='running' WHERE id=?1", [id])
                .unwrap();
        }
        on_process_exit(id, Some("lamplife"), 3, &AtomicBool::new(false));
        let (s, exit, _) = state();
        assert_eq!((s.as_str(), exit), ("failed", Some(3)));
        let (level, text) = note();
        assert_eq!(level, "err");
        assert!(text.contains("exit 3"), "{text}");

        // a cancelled run leaves the state alone (cancel already wrote failed)
        {
            let conn = db::db_static().lock();
            conn.execute("UPDATE items SET delegation_state='running' WHERE id=?1", [id])
                .unwrap();
        }
        on_process_exit(id, Some("lamplife"), 0, &AtomicBool::new(true));
        assert_eq!(state().0, "running", "the cancel path owns this transition");
    }

    /// micelium's hand-computed case (dispatch.go `slugDir`: every `/` → `-`)
    /// and the on-disk refinement: a cwd UNDER the home keeps the leading
    /// separator but drops the home prefix — the real `~/.omp/agent/sessions`
    /// entries (`-Projects-atlas` for this repo, `-Work`, `-tmp`).
    #[test]
    fn dirslug_matches_micelium_and_the_on_disk_facts() {
        // micelium slugDir, plain replace: /tmp/x → -tmp-x
        assert_eq!(dirslug("/home/u", "/tmp/x"), "-tmp-x");
        assert_eq!(dirslug("/home/gwushor", "/tmp"), "-tmp");
        // home-nested cwds (disk-verified)
        assert_eq!(
            dirslug("/home/gwushor", "/home/gwushor/Projects/atlas"),
            "-Projects-atlas"
        );
        assert_eq!(dirslug("/home/gwushor", "/home/gwushor/Work"), "-Work");
        // the home boundary must not eat look-alike prefixes
        assert_eq!(dirslug("/home/u", "/home/u-evil/x"), "-home-u-evil-x");
    }

    /// The seeded fork: a full copy under a NEW omp-shaped id — the session
    /// identity rewritten everywhere, the original untouched, the record
    /// count identical.
    #[test]
    fn fork_copies_the_project_session_under_a_new_id() {
        let dir = std::env::temp_dir().join(format!("deleg-fork-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src_dir = dir.join("proj");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("2026-10-12T10-00-00.000Z_11111111-2222-3333-4444-555555555555.jsonl");
        std::fs::write(
            &src,
            concat!(
                "{\"type\":\"title\",\"v\":1,\"title\":\"дело\",\"source\":\"auto\"}\n",
                "{\"type\":\"session\",\"version\":3,\"id\":\"OLD\",\"timestamp\":\"t\",\"cwd\":\"/p\",\"title\":\"дело\"}\n",
                "{\"type\":\"user\",\"id\":\"a\",\"parentId\":null,\"sessionId\":\"OLD\",\"text\":\"раз\"}\n",
                "{\"type\":\"assistant\",\"id\":\"b\",\"parentId\":\"a\",\"sessionId\":\"OLD\",\"text\":\"два\"}\n",
                "{\"type\":\"assistant\",\"id\":\"c\",\"parentId\":\"b\",\"sessionId\":\"OLD\",\"text\":\"три\"}\n",
            ),
        )
        .unwrap();

        let head = parse_session_head(&src).expect("header with id");
        assert_eq!(head.id, "OLD");
        assert_eq!(head.cwd, "/p");
        let dest_dir = dir.join("run");
        std::fs::create_dir_all(&dest_dir).unwrap(); // production: start sites pre-create the dossier dir
        let copy = fork_session_file(&src, &head, &dest_dir).expect("fork");

        // omp-style name <stamp>_<id>.jsonl; the new id is 8-4-4-4-12 hex
        let name = copy.file_name().unwrap().to_str().unwrap().to_string();
        assert!(name.contains('_') && name.ends_with(".jsonl"), "{name}");
        let new_id = parse_session_head(&copy).expect("copy header").id;
        let parts: Vec<&str> = new_id.split('-').collect();
        assert_eq!(parts.len(), 5, "{new_id}");
        let lens = [8, 4, 4, 4, 12];
        for (p, l) in parts.iter().zip(lens) {
            assert_eq!(p.len(), l, "{new_id}");
            assert!(p.chars().all(|c| c.is_ascii_hexdigit()), "{new_id}");
        }

        let text = std::fs::read_to_string(&copy).unwrap();
        assert!(!text.contains("\"OLD\""), "old id must be gone: {text}");
        assert_eq!(
            text.matches(&format!("\"sessionId\":\"{new_id}\"")).count(),
            3,
            "every carried record rewritten: {text}"
        );
        assert_eq!(text.matches(&new_id).count(), 4, "{text}");
        assert!(text.contains("\"previousSessionFiles\""), "{text}");
        assert!(text.contains("\"title\":\"Fork: дело\""), "{text}");
        assert!(text.contains("\"text\":\"три\""), "bodies stay verbatim");
        // 2 fresh identity lines + 3 records; the original's own title/session
        // pair is replaced, not duplicated
        assert_eq!(text.lines().count(), 5);
        assert_eq!(text.matches("\"type\":\"session\"").count(), 1, "{text}");

        // the original is never touched
        let orig = std::fs::read_to_string(&src).unwrap();
        assert!(orig.contains("\"sessionId\":\"OLD\""));
        assert_eq!(orig.lines().count(), 5);
        // no temp leftovers
        assert_eq!(std::fs::read_dir(&dest_dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The brief's «Параллельная работа»: the two most recent FINISHED runs
    /// of OTHER items of the same project (the item's own row excluded, long
    /// итоги clipped), and no section without a project or without siblings.
    #[test]
    fn brief_carries_sibling_finals_for_the_same_project() {
        crate::db::test_init_once();
        let project = format!("sib-{}", std::process::id());
        {
            let conn = db::db_static().lock();
            let add = |title: &str, fin: &str, ended: f64| -> i64 {
                conn.execute(
                    "INSERT INTO items(title,quadrant,project,created_at,updated_at,\
                     delegation_state,delegation_final,delegation_ended) \
                     VALUES(?1,'q1',?2,1.0,1.0,'done',?3,?4)",
                    rusqlite::params![title, project, fin, ended],
                )
                .unwrap();
                conn.last_insert_rowid()
            };
            add("сиблинг А", "СДЕЛАНО: всё  хорошо\nИЗМЕНЕНО: x", 10.0);
            add("сиблинг Б", &"длинный ".repeat(200), 20.0);
            // this item's own row: newest of all, yet never its own sibling
            conn.execute(
                "INSERT INTO items(id,title,quadrant,project,created_at,updated_at,\
                 delegation_state,delegation_final,delegation_ended) \
                 VALUES(999000,'сам дело','q1',?1,1.0,1.0,'done','СВОЙ итог',30.0)",
                rusqlite::params![project],
            )
            .unwrap();
        }
        let mut it = item();
        it.id = 999_000;
        it.project = Some(project.clone());

        let brief = compile_brief(&it, Path::new("/p"));
        assert!(brief.contains("## Параллельная работа"), "{brief}");
        let section = &brief[brief.find("## Параллельная работа").unwrap()..];
        // newest first, the item's own final excluded → Б, А — never «сам»
        assert!(section.contains("- сиблинг Б: "), "{section}");
        assert!(section.contains("- сиблинг А: СДЕЛАНО: всё хорошо ИЗМЕНЕНО: x"), "{section}");
        assert!(!section.contains("сам дело"), "{section}");
        // exactly two entries
        assert_eq!(section.matches("\n- ").count(), 2, "{section}");
        // long итог clipped to 600 chars on one flattened line
        let (_, tail) = section.split_once("- сиблинг Б: ").unwrap();
        let line = tail.split('\n').next().unwrap();
        assert_eq!(line.chars().count(), 600);

        // no finished siblings → no section; no project → none either
        it.project = Some(format!("{project}-пусто"));
        assert!(!compile_brief(&it, Path::new("/p")).contains("## Параллельная работа"));
        it.project = None;
        assert!(!compile_brief(&it, Path::new("/p")).contains("## Параллельная работа"));
    }
}
