//! Make task discovery (`GET /api/project/{name}/tasks`) and the per-project
//! `make <target>` runner.
//!
//! Runner state is in-process (`Mutex<HashMap>`): a running task dies with the
//! server process. That is accepted — the UI reads status from the same
//! process, so a restart simply shows "no run" again.

use crate::db;
use crate::runner::{self, Run};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

/// GNU make's own search order.
const MAKEFILES: [&str; 3] = ["Makefile", "makefile", "GNUmakefile"];

/// First existing makefile in the project directory.
pub fn find_makefile(dir: &Path) -> Option<PathBuf> {
    MAKEFILES.iter().map(|n| dir.join(n)).find(|p| p.is_file())
}

#[derive(Debug, PartialEq, Clone)]
pub struct Task {
    pub name: String,
    pub doc: Option<String>,
}

/// Target names on one rule line: one or more `name` tokens followed by `:`,
/// where the colon is neither `:=` nor `::=` (assignments never match), the
/// name starts with an alphanumeric (so `.PHONY` and friends never match) and
/// the remaining chars are `[A-Za-z0-9_.%/-]`. `a b: dep` yields `[a, b]`.
fn target_names(line: &str) -> Option<Vec<String>> {
    let bytes = line.as_bytes();
    let is_name_byte =
        |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'%' | b'/' | b'-');
    let mut names: Vec<String> = Vec::new();
    let mut i = 0;
    loop {
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && is_name_byte(bytes[i]) {
            i += 1;
        }
        if i == start || !bytes[start].is_ascii_alphanumeric() {
            return None;
        }
        names.push(line[start..i].to_string());
        let mut j = i;
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b':' {
            // `:=` and `::=` are assignments, not rules
            if matches!(bytes.get(j + 1), Some(b'=') | Some(b':')) {
                return None;
            }
            return Some(names);
        }
        if j == i {
            return None; // tokens ran into each other without a space
        }
        i = j;
    }
}

