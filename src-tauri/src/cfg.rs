//! Process configuration, well-known paths, external binaries, logging.
use serde::Deserialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{LazyLock, OnceLock};

pub struct Cfg {
    pub port: u16,
    pub db_path: PathBuf,
    pub serve_only: bool,
}

/// Runtime CLI input — set once from main().
pub static CFG: OnceLock<Cfg> = OnceLock::new();

pub fn cfg() -> &'static Cfg {
    CFG.get().expect("cfg not initialised")
}

// ------------------------------------------------------- config.toml ----

/// Resolved application configuration: the well-known paths and external
/// program names an installation may need to move. Every field defaults to
/// the value hard-coded before the config existed; an optional human-editable
/// file ([`config_file`], README «Конфигурация») overrides individual fields.
/// Loaded once at startup — before `--db`/verb parsing, so every later reader
/// sees the same values; code that never runs the startup (unit tests) sees
/// the defaults.
///
/// Deliberately NOT configurable: files derived from other fields
/// (`control.db`, `atlas-orchestrator.jsonl`, `fastembed/`, `paste/` — all
/// follow `data_dir`) and the vault layout *inside* the vault root is three
/// plain subdir names rather than full paths.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Git repos root — the sync's `git` source, `POST /api/projects/create`.
    pub projects_dir: PathBuf,
    /// Obsidian vault root — the `vault` source; the three subdirs below
    /// carry its layout.
    pub vault_dir: PathBuf,
    pub vault_projects_subdir: String,
    pub vault_areas_subdir: String,
    pub vault_inbox_subdir: String,
    /// Everything the app persists: control.db, orchestrator/delegation
    /// session files, fastembed cache, clipboard paste.
    pub data_dir: PathBuf,
    /// omp session logs (the `omp` source).
    pub sessions_dir: PathBuf,
    /// mnemopi sqlite (the `memory` source).
    pub memory_db: PathBuf,
    /// Root of the delegation worktrees (`delegate.rs::worktree_dir`).
    pub worktree_root: PathBuf,
    /// Model pinned for every delegation worker (`[delegation] model`).
    /// Empty → the flag is omitted and omp's default applies — the config-dev
    /// instance and fresh installs keep today's behavior.
    pub delegation_model: String,
    /// Model of the fresh-context reviewer (`[delegation] reviewer`): after a
    /// run settles in `review`, a SECOND headless `omp -p` session (cold
    /// context, never a continuation of the worker's) re-reads the diff and
    /// answers VERDICT: accept|block into `delegation_review`. Empty (the
    /// default) = the reviewer is off — the owner's judgment stays the only
    /// gate; a configured model adds a second opinion without a veto.
    pub delegation_reviewer: String,
    /// The delegation autopilot (`[delegation] autopilot`, default off): when
    /// a run reaches `review`, the machine decides — gate ok + reviewer
    /// accept (or no reviewer) → the item closes itself; gate fail / reviewer
    /// block → the run is auto-rejected with the machine report as feedback
    /// and re-delegated, at most DELEGATION_RETRIES times, then escalated to
    /// the owner. A manual reject zeroes the attempts («рули я»).
    pub delegation_autopilot: bool,
    /// Silence WARN threshold of a delegation run (`[delegation]
    /// silence_warn_secs`): the run poller notifies the owner ONCE when the
    /// rpc child produced no frames this long — «возможно, долгий thinking».
    /// NO timer ever kills a working child: a long but useful operation (a
    /// long think, a big generation) must not die from silence; the owner
    /// watches the live журнал and cancels by hand. `0` = no warn at all.
    pub delegation_silence_warn_secs: u64,
    /// Where the `omp -p` delegation worker is spawned (`[delegation]
    /// run_in`): `"systemd"` (the default) keeps the transient-unit spawn —
    /// prod behavior byte-identical; `"herdr"` runs the worker inside a
    /// dedicated herdr workspace/pane (`atlas-deleg-<slug>`, same slug as
    /// the unit name) so the owner watches the raw output live in a
    /// terminal. The orchestrator rpc child always stays on systemd/direct —
    /// its rpc stdio is spoken by atlas, a pane tty cannot carry it.
    pub delegation_run_in: String,
    /// Herdr toasts for delegation transitions (`[delegation]
    /// herdr_toasts`, default off): when a run settles (review /
    /// needs-feedback / failed), atlas fires `herdr notification show` —
    /// one toast per transition, best-effort, never blocking the settle.
    pub delegation_herdr_toasts: bool,
    /// omarchy theme colors the panel palette follows.
    pub theme_file: PathBuf,
    /// zen-tabs script behind the `zen` source.
    pub zen_script: PathBuf,
    /// The user's voxtype config, rewritten per run with `language = "ru"`.
    pub voxtype_config: PathBuf,
    /// External programs. A bare name resolves on PATH; an absolute path is
    /// used verbatim (no existence probe — a wrong value fails at spawn,
    /// loudly). Defaults keep the historical `~/.local/bin/<name>`-first probe.
    pub omp_bin: String,
    pub herdr_bin: String,
    pub bun_bin: String,
    /// Desktop launcher for «open file», invoked as `<opener> open <path>`;
    /// missing → `xdg-open` fallback (http.rs `opener_argv`).
    pub opener_bin: String,
    pub voxtype_bin: String,
    pub pw_record_bin: String,
    /// Terminal emulator behind «открыть в wezterm»
    /// (`POST /api/terminal/{id}/open`): a tab in a running GUI via
    /// `cli spawn`, else a new window via `start --always-new-process`.
    pub term_bin: String,
}

