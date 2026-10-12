//! Shared test scaffolding for the `web` modules: one router over a
//! throwaway DB, the JSON `call` helper, and the board/git fixtures the
//! contract tests seed. Compiled only under `cfg(test)`.

use super::{router, AppState};
use crate::orch::Orchestrator;
use crate::stt::SttState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::Value;
use std::path::Path;
use tower::ServiceExt;

/// One router over a throwaway DB file. `cfg::CFG` and the DB are
/// process-global (OnceLock), so this runs once for the whole test binary
/// and every request below shares it.
pub(crate) fn test_router() -> Router {
    crate::db::test_init_once();
    // projects the focus contract needs: one with a path, one without.
    // `/tmp` never hosts a shell here — the terminal paths below are all
    // dry runs, so nothing is spawned and no real project directory is
    // touched.
    {
        let conn = crate::db::db_static().lock();
        conn.execute(
            "INSERT INTO projects(name,path) VALUES('lamplife', ?1) \
             ON CONFLICT(name) DO UPDATE SET path = excluded.path",
            rusqlite::params![std::env::temp_dir().join("lamplife").to_string_lossy()],
        )
        .expect("seed project");
        conn.execute(
            "INSERT INTO projects(name,path) VALUES('phone-access', NULL) \
             ON CONFLICT(name) DO UPDATE SET path = NULL",
            [],
        )
        .expect("seed project");
    }
    router(AppState {
        orch: Orchestrator::new(),
        stt: SttState::new(),
    })
}

pub(crate) async fn call(
    r: &Router,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = r
        .clone()
        .oneshot(req.body(body).expect("request"))
        .await
        .expect("router call");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

// ------------------------------------------- board delegation («Этап B») ---

/// One board item with the given brief; returns its id.
pub(crate) fn seed_board_item(title: &str, project: Option<&str>, description: Option<&str>) -> i64 {
    let conn = crate::db::db_static().lock();
    conn.execute(
        "INSERT INTO items(title,quadrant,project,description,created_at,updated_at) \
         VALUES(?1,'q1',?2,?3,1.0,1.0)",
        rusqlite::params![title, project, description],
    )
    .expect("seed item");
    conn.last_insert_rowid()
}

pub(crate) fn seed_project(name: &str, path: Option<&str>) {
    let conn = crate::db::db_static().lock();
    conn.execute(
        "INSERT INTO projects(name,path) VALUES(?1,?2) \
         ON CONFLICT(name) DO UPDATE SET path = excluded.path",
        rusqlite::params![name, path],
    )
    .expect("seed project");
}

pub(crate) fn set_delegation(id: i64, state: &str, target: Option<&str>, reference: Option<&str>) {
    let conn = crate::db::db_static().lock();
    conn.execute(
        "UPDATE items SET delegation_state=?1, delegation_target=?2, delegation_ref=?3 \
         WHERE id=?4",
        rusqlite::params![state, target, reference, id],
    )
    .expect("set delegation");
}

/// The item's `delegation` object off the board.
pub(crate) async fn board_delegation(r: &Router, id: i64) -> Value {
    let (st, board) = call(r, "GET", "/api/board", None).await;
    assert_eq!(st, StatusCode::OK);
    for q in ["q1", "q2", "q3", "q4"] {
        if let Some(arr) = board[q].as_array() {
            for it in arr {
                if it["id"].as_i64() == Some(id) {
                    return it["delegation"].clone();
                }
            }
        }
    }
    panic!("item {id} not on the board");
}

/// A throwaway git repo with one commit (`file.txt` = `base`).
pub(crate) fn git_repo(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("atlas-deleg-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("repo dir");
    git_in(&dir, &["init", "-b", "main"]);
    git_in(&dir, &["config", "user.email", "atlas@test.invalid"]);
    git_in(&dir, &["config", "user.name", "atlas test"]);
    git_in(&dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("file.txt"), "base\n").unwrap();
    git_in(&dir, &["add", "."]);
    git_in(&dir, &["commit", "-m", "base"]);
    dir
}

pub(crate) fn git_try(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// git in `dir`, asserting success (fixture-building steps).
pub(crate) fn git_in(dir: &Path, args: &[&str]) -> String {
    let (ok, out) = git_try(dir, args);
    assert!(ok, "git {args:?} in {}: {out}", dir.display());
    out
}