/// `target: ## текст` on the rule line itself — the self-documenting-makefile
/// idiom the user's real lamplife makefile uses. Only text after the colon
/// counts: a `#` inside the prerequisite list is an ordinary comment.
fn inline_doc(line: &str) -> Option<String> {
    let hash = line.find("##")?;
    let colon = line.find(':')?;
    if hash < colon {
        return None;
    }
    let text = line[hash + 2..].trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Parse a makefile into its named tasks. `## text` immediately above a rule
/// becomes that rule's `doc` (any other line in between drops the pending
/// doc); a missing pending doc falls back to a `## text` tail on the rule line
/// itself. Backslash continuations join into one logical line, so a target
/// list spread over several lines (`build test \` / `lint: dep`) still yields
/// every target. CRLF input is tolerated (the user's CBQN makefile is CRLF).
pub fn parse_makefile(text: &str) -> Vec<Task> {
    let mut tasks: Vec<Task> = Vec::new();
    let mut doc: Option<String> = None;
    // the logical line being built from `\` continuations, if any
    let mut pending: Option<String> = None;
    for raw in text.lines() {
        let raw = raw.trim_end_matches('\r');
        // a recipe line (leading tab) is never a rule and never continues a
        // logical line that is not already open
        if pending.is_none() && raw.starts_with('\t') {
            continue;
        }
        let piece = if pending.is_some() { raw.trim() } else { raw };
        let logical = match pending.take() {
            Some(prev) => format!("{prev} {piece}"),
            None => piece.to_string(),
        };
        if let Some(cut) = logical.strip_suffix('\\') {
            pending = Some(cut.trim_end().to_string());
            continue;
        }
        let line = logical.trim_end();
        if line.is_empty() {
            doc = None;
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("##") {
            doc = Some(rest.trim().to_string());
            continue;
        }
        if trimmed.starts_with('#') {
            doc = None; // ordinary comment breaks "immediately above"
            continue;
        }
        match target_names(line) {
            Some(names) => {
                let doc = doc.take().or_else(|| inline_doc(line));
                for n in names {
                    tasks.push(Task {
                        name: n,
                        doc: doc.clone(),
                    });
                }
            }
            None => doc = None,
        }
    }
    tasks
}

// -------------------------------------------------------------- runner -----

pub enum StartError {
    /// A task for this project is still running (409).
    Active,
    /// `make` could not be spawned (e.g. not on PATH).
    Spawn(String),
}

/// The newest run per project name (in-process — see the module docs).
static RUNS: LazyLock<Mutex<HashMap<String, Arc<Run>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn runs() -> &'static Mutex<HashMap<String, Arc<Run>>> {
    &RUNS
}

/// `make <target>` in `dir`, argv array (no shell), stdout+stderr merged into
/// the run's ring. The exit notification goes through the notifications table.
pub fn start(project: &str, dir: &Path, target: &str) -> Result<i64, StartError> {
    let mut guard = runner::lock(runs());
    if let Some(r) = guard.get(project) {
        if r.active() {
            return Err(StartError::Active);
        }
    }
    let argv = vec!["make".to_string(), target.to_string()];
    let (project_owned, target_owned) = (project.to_string(), target.to_string());
    let run = runner::spawn(&argv, dir, move |code| {
        notify_exit(&project_owned, &target_owned, code);
    })
    .map_err(|e| StartError::Spawn(format!("make: {e}")))?;
    let id = run.id;
    guard.insert(project.to_string(), run);
    Ok(id)
}

fn notify_exit(project: &str, target: &str, code: i32) {
    let level = if code != 0 { "err" } else { "info" };
    let text = format!("make {target} — exit {code} ({project})");
    let conn = db::db_static().lock();
    if let Err(e) = db::insert_notification(&conn, level, Some(project), &text) {
        log::warn!("task notification failed: {e}");
    }
}

/// `None` = this project never ran a task in this process.
pub fn status(project: &str) -> Option<Value> {
    let run = runner::lock(runs()).get(project).cloned()?;
    Some(json!({
        "active": run.active(),
        "id": run.id,
        "exit": run.exit(),
        "lines": run.lines_json(),
        "started_at": run.started_at,
        "duration_s": (run.duration_s() * 1000.0).round() / 1000.0,
    }))
}

/// SIGTERM the active child. false = no active task for this project.
pub fn kill(project: &str) -> bool {
    let pid = {
        let guard = runner::lock(runs());
        match guard.get(project) {
            Some(r) if r.active() => r.pid,
            _ => return false,
        }
    };
    runner::sigterm(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets_docs_and_traps() {
        let fixture = concat!(
            "# обычный комментарий сверху\n",
            "VAR := x\n",
            "OTHER ::= y\n",
            "Q ?= z\n",
            "## собрать релиз\n",
            "build: dep1 dep2\n",
            "\tcargo build --release\n",
            "\n",
            "test:\n",
            "\tcargo test\n",
            ".PHONY: build test\n",
            "## прогнать линт\n",
            "lint check:\n",
            "\ttrue\n",
            "install: build ## inline-хвост после правила\n",
            "\tcp a b\n",
        );
        let got = parse_makefile(fixture);
        let want = vec![
            Task { name: "build".into(), doc: Some("собрать релиз".into()) },
            Task { name: "test".into(), doc: None },
            Task { name: "lint".into(), doc: Some("прогнать линт".into()) },
            Task { name: "check".into(), doc: Some("прогнать линт".into()) },
            Task { name: "install".into(), doc: Some("inline-хвост после правила".into()) },
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn assignment_lines_and_dot_targets_never_match() {
        let got = parse_makefile(concat!(
            "A := 1\n",
            "B ::= 2\n",
            "C = 3\n",
            "D ?= 4\n",
            "E += 5\n",
            ".PHONY: x\n",
            ".DEFAULT_GOAL := all\n",
            "include foo.mk\n",
            "ifeq ($(X),1)\n",
            "endif\n",
        ));
        assert!(got.is_empty(), "no real targets in: {got:?}");
        assert!(!got.iter().any(|t| t.name.starts_with('.')));
    }

    #[test]
    fn doc_must_be_immediately_above() {
        let got = parse_makefile(concat!(
            "## устаревший док\n",
            "# прослойка\n",
            "target:\n",
            "\ttrue\n",
        ));
        assert_eq!(got, vec![Task { name: "target".into(), doc: None }]);
    }

    /// Structural shape of the user's real `~/Projects/finance/emigate/makefile`
    /// (private content stripped): a LOWERCASE `makefile`, `$(shell …)`
    /// assignments, `:=` / `?=` variables, `.PHONY` over two lines, plain
    /// `target: dep` rules and recipes with `$$` and `\` continuations. This is
    /// the file the endpoint returned an empty list for in the report — the
    /// parser must see every one of its targets.
    const APP_SHAPE: &str = r#"# приложение — сборка и запуск локальных инстансов.
BQNFLAGS = -DJIT_ENABLED=0

BIN      = app
PIDFILE  = run/app.pid
ARGS    ?=

API_ADDR = $(shell sed -n 's/.*"listenAddr": *"\([^"]*\)".*/\1/p' config.json | head -1)
API_PORT = $(lastword $(subst :, ,$(API_ADDR)))

.PHONY: all build up down restart status logs clean-app
.PHONY: libextra extra-dll clean-libextra

all: build

# Build the binary next to the sources (gitignored). The recipe always runs:
# a hand-rolled file rule silently kept a stale binary after source edits.
build: libextra.so
	CGO_ENABLED=0 go build -o $(BIN) .

libextra.so:
	$(MAKE) libextra

# Shared library for the FFI binding (vendored dependency).
libextra:
	$(MAKE) -C vendor/lib shared-o3 f="$(BQNFLAGS)"
	@lib=$$(ls vendor/lib/libextra.so vendor/lib/libextra.dylib 2>/dev/null | head -1); \
	test -n "$$lib" || { echo "no shared library produced"; exit 1; }; \
	cp -f "$$lib" . && echo "copied $$lib"

extra-dll: libextra
clean-libextra:
	rm -f libextra.so

up: build
	./$(BIN) -mock &
down:
	pkill -f $(BIN) || true
restart: down up
status:
	cat $(PIDFILE)
logs:
	tail -f run/app.log
clean-app:
	rm -rf run
"#;

    #[test]
    fn real_lowercase_makefile_shape_yields_every_target() {
        let got = parse_makefile(APP_SHAPE);
        let names: Vec<&str> = got.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "all",
                "build",
                "libextra.so",
                "libextra",
                "extra-dll",
                "clean-libextra",
                "up",
                "down",
                "restart",
                "status",
                "logs",
                "clean-app",
            ],
            "every real rule must be found (never an empty list)"
        );
        // a plain comment above a rule is a doc only with `##`
        assert_eq!(got[1].doc, None, "single-hash comments are not docs");
        // and the sanitized fixture still keeps no assignment line
        assert!(!names.contains(&"API_ADDR"));
        assert!(!names.contains(&"BQNFLAGS"));
    }

    /// `target: ## текст` on the rule itself is the self-documenting idiom the
    /// user's real `~/Projects/apps/lamplife/Makefile` uses on all 20 targets.
    #[test]
    fn inline_doc_on_the_rule_line_is_used() {
        let got = parse_makefile(concat!(
            "help: ## список команд\n",
            "\t@grep -E '^[a-z-]+:.*?## ' $(MAKEFILE_LIST)\n",
            "up: ## поднять стек\n",
            "\tdocker compose up -d\n",
            "## док сверху важнее хвоста\n",
            "down: dep ## хвост игнорируется\n",
            "\ttrue\n",
            "no-doc:\n",
            "\ttrue\n",
        ));
        assert_eq!(
            got,
            vec![
                Task { name: "help".into(), doc: Some("список команд".into()) },
                Task { name: "up".into(), doc: Some("поднять стек".into()) },
                Task { name: "down".into(), doc: Some("док сверху важнее хвоста".into()) },
                Task { name: "no-doc".into(), doc: None },
            ]
        );
        // an inline `#` comment (CBQN's real makefile) is not a doc
        assert_eq!(
            parse_makefile("for-build: # for running the build system\n\ttrue\n"),
            vec![Task { name: "for-build".into(), doc: None }]
        );
        // and a `##` before the colon can never match (no colon → not a rule)
        assert!(parse_makefile("## это не правило\n").is_empty());
    }

    /// Real makefiles spread a target list over a backslash continuation
    /// (`.PHONY` aside, this is legal for rules too); CRLF input must parse as
    /// well — the user's CBQN makefile is ASCII text with CRLF terminators.
    #[test]
    fn continuations_and_crlf_lines_parse() {
        let got = parse_makefile("build test \\\n\tlint: dep\n\ttrue\n");
        let names: Vec<&str> = got.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "lint"]);
        assert!(got.iter().all(|t| t.doc.is_none()));

        let crlf = parse_makefile("default: o3\r\no3:\r\n\tmake -j\r\nclean-build:\r\n\trm -rf build\r\n");
        let names: Vec<&str> = crlf.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["default", "o3", "clean-build"]);

        // a continued `.PHONY` list never becomes targets (it is not a rule),
        // and it must not swallow the real rules that follow it
        let got2 = parse_makefile(concat!(
            ".PHONY: help up down \\\n",
            "\tps logs url open\n",
            "up:\n",
            "\ttrue\n",
        ));
        assert_eq!(
            got2,
            vec![Task { name: "up".into(), doc: None }],
            "continuation of a rejected line must not invent targets"
        );
    }

    #[test]
    fn multi_target_line_lists_each_and_find_makefile_order() {
        assert_eq!(
            target_names("a b c: dep"),
            Some(vec!["a".into(), "b".into(), "c".into()])
        );
        assert_eq!(target_names("a := b"), None);
        assert_eq!(target_names(".PHONY: a"), None);
        let dir = std::env::temp_dir().join(format!("atlas-mk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(find_makefile(&dir).is_none());
        std::fs::write(dir.join("GNUmakefile"), "x:\n").unwrap();
        assert_eq!(find_makefile(&dir), Some(dir.join("GNUmakefile")));
        std::fs::write(dir.join("Makefile"), "y:\n").unwrap();
        assert_eq!(find_makefile(&dir), Some(dir.join("Makefile")));
        // a project whose `path` has no makefile at the root stays empty: the
        // endpoint does NOT scan subdirectories (see README «Задачи make»)
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/Makefile"), "z:\n").unwrap();
        let bare = dir.join("only-sub");
        std::fs::create_dir_all(bare.join("sub")).unwrap();
        std::fs::write(bare.join("sub/Makefile"), "z:\n").unwrap();
        assert!(find_makefile(&bare).is_none(), "no recursive scanning");
        // a DIRECTORY named Makefile is not a makefile
        let weird = dir.join("weird");
        std::fs::create_dir_all(weird.join("Makefile")).unwrap();
        assert!(find_makefile(&weird).is_none());
        // the user's real project layout: lowercase `makefile` wins in order
        let lower = dir.join("lower");
        std::fs::create_dir_all(&lower).unwrap();
        std::fs::write(lower.join("makefile"), "q:\n").unwrap();
        assert_eq!(find_makefile(&lower), Some(lower.join("makefile")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