impl Config {
    /// Today's hard-coded values derived from `$HOME` — the baseline every
    /// config file field overrides from.
    fn from_home() -> Config {
        let home = home();
        let local_bin = |name: &str| home.join(".local/bin").join(name);
        Config {
            projects_dir: home.join("Projects"),
            vault_dir: home.join("vault"),
            vault_projects_subdir: "Проекты".into(),
            vault_areas_subdir: "Области".into(),
            vault_inbox_subdir: "Инбокс".into(),
            data_dir: home.join(".local/share/atlas"),
            sessions_dir: home.join(".omp/agent/sessions"),
            memory_db: home.join(".omp/agent/memories/mnemopi/mnemopi.db"),
            worktree_root: home.join(".atlas-wt"),
            delegation_model: String::new(),
            delegation_reviewer: String::new(),
            delegation_autopilot: false,
            delegation_silence_warn_secs: 300,
            delegation_run_in: "systemd".into(),
            delegation_herdr_toasts: false,
            theme_file: home.join(".local/state/omarchy/current/theme/colors.toml"),
            zen_script: home.join("Projects/ai/zen-tabs/bin/spaces.mjs"),
            voxtype_config: home.join(".config/voxtype/config.toml"),
            omp_bin: first_existing(&[&local_bin("omp").to_string_lossy(), "omp"]),
            herdr_bin: first_existing(&[&local_bin("herdr").to_string_lossy(), "herdr"]),
            bun_bin: first_existing(&[
                &home
                    .join(".local/share/mise/installs/bun/1.4.2/bin/bun")
                    .to_string_lossy(),
                "bun",
            ]),
            opener_bin: "gio".into(),
            voxtype_bin: "voxtype".into(),
            pw_record_bin: "pw-record".into(),
            term_bin: "wezterm".into(),
        }
    }
}

/// `[paths]` of config.toml. Every field optional; a missing field keeps the
/// default. Unknown keys are errors on purpose — a typo must not be silently
/// ignored (the caller then runs on the defaults and says so in the log).
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawPaths {
    projects: Option<String>,
    vault: Option<String>,
    vault_projects_subdir: Option<String>,
    vault_areas_subdir: Option<String>,
    vault_inbox_subdir: Option<String>,
    data: Option<String>,
    omp_sessions: Option<String>,
    memory_db: Option<String>,
    worktrees: Option<String>,
    theme: Option<String>,
    zen_script: Option<String>,
    voxtype_config: Option<String>,
}

/// `[binaries]` of config.toml.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawBinaries {
    omp: Option<String>,
    herdr: Option<String>,
    bun: Option<String>,
    opener: Option<String>,
    voxtype: Option<String>,
    pw_record: Option<String>,
    term: Option<String>,
}

/// `[delegation]` of config.toml.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawDelegation {
    model: Option<String>,
    silence_warn_secs: Option<u64>,
    reviewer: Option<String>,
    autopilot: Option<bool>,
    run_in: Option<String>,
    herdr_toasts: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    paths: RawPaths,
    binaries: RawBinaries,
    delegation: RawDelegation,
}

/// A configured path may start with `~/` — expanded against `$HOME` so the
/// README sample stays portable across users.
fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        home()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(s)
    }
}

/// Parse a config.toml body over the defaults. `Err` names the problem (bad
/// TOML or an unknown key); the caller warns and runs on the plain defaults —
/// a broken file must never keep the app from starting.
fn config_from_toml(text: &str) -> Result<Config, String> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut c = Config::from_home();
    let p = raw.paths;
    for (v, slot) in [
        (p.projects, &mut c.projects_dir),
        (p.vault, &mut c.vault_dir),
        (p.data, &mut c.data_dir),
        (p.omp_sessions, &mut c.sessions_dir),
        (p.memory_db, &mut c.memory_db),
        (p.worktrees, &mut c.worktree_root),
        (p.theme, &mut c.theme_file),
        (p.zen_script, &mut c.zen_script),
        (p.voxtype_config, &mut c.voxtype_config),
    ] {
        if let Some(v) = v {
            *slot = expand_tilde(&v);
        }
    }
    for (v, slot) in [
        (p.vault_projects_subdir, &mut c.vault_projects_subdir),
        (p.vault_areas_subdir, &mut c.vault_areas_subdir),
        (p.vault_inbox_subdir, &mut c.vault_inbox_subdir),
    ] {
        if let Some(v) = v {
            *slot = v;
        }
    }
    let b = raw.binaries;
    for (v, slot) in [
        (b.omp, &mut c.omp_bin),
        (b.herdr, &mut c.herdr_bin),
        (b.bun, &mut c.bun_bin),
        (b.opener, &mut c.opener_bin),
        (b.voxtype, &mut c.voxtype_bin),
        (b.pw_record, &mut c.pw_record_bin),
        (b.term, &mut c.term_bin),
    ] {
        if let Some(v) = v {
            *slot = v;
        }
    }
    if let Some(m) = raw.delegation.model {
        c.delegation_model = m;
    }
    if let Some(r) = raw.delegation.reviewer {
        c.delegation_reviewer = r;
    }
    if let Some(a) = raw.delegation.autopilot {
        c.delegation_autopilot = a;
    }
    if let Some(s) = raw.delegation.silence_warn_secs {
        c.delegation_silence_warn_secs = s;
    }
    if let Some(r) = raw.delegation.run_in {
        c.delegation_run_in = r;
    }
    if let Some(t) = raw.delegation.herdr_toasts {
        c.delegation_herdr_toasts = t;
    }
    Ok(c)
}

/// `~/.config/atlas/config.toml`, honouring an absolute `$XDG_CONFIG_HOME`
/// per the XDG base-dir spec.
pub fn config_file() -> PathBuf {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() && std::path::Path::new(&x).is_absolute() => PathBuf::from(x),
        _ => home().join(".config"),
    };
    base.join("atlas").join("config.toml")
}

/// Read one config file: absent/unreadable → defaults; unparsable → defaults
/// (the caller warns). Pure, so tests can point it anywhere.
fn load_config_at(path: &std::path::Path) -> Config {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| config_from_toml(&t).ok())
        .unwrap_or_else(Config::from_home)
}

/// The resolved configuration: the startup-loaded file values, or the
/// `$HOME`-derived defaults for anything running before/outside the server
/// startup (unit tests).
pub fn config() -> &'static Config {
    FILE_CONFIG.get().unwrap_or(&DEFAULT_CONFIG)
}

static FILE_CONFIG: OnceLock<Config> = OnceLock::new();
static DEFAULT_CONFIG: LazyLock<Config> = LazyLock::new(Config::from_home);

/// What [`init_config`] wants logged, replayed by [`log_config_note`] once
/// the logger exists — init must run earlier, because the `--db` default and
/// the CLI verbs already read the resolved values. `(true, …)` = the file was
/// there but broken.
static CONFIG_NOTE: OnceLock<Option<(bool, String)>> = OnceLock::new();

/// Startup: read the optional config file (absent → defaults; broken →
/// warn + defaults) and fix the process-wide values. `override_path`
/// (`--config`, e.g. the dev instance's `config-dev.toml`) replaces the
/// well-known `config_file()`; `None` keeps the default behaviour.
pub fn init_config(override_path: Option<&std::path::Path>) {
    let path = override_path
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(config_file);
    let note = match std::fs::read_to_string(&path) {
        Err(_) => None,
        Ok(text) => match config_from_toml(&text) {
            Ok(_) => Some((false, format!("config: {}", path.display()))),
            Err(e) => Some((
                true,
                format!("config {}: {e} — работаем на значениях по умолчанию", path.display()),
            )),
        },
    };
    let _ = FILE_CONFIG.set(load_config_at(&path));
    let _ = CONFIG_NOTE.set(note);
}

/// Replay the config note now that the logger is up.
pub fn log_config_note() {
    match CONFIG_NOTE.get().and_then(|n| n.as_ref()) {
        Some((true, m)) => log::warn!("{m}"),
        Some((false, m)) => log::info!("{m}"),
        None => {}
    }
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string()))
}

/// Repository root (the Python `ATLAS_DIR`): parent of src-tauri.
pub fn atlas_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(home)
}

pub fn data_dir() -> PathBuf {
    config().data_dir.clone()
}

pub fn default_db_path() -> PathBuf {
    config().data_dir.join("control.db")
}

pub fn vault_dir() -> PathBuf {
    config().vault_dir.clone()
}

/// Vault layout: `<vault>/<subdir>` for the three configured subdirs.
pub fn vault_projects_dir() -> PathBuf {
    vault_dir().join(&config().vault_projects_subdir)
}

pub fn vault_areas_dir() -> PathBuf {
    vault_dir().join(&config().vault_areas_subdir)
}

pub fn vault_inbox_dir() -> PathBuf {
    vault_dir().join(&config().vault_inbox_subdir)
}

pub fn sessions_dir() -> PathBuf {
    config().sessions_dir.clone()
}

pub fn projects_dir() -> PathBuf {
    config().projects_dir.clone()
}

pub fn memory_db() -> PathBuf {
    config().memory_db.clone()
}

pub fn theme_file() -> PathBuf {
    config().theme_file.clone()
}

pub fn zen_script() -> PathBuf {
    config().zen_script.clone()
}

/// Root of the delegation worktrees (default `~/.atlas-wt`).
pub fn worktree_root() -> PathBuf {
    config().worktree_root.clone()
}

/// STT: program names for the recorder and the transcriber, plus the user
/// voxtype config rewritten per run with `language = "ru"`.
pub fn pw_record_bin() -> &'static str {
    &config().pw_record_bin
}

pub fn voxtype_bin() -> &'static str {
    &config().voxtype_bin
}

pub fn voxtype_config() -> PathBuf {
    config().voxtype_config.clone()
}

/// Desktop launcher for «open file» (http.rs `opener_argv`).
pub fn opener_bin() -> &'static str {
    &config().opener_bin
}

pub fn log_file() -> PathBuf {
    atlas_dir().join("atlas-rs.log")
}

// ------------------------------------------------- orchestrator prompt ----

/// Fallback one-liner appended to omp's system prompt when `orchestrator.md` is
/// missing or unreadable — the orchestrator must never fail to spawn because of
/// an absent file.
pub const ORCH_FALLBACK_PROMPT: &str = "You are the atlas orchestrator: you coordinate the user's projects via omp session cast. Reply concisely.";

/// The standing instructions file, `<atlas_dir>/orchestrator.md`, when it exists
/// and is readable. Both launch sites (the interactive `orch` PTY in `term.rs`
/// and the rpc channel in `orch.rs`) append it to omp's system prompt.
pub fn orchestrator_prompt() -> Option<PathBuf> {
    orchestrator_prompt_in(&atlas_dir())
}

fn orchestrator_prompt_in(dir: &std::path::Path) -> Option<PathBuf> {
    let p = dir.join("orchestrator.md");
    (p.is_file() && std::fs::File::open(&p).is_ok()).then_some(p)
}

fn orch_prompt_args_from(prompt: Option<PathBuf>) -> Vec<String> {
    match prompt {
        Some(p) => vec![
            "--append-system-prompt".to_string(),
            p.to_string_lossy().into_owned(),
        ],
        None => {
            log::warn!(
                "orchestrator.md is not readable at {} — using the built-in fallback prompt",
                atlas_dir().join("orchestrator.md").display()
            );
            vec![
                "--append-system-prompt".to_string(),
                ORCH_FALLBACK_PROMPT.to_string(),
            ]
        }
    }
}

/// `--append-system-prompt <orchestrator.md | fallback>` for an omp spawn. A
/// missing file degrades to the fallback one-liner; it never blocks the spawn.
pub fn orch_prompt_args() -> Vec<String> {
    orch_prompt_args_from(orchestrator_prompt())
}

// ------------------------------------------------- orchestrator policy ----

/// The orchestrator's approval-policy overlay, `<atlas_dir>/orchestrator-approval.yml`.
/// Versioned in the repo so the policy is reviewable and survives a reinstall;
/// it carries `tools.approvalMode: yolo` (harmless tools run free) plus the
/// curated `bash.patterns` list that still *asks* for the destructive and
/// outward-facing command classes (see the file's own header for the why).
pub const ORCH_OVERLAY_NAME: &str = "orchestrator-approval.yml";

pub fn orch_overlay_file() -> PathBuf {
    atlas_dir().join(ORCH_OVERLAY_NAME)
}

fn orch_policy_args_in(overlay: &std::path::Path) -> Vec<String> {
    if overlay.is_file() {
        vec![
            "--config".to_string(),
            overlay.to_string_lossy().into_owned(),
        ]
    } else {
        // `omp --config <missing>` is a HARD error (no silent fallback), and
        // the orchestrator must never brick on an absent file. Without the
        // overlay the child simply runs on the global/project settings: the
        // mode falls back to their `tools.approvalMode` (schema default
        // `yolo`) and the global deny list still applies — the only thing
        // lost is this file's curated prompt list.
        log::warn!(
            "orchestrator policy overlay is not readable at {} — running the rpc child without --config",
            overlay.display()
        );
        Vec::new()
    }
}

/// `--config <orchestrator-approval.yml>` for an omp spawn, when the overlay
/// exists. The path is absolute because a relative `--config` is resolved
/// against the child's effective working directory — measured 09.10.2026: with
/// `--cwd /tmp/x`, a relative `./orchestrator-approval.yml` was looked for under
/// `/tmp/x`, and a missing overlay is a HARD error (the orchestrator would not
/// start). Same reasoning as the persona file above.
pub fn orch_policy_args() -> Vec<String> {
    orch_policy_args_in(&orch_overlay_file())
}

fn first_existing(candidates: &[&str]) -> String {
    for c in candidates {
        if !c.is_empty() && std::path::Path::new(c).exists() {
            return c.to_string();
        }
    }
    candidates[0].to_string()
}

/// External programs, config-backed (defaults keep the
/// `~/.local/bin/<name>`-first probe; see `Config`).
pub static HERDR_BIN: LazyLock<String> = LazyLock::new(|| config().herdr_bin.clone());

pub static OMP_BIN: LazyLock<String> = LazyLock::new(|| config().omp_bin.clone());

pub static BUN_BIN: LazyLock<String> = LazyLock::new(|| config().bun_bin.clone());

/// Terminal emulator for «открыть в wezterm» (term.rs / web/terminal.rs).
pub static TERM_BIN: LazyLock<String> = LazyLock::new(|| config().term_bin.clone());

// ------------------------------------------------------------------ log ----

struct Logger {
    file: std::sync::Mutex<Option<std::fs::File>>,
}

/// Runtime input (`--serve-only` decides whether the file sink exists).
static LOGGER: OnceLock<Logger> = OnceLock::new();

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Debug
    }
    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {:<5} {}: {}\n",
            util::now_iso_utc(),
            record.level(),
            record.target(),
            record.args()
        );
        let _ = std::io::stderr().write_all(line.as_bytes());
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }
    fn flush(&self) {
        let _ = std::io::stderr().flush();
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.flush();
            }
        }
    }
}

/// Stderr always; additionally `atlas-rs.log` when `--serve-only`.
pub fn init_logging(serve_only: bool) {
    let file = if serve_only {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file())
            .ok()
    } else {
        None
    };
    let _ = log::set_logger(LOGGER.get_or_init(|| Logger {
        file: std::sync::Mutex::new(file),
    }));
    log::set_max_level(log::LevelFilter::Debug);
}

pub mod util {
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    fn civil_from_days(z: i64) -> (i64, u32, u32) {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64; // [0, 146096]
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
        let mp = (5 * doy + 2) / 153; // [0, 11]
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    pub fn now_iso_utc() -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs() as i64;
        let millis = now.subsec_millis();
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            y,
            m,
            d,
            sod / 3600,
            (sod % 3600) / 60,
            sod % 60,
            millis
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atlas-cfg-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    #[test]
    fn orch_prompt_args_use_the_file_when_it_exists() {
        let dir = tmp_dir("exists");
        let file = dir.join("orchestrator.md");
        std::fs::write(&file, "# orchestrator\n").expect("write prompt");

        let argv = orch_prompt_args_from(orchestrator_prompt_in(&dir));
        assert_eq!(argv[0], "--append-system-prompt");
        assert_eq!(argv.len(), 2);
        assert_eq!(argv[1], file.to_string_lossy());
        assert!(!argv.iter().any(|a| a == ORCH_FALLBACK_PROMPT));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn orch_prompt_args_fall_back_when_missing() {
        let dir = tmp_dir("missing");
        assert_eq!(orchestrator_prompt_in(&dir), None, "no file in a fresh dir");
        let argv = orch_prompt_args_from(orchestrator_prompt_in(&dir));
        assert_eq!(
            argv,
            vec![
                "--append-system-prompt".to_string(),
                ORCH_FALLBACK_PROMPT.to_string(),
            ]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_repo_ships_orchestrator_md() {
        let p = orchestrator_prompt().expect("orchestrator.md must ship in the repo root");
        assert_eq!(p, atlas_dir().join("orchestrator.md"));
        assert!(
            orch_prompt_args().iter().any(|a| a == &p.to_string_lossy()),
            "the real repo prompt path must reach the omp argv"
        );
    }

    #[test]
    fn the_repo_ships_the_approval_overlay() {
        let p = orch_overlay_file();
        assert_eq!(p, atlas_dir().join(ORCH_OVERLAY_NAME));
        assert!(p.is_file(), "overlay must ship in the repo root: {}", p.display());
        // absolute: `--config` resolves relative paths against the process cwd,
        // which is not the repo when the server is started from elsewhere
        assert!(p.is_absolute(), "overlay path must be absolute: {}", p.display());
        let argv = orch_policy_args();
        assert_eq!(
            argv,
            vec![
                "--config".to_string(),
                p.to_string_lossy().into_owned(),
            ],
            "the overlay reaches the omp argv as --config <path>"
        );
    }

    #[test]
    fn a_missing_overlay_degrades_to_no_flag() {
        let dir = tmp_dir("no-overlay");
        // never `--config <missing>`: omp treats that as a hard error, and the
        // orchestrator must still start (global settings then govern)
        assert!(orch_policy_args_in(&dir.join("nope.yml")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_absent_file_means_defaults() {
        let p = tmp_dir("cfg-absent").join("config.toml");
        assert_eq!(load_config_at(&p), Config::from_home());
        // and the global falls back to the same defaults before init_config
        assert_eq!(config().data_dir, Config::from_home().data_dir);
    }

    /// `[delegation] model` pins the delegation workers' model; a config
    /// without the section (every existing install, incl. config-dev) keeps
    /// the empty default — the flag is omitted and omp's default applies.
    /// Unknown keys inside the section stay a parse error, like everywhere.
    #[test]
    fn delegation_model_key_parses_and_defaults_to_empty() {
        let dir = tmp_dir("cfg-deleg");
        let p = dir.join("config.toml");
        std::fs::write(&p, "[delegation]\nmodel = \"zai/glm-5.3-flash\"\n").expect("write config");
        let c = load_config_at(&p);
        assert_eq!(c.delegation_model, "zai/glm-5.3-flash");

        std::fs::write(&p, "[paths]\ndata = \"/d\"\n").expect("write config");
        let c = load_config_at(&p);
        assert_eq!(c.delegation_model, "", "no [delegation] → empty default");

        std::fs::write(&p, "[delegation]\nmodel = \"m\"\ntypo = 1\n").expect("write config");
        assert!(
            config_from_toml(&std::fs::read_to_string(&p).unwrap()).is_err(),
            "unknown keys inside [delegation] must not be silently ignored"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `[delegation] silence_warn_secs` frames the run poller's warn:
    /// default 300 applies without the key, an explicit value wins, `0` is
    /// legal and means «never warn». NO kill lives behind this key — timers
    /// never stop a delegation (the owner cancels by hand).
    #[test]
    fn delegation_silence_warn_key_parses_and_zero_is_legal() {
        let d = Config::from_home();
        assert_eq!(d.delegation_silence_warn_secs, 300, "default warn threshold");

        let dir = tmp_dir("cfg-deleg-warn");
        let p = dir.join("config.toml");
        std::fs::write(&p, "[delegation]\nsilence_warn_secs = 90\n").expect("write config");
        let c = load_config_at(&p);
        assert_eq!(c.delegation_silence_warn_secs, 90);

        std::fs::write(&p, "[delegation]\nsilence_warn_secs = 0\n").expect("write config");
        let c = load_config_at(&p);
        assert_eq!(c.delegation_silence_warn_secs, 0, "0 = never warn, parser keeps it");

        // a config without the section keeps the default (config-dev shape)
        std::fs::write(&p, "[paths]\ndata = \"/d\"\n").expect("write config");
        let c = load_config_at(&p);
        assert_eq!(c.delegation_silence_warn_secs, 300);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `[delegation] reviewer` names the fresh-context reviewer's model; empty
    /// (the default, and a section-less config) = the reviewer is off —
    /// today's behavior; a non-empty value reaches `delegation_reviewer`.
    #[test]
    fn delegation_reviewer_key_parses_and_defaults_to_off() {
        let d = Config::from_home();
        assert_eq!(d.delegation_reviewer, "", "default: reviewer off");

        let dir = tmp_dir("cfg-deleg-reviewer");
        let p = dir.join("config.toml");
        std::fs::write(&p, "[delegation]\nreviewer = \"zai/glm-5.3-flash\"\n").expect("write config");
        let c = load_config_at(&p);
        assert_eq!(c.delegation_reviewer, "zai/glm-5.3-flash");

        std::fs::write(&p, "[paths]\ndata = \"/d\"\n").expect("write config");
        let c = load_config_at(&p);
        assert_eq!(c.delegation_reviewer, "", "no [delegation] → reviewer off");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `[delegation] autopilot` flips the machine-driven cycle; the default
    /// (and a section-less config) keeps it off — no automation unless asked.
    #[test]
    fn delegation_autopilot_key_parses_and_defaults_off() {
        assert!(!Config::from_home().delegation_autopilot, "default: autopilot off");

        let dir = tmp_dir("cfg-deleg-auto");
        let p = dir.join("config.toml");
        std::fs::write(&p, "[delegation]\nautopilot = true\n").expect("write config");
        assert!(load_config_at(&p).delegation_autopilot, "explicit true wins");

        std::fs::write(&p, "[paths]\ndata = \"/d\"\n").expect("write config");
        assert!(!load_config_at(&p).delegation_autopilot, "no section → off");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `[delegation] run_in` picks the worker spawn surface; the default
    /// (and a section-less config) stays on the transient systemd unit —
    /// prod behavior byte-identical until the config says otherwise.
    #[test]
    fn delegation_run_in_key_parses_and_defaults_to_systemd() {
        assert_eq!(Config::from_home().delegation_run_in, "systemd");

        let dir = tmp_dir("cfg-deleg-runin");
        let p = dir.join("config.toml");
        std::fs::write(&p, "[delegation]\nrun_in = \"herdr\"\n").expect("write config");
        assert_eq!(load_config_at(&p).delegation_run_in, "herdr");

        std::fs::write(&p, "[paths]\ndata = \"/d\"\n").expect("write config");
        assert_eq!(
            load_config_at(&p).delegation_run_in,
            "systemd",
            "no [delegation] → systemd"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `[delegation] herdr_toasts` enables the herdr toast on delegation
    /// transitions; the default (and a section-less config) stays off —
    /// prod has no toasts until the config says so.
    #[test]
    fn delegation_herdr_toasts_key_parses_and_defaults_off() {
        assert!(!Config::from_home().delegation_herdr_toasts, "default: off");

        let dir = tmp_dir("cfg-deleg-toasts");
        let p = dir.join("config.toml");
        std::fs::write(&p, "[delegation]\nherdr_toasts = true\n").expect("write config");
        assert!(load_config_at(&p).delegation_herdr_toasts, "explicit true wins");

        std::fs::write(&p, "[paths]\ndata = \"/d\"\n").expect("write config");
        assert!(
            !load_config_at(&p).delegation_herdr_toasts,
            "no [delegation] → off"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_one_override_applies_the_rest_stay_default() {
        let dir = tmp_dir("cfg-override");
        let p = dir.join("config.toml");
        std::fs::write(
            &p,
            "[paths]\nworktrees = \"/wt\"\n[binaries]\nomp = \"/opt/omp/bin/omp\"\nterm = \"/usr/bin/wezterm\"\n",
        )
        .expect("write config");
        let c = load_config_at(&p);
        let d = Config::from_home();
        assert_eq!(c.worktree_root, PathBuf::from("/wt"));
        assert_eq!(c.omp_bin, "/opt/omp/bin/omp");
        assert_eq!(c.term_bin, "/usr/bin/wezterm");
        assert_eq!(d.term_bin, "wezterm", "the default terminal is wezterm");
        assert_eq!(c.projects_dir, d.projects_dir);
        assert_eq!(c.vault_dir, d.vault_dir);
        assert_eq!(c.data_dir, d.data_dir);
        assert_eq!(c.vault_projects_subdir, d.vault_projects_subdir);
        assert_eq!(c.herdr_bin, d.herdr_bin);
        assert_eq!(c.opener_bin, d.opener_bin);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_dev_file_is_data_override_only() {
        // the shape of ~/.config/atlas/config-dev.toml: a single `[paths] data`
        // override — everything else must stay at the `$HOME` default, so the
        // dev instance differs from the live one ONLY in where it writes
        let dir = tmp_dir("cfg-dev");
        let p = dir.join("config-dev.toml");
        std::fs::write(&p, "[paths]\ndata = \"~/.local/share/atlas-dev\"\n").expect("write config");
        let c = load_config_at(&p);
        let d = Config::from_home();
        assert_eq!(c.data_dir, home().join(".local/share/atlas-dev"));
        assert_eq!(c.vault_dir, d.vault_dir);
        assert_eq!(c.projects_dir, d.projects_dir);
        assert_eq!(c.worktree_root, d.worktree_root);
        assert_eq!(c.omp_bin, d.omp_bin);
        assert_eq!(c.term_bin, d.term_bin);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_tilde_expands_against_home() {
        let dir = tmp_dir("cfg-tilde");
        let p = dir.join("config.toml");
        std::fs::write(&p, "[paths]\nprojects = \"~/repo\"\n").expect("write config");
        let c = load_config_at(&p);
        assert_eq!(c.projects_dir, home().join("repo"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_broken_file_is_rejected_not_silently_half_applied() {
        // an unknown key and malformed TOML are both hard parse errors; the
        // startup path then warns and runs on the defaults
        assert!(config_from_toml("[paths]\nnope = \"x\"\n").is_err());
        assert!(config_from_toml("[binaries\n").is_err());
        assert!(config_from_toml("[binaries]\nvoxtype = 7\n").is_err());
    }
}
