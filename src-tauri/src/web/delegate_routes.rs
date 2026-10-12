//! Delegation verbs on board items («Этап B»): preview/start/cancel/run/diff,
//! the review verdict (accept/reject) and the pristine reset. Thin HTTP over
//! `crate::delegate` (reset is plain item-column SQL through the shared lock).

use super::{ok_json, ApiError};
use crate::delegate;
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use serde_json::{json, Value};

// -------------------------------------------- board delegation («Этап B») ----

/// HTTP shape of a [`delegate::Error`].
impl From<delegate::Error> for ApiError {
    fn from(e: delegate::Error) -> ApiError {
        match e {
            delegate::Error::NotFound(m) => ApiError::not_found(m),
            delegate::Error::Busy(m) => ApiError::new(StatusCode::CONFLICT, m),
            delegate::Error::NoPath(m) | delegate::Error::Bad(m) => ApiError::unprocessable(m),
            delegate::Error::Io(m) => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, m),
        }
    }
}

/// Item id of `…/item/{id}/<suffix…>`: `trailing` = segments after the id.
fn item_id_at(uri: &Uri, trailing: usize) -> Result<i64, ApiError> {
    let seg: Vec<&str> = uri.path().rsplit('/').collect();
    seg.get(trailing)
        .copied()
        .unwrap_or("")
        .parse()
        .map_err(|_| ApiError::unprocessable("item id must be an integer"))
}

/// Body of every delegation verb: `{"target":"orchestrator"|"worktree"|"dir"}`.
fn delegate_target(body: &str) -> Result<delegate::Target, ApiError> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    v.get("target")
        .and_then(|t| t.as_str())
        .and_then(delegate::Target::parse)
        .ok_or_else(|| {
            ApiError::unprocessable(format!(
                "target must be one of {}",
                delegate::TARGETS
                    .iter()
                    .map(|t| format!("\"{t}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

/// POST /api/board/item/{id}/delegate/preview — compile the brief, start
/// nothing: `{"ok":true,"brief":"…","cwd":"…"}`.
pub(crate) async fn api_delegate_preview(uri: Uri, body: String) -> Result<Response, ApiError> {
    let id = item_id_at(&uri, 2)?;
    let target = delegate_target(&body)?;
    Ok(ok_json(delegate::preview(id, target).await?))
}

/// POST /api/board/item/{id}/delegate — start the delegation. The panel's
/// interactive orchestrator is NOT involved: every target spawns its own
/// worker (rpc child or headless `omp -p`), so delegations never collide with
/// the chat and run in parallel.
pub(crate) async fn api_delegate_start(uri: Uri, body: String) -> Result<Response, ApiError> {
    let id = item_id_at(&uri, 1)?;
    let target = delegate_target(&body)?;
    Ok(ok_json(delegate::start(id, target).await?))
}

/// POST /api/board/item/{id}/delegate/cancel — stop the worker or the
/// delegation's rpc child (SIGTERM, then SIGKILL after the grace).
pub(crate) async fn api_delegate_cancel(uri: Uri) -> Result<Response, ApiError> {
    let id = item_id_at(&uri, 2)?;
    Ok(ok_json(delegate::cancel(id).await?))
}

/// GET /api/board/item/{id}/run — state + ring tail of the delegation run.
pub(crate) async fn api_delegate_run(uri: Uri) -> Result<Response, ApiError> {
    let id = item_id_at(&uri, 1)?;
    Ok(ok_json(delegate::run_status(id).await?))
}

/// GET /api/board/item/{id}/diff — the worker's changes relative to the start
/// HEAD (`stale:true` + bare-HEAD fallback for legacy rows), commit list and
/// untracked files included.
pub(crate) async fn api_delegate_diff(uri: Uri) -> Result<Response, ApiError> {
    let id = item_id_at(&uri, 1)?;
    Ok(ok_json(delegate::diff(id).await?))
}

/// POST /api/board/item/{id}/accept — `{"merge":bool}`: close the item (and,
/// for worktree targets, merge the task branch into the project).
pub(crate) async fn api_board_accept(uri: Uri, body: String) -> Result<Response, ApiError> {
    let id = item_id_at(&uri, 1)?;
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let merge = v.get("merge").and_then(|m| m.as_bool()).unwrap_or(false);
    Ok(ok_json(delegate::accept(id, merge).await?))
}

/// POST /api/board/item/{id}/reject — `{"feedback":"…"}`: store the feedback
/// and put the item back to `queued`.
pub(crate) async fn api_board_reject(uri: Uri, body: String) -> Result<Response, ApiError> {
    let id = item_id_at(&uri, 1)?;
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let feedback = v
        .get("feedback")
        .and_then(|f| f.as_str())
        .ok_or_else(|| ApiError::unprocessable("feedback must be a string"))?;
    Ok(ok_json(delegate::reject(id, feedback.to_string()).await?))
}

/// POST /api/board/item/{id}/delegate/reset — wipe every delegation column
/// back to the never-delegated defaults (`state=''`, the rest NULL): the
/// board renders the item as `delegation:null` again. A live delegation is
/// refused — the `running` guard lives inside the UPDATE, so the check and
/// the write are one atomic statement under the DB lock; nothing is killed.
pub(crate) async fn api_delegate_reset(uri: Uri) -> Result<Response, ApiError> {
    let id = item_id_at(&uri, 2)?;
    let cleared = {
        let conn = super::db_lock();
        conn.execute(
            "UPDATE items SET delegation_state='', delegation_target=NULL, delegation_ref=NULL, \
             delegation_run_id=NULL, delegation_started=NULL, delegation_ended=NULL, \
             delegation_exit=NULL, delegation_feedback=NULL, delegation_head=NULL, \
             delegation_final=NULL, delegation_session=NULL, delegation_tail=NULL, \
             delegation_cmds=NULL, delegation_review=NULL, delegation_attempts=0, updated_at=?1 \
             WHERE id=?2 AND delegation_state <> 'running'",
            rusqlite::params![crate::util::now(), id],
        )
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
    };
    if cleared == 0 {
        // The guarded UPDATE matched nothing: an unknown id is 404, a still
        // running delegation is 409 (cancel/reap first — reset kills nothing).
        let exists = {
            let conn = super::db_lock();
            conn.query_row("SELECT 1 FROM items WHERE id=?1", [id], |_| Ok(()))
                .is_ok()
        };
        if !exists {
            return Err(ApiError::not_found(format!("item {id} not found")));
        }
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "делегация выполняется — сначала отмена",
        ));
    }
    Ok(ok_json(json!({"ok": true, "state": ""})))
}

#[cfg(test)]
mod tests {
    use crate::http::web::support::{
        board_delegation, call, git_in, git_repo, git_try, seed_board_item, seed_project,
        set_delegation, test_router,
    };
    use axum::http::StatusCode;
    use serde_json::{json, Value};
    /// «Этап B» preview: the brief is compiled without starting anything, the
    /// cwd is the target's working directory, and the guards are honest
    /// (404 unknown item, 422 a target that needs a project with a path).
    #[tokio::test]
    async fn delegate_preview_compiles_the_brief_only() {
        let r = test_router();
        let id = seed_board_item("preview-дело", Some("lamplife"), Some("  починить  "));
        // never delegated → the board carries a null delegation
        assert_eq!(board_delegation(&r, id).await, Value::Null);

        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/delegate/preview"),
            Some(r#"{"target":"orchestrator"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["ok"], true);
        let cwd = std::env::temp_dir().join("lamplife").to_string_lossy().into_owned();
        assert_eq!(
            out["cwd"], cwd,
            "orchestrator delegation works in the item's project, not the atlas repo"
        );
        let brief = out["brief"].as_str().unwrap();
        assert!(brief.starts_with("# preview-дело\nпочинить\n"), "{brief}");
        assert!(
            brief.contains(&format!(
                "Работай в каталоге {cwd}. Ты работаешь автономно: ответить на вопрос некому — \
                 если данных не хватает, остановись, заполни раздел ВОПРОСЫ, ничего не ломай."
            )),
            "{brief}"
        );
        assert!(
            brief.contains("## Итог (обязательно)")
                && brief.contains("СДЕЛАНО:")
                && brief.contains("ИЗМЕНЕНО:")
                && brief.contains("ПРОВЕРЕНО:")
                && brief.contains("ВОПРОСЫ:"),
            "the brief ends with the final-message contract: {brief}"
        );
        assert!(
            !brief.contains("## Шаги") && !brief.contains("## Файлы") && !brief.contains("## Фидбек"),
            "empty sections are dropped: {brief}"
        );

        // a target that needs a project path: project without a path → 422
        let nopath = seed_board_item("без пути", Some("phone-access"), None);
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{nopath}/delegate/preview"),
            Some(r#"{"target":"worktree"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        assert!(out["detail"].as_str().unwrap().contains("нет пути"), "{out}");
        // ... and an item with no project at all
        let orphan = seed_board_item("без проекта", None, None);
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{orphan}/delegate/preview"),
            Some(r#"{"target":"dir"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        assert!(out["detail"].as_str().unwrap().contains("нет проекта"), "{out}");

        // unknown target → 422; unknown item → 404
        let (st, _) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/delegate/preview"),
            Some(r#"{"target":"remote"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let (st, _) = call(
            &r,
            "POST",
            "/api/board/item/999999/delegate/preview",
            Some(r#"{"target":"dir"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    /// The busy guards: an item already `running` is a 409 on every
    /// target (`queued` delegates fine — it IS the post-reject follow-up
    /// state), an existing worktree path is a 409 before anything spawns, and
    /// cancel/accept/reject off the right state are 409s — never a silent
    /// no-op.
    #[tokio::test]
    async fn delegate_busy_and_wrong_state_are_409() {
        let r = test_router();
        let running = seed_board_item("уже идёт", Some("lamplife"), None);
        set_delegation(running, "running", Some("worktree"), Some("/tmp/nope"));
        for target in ["orchestrator", "worktree", "dir"] {
            let (st, out) = call(
                &r,
                "POST",
                &format!("/api/board/item/{running}/delegate"),
                Some(&format!(r#"{{"target":"{target}"}}"#)),
            )
            .await;
            assert_eq!(st, StatusCode::CONFLICT, "target {target}: {out}");
            assert!(
                out["detail"].as_str().unwrap().contains("уже делегировано"),
                "{out}"
            );
        }

        // nothing runs → 409; accept off review → 409; reject while busy → 409
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{running}/delegate/cancel"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{out}");
        assert!(
            out["detail"].as_str().unwrap().contains("ничего не выполняется"),
            "{out}"
        );
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{running}/accept"),
            Some(r#"{"merge":false}"#),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{out}");
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{running}/reject"),
            Some(r#"{"feedback":"x"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{out}");

        // merge=true is worktree-only → 422 (the item IS on review)
        let dir_item = seed_board_item("dir-приём", Some("lamplife"), None);
        set_delegation(dir_item, "review", Some("dir"), Some("/tmp/nope"));
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{dir_item}/accept"),
            Some(r#"{"merge":true}"#),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        assert!(out["detail"].as_str().unwrap().contains("worktree"), "{out}");

        // a target that needs a project path, on the DELEGATE verb itself:
        // no project path → 422, unknown item → 404, bad target → 422
        let nopath = seed_board_item("делегировать некуда", Some("phone-access"), None);
        for target in ["worktree", "dir"] {
            let (st, out) = call(
                &r,
                "POST",
                &format!("/api/board/item/{nopath}/delegate"),
                Some(&format!(r#"{{"target":"{target}"}}"#)),
            )
            .await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "target {target}: {out}");
            assert!(out["detail"].as_str().unwrap().contains("нет пути"), "{out}");
        }
        let (st, _) = call(
            &r,
            "POST",
            &format!("/api/board/item/{nopath}/delegate"),
            Some(r#"{"target":"remote"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let (st, _) = call(
            &r,
            "POST",
            "/api/board/item/999999/delegate",
            Some(r#"{"target":"dir"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = call(
            &r,
            "POST",
            &format!("/api/board/item/{nopath}/delegate/cancel"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "nothing runs for a fresh item");

        // an existing worktree path is a 409 before git or omp run
        let wt_item = seed_board_item("ворктри занят", Some("wt-busy"), None);
        let repo = std::env::temp_dir().join(format!("atlas-deleg-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        seed_project("wt-busy", Some(&repo.to_string_lossy()));
        let wt = crate::delegate::worktree_dir("wt-busy", wt_item, "ворктри занят");
        std::fs::create_dir_all(&wt).expect("pre-create the worktree dir");
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{wt_item}/delegate"),
            Some(r#"{"target":"worktree"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{out}");
        assert!(
            out["detail"].as_str().unwrap().contains("ворктри уже существует"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The review gate: reject stores the feedback, flips the item back to
    /// `queued` and the NEXT preview compiles it into «Фидбек».
    #[tokio::test]
    async fn delegate_reject_requeues_and_feeds_the_next_brief() {
        let r = test_router();
        let id = seed_board_item("на ревью", Some("lamplife"), Some("бриф"));
        set_delegation(id, "review", Some("dir"), Some("/tmp/nope"));

        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/reject"),
            Some(r#"{"feedback":"  тесты падали  "}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["ok"], true);
        let d = board_delegation(&r, id).await;
        assert_eq!(d["state"], "queued");
        assert_eq!(d["target"], "dir");

        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/delegate/preview"),
            Some(r#"{"target":"dir"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let brief = out["brief"].as_str().unwrap();
        assert!(
            brief.contains("## Фидбек к прошлой попытке\nтесты падали\n"),
            "{brief}"
        );

        // blank feedback → 422; unknown item → 404
        set_delegation(id, "review", Some("dir"), Some("/tmp/nope"));
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/reject"),
            Some(r#"{"feedback":"   "}"#),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        let (st, _) = call(
            &r,
            "POST",
            "/api/board/item/999999/reject",
            Some(r#"{"feedback":"x"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    /// `GET …/run` mirrors the persisted state (an empty shape before the first
    /// delegation), `GET …/diff` is honest about nothing-to-diff and non-repos
    /// and reports a real change otherwise.
    #[tokio::test]
    async fn delegate_run_and_diff_are_honest() {
        let r = test_router();
        let plain = seed_board_item("не делегировали", None, None);
        let (st, out) = call(&r, "GET", &format!("/api/board/item/{plain}/run"), None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["state"], "");
        assert_eq!(out["lines"], json!([]));
        assert_eq!(out["exit"], Value::Null);
        assert_eq!(out["started"], Value::Null);
        assert_eq!(out["duration_s"], Value::Null);
        assert_eq!(out["truncated"], false);
        let (st, out) = call(&r, "GET", &format!("/api/board/item/{plain}/diff"), None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["ok"], false);
        assert_eq!(out["ref"], Value::Null);

        // a non-repo ref → ok:false with a named reason
        let notrepo =
            std::env::temp_dir().join(format!("atlas-deleg-notrepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&notrepo);
        std::fs::create_dir_all(&notrepo).unwrap();
        let id = seed_board_item("диф не-репо", Some("lamplife"), None);
        set_delegation(id, "review", Some("dir"), Some(&notrepo.to_string_lossy()));
        let (st, out) = call(&r, "GET", &format!("/api/board/item/{id}/diff"), None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["ok"], false);
        assert_eq!(out["detail"], "не git-репозиторий");

        // a real repo with uncommitted AND staged changes → stat + diff
        let repo = git_repo("diff-real");
        std::fs::write(repo.join("file.txt"), "base\nchanged\n").unwrap();
        std::fs::write(repo.join("fresh.txt"), "new file\n").unwrap();
        git_in(&repo, &["add", "fresh.txt"]);
        let id2 = seed_board_item("диф реальный", Some("lamplife"), None);
        set_delegation(id2, "review", Some("dir"), Some(&repo.to_string_lossy()));
        let (st, out) = call(&r, "GET", &format!("/api/board/item/{id2}/diff"), None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(out["ref"], repo.to_string_lossy().as_ref());
        assert_eq!(out["truncated"], false);
        assert!(out["stat"].as_str().unwrap().contains("file.txt"), "{out}");
        assert!(out["diff"].as_str().unwrap().contains("+changed"), "{out}");
        assert!(out["diff"].as_str().unwrap().contains("fresh.txt"), "{out}");

        // an UNTRACKED file shows too: a worker that only ADDS files leaves
        // them untracked, where `git diff HEAD` alone is blind
        std::fs::write(repo.join("untracked.txt"), "brand new\n").unwrap();
        let (st, out) = call(&r, "GET", &format!("/api/board/item/{id2}/diff"), None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert!(out["ok"].as_bool().unwrap(), "{out}");
        assert!(out["diff"].as_str().unwrap().contains("untracked.txt"), "{out}");
        assert!(out["diff"].as_str().unwrap().contains("+brand new"), "{out}");
        assert!(out["stat"].as_str().unwrap().contains("untracked.txt"), "{out}");

        // /run reads the persisted row even when the in-process ring is gone
        {
            let conn = crate::db::db_static().lock();
            conn.execute(
                "UPDATE items SET delegation_state='running', delegation_started=100.0, \
                 delegation_ended=101.5, delegation_exit=NULL WHERE id=?1",
                [id2],
            )
            .unwrap();
        }
        let (st, out) = call(&r, "GET", &format!("/api/board/item/{id2}/run"), None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["state"], "running");
        assert_eq!(out["started"], 100.0);
        assert_eq!(out["duration_s"], 1.5);
        assert_eq!(out["exit"], Value::Null);

        // unknown item → 404 on both
        let (st, _) = call(&r, "GET", "/api/board/item/999999/run", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = call(&r, "GET", "/api/board/item/999999/diff", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&notrepo);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The accept gate against REAL throwaway repos: a clean merge lands in the
    /// project checkout and removes the worktree; a conflicting merge is
    /// aborted, stays on `review` and says so.
    #[tokio::test]
    async fn delegate_accept_merges_or_aborts_on_conflict() {
        let r = test_router();

        // --- conflict: both sides touch file.txt
        let repo = git_repo("accept-conflict");
        let project = format!("deleg-c-{}", std::process::id());
        seed_project(&project, Some(&repo.to_string_lossy()));
        let id = seed_board_item("мерж конфликт", Some(&project), None);
        let slug = crate::delegate::slug(id, "мерж конфликт");
        let wt = std::env::temp_dir().join(format!("atlas-deleg-wt-c-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wt);
        git_in(
            &repo,
            &["worktree", "add", &wt.to_string_lossy(), "-b", &format!("task/{slug}")],
        );
        std::fs::write(wt.join("file.txt"), "wt side\n").unwrap();
        git_in(&wt, &["add", "."]);
        git_in(&wt, &["commit", "-m", "worker"]);
        std::fs::write(repo.join("file.txt"), "main side\n").unwrap();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-m", "main"]);
        set_delegation(id, "review", Some("worktree"), Some(&wt.to_string_lossy()));

        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/accept"),
            Some(r#"{"merge":true}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["ok"], false, "{out}");
        assert_eq!(out["detail"], "конфликт слияния — решай вручную");
        // the merge was aborted and the checkout is untouched
        assert!(
            !git_try(&repo, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).0,
            "MERGE_HEAD must be gone after the abort"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("file.txt")).unwrap(),
            "main side\n"
        );
        // the item stays on review, the worktree is still there
        assert_eq!(board_delegation(&r, id).await["state"], "review");
        assert!(wt.exists(), "a conflict must not remove the worktree");

        // ... and reject then puts it back to queued
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/reject"),
            Some(r#"{"feedback":"конфликт"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(board_delegation(&r, id).await["state"], "queued");
        let _ = git_try(&repo, &["worktree", "remove", "--force", &wt.to_string_lossy()]);

        // --- clean merge: the worker leaves its file UNCOMMITTED (the common
        // `omp -p` shape) — the accept path commits it onto the task branch,
        // the merge lands it in the checkout and the worktree goes away
        let repo2 = git_repo("accept-clean");
        let project2 = format!("deleg-k-{}", std::process::id());
        seed_project(&project2, Some(&repo2.to_string_lossy()));
        let id2 = seed_board_item("мерж чистый", Some(&project2), None);
        let slug2 = crate::delegate::slug(id2, "мерж чистый");
        let wt2 = std::env::temp_dir().join(format!("atlas-deleg-wt-k-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wt2);
        git_in(
            &repo2,
            &["worktree", "add", &wt2.to_string_lossy(), "-b", &format!("task/{slug2}")],
        );
        std::fs::write(wt2.join("added.txt"), "hi\n").unwrap();
        set_delegation(id2, "review", Some("worktree"), Some(&wt2.to_string_lossy()));

        // the review gate sees the untracked file before it is merged
        let (st, out) = call(&r, "GET", &format!("/api/board/item/{id2}/diff"), None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert!(out["diff"].as_str().unwrap().contains("added.txt"), "{out}");

        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id2}/accept"),
            Some(r#"{"merge":true}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(out["merged"], true);
        assert_eq!(
            std::fs::read_to_string(repo2.join("added.txt")).unwrap(),
            "hi\n",
            "the worker's uncommitted work is in the project checkout"
        );
        assert!(
            git_try(&repo2, &["log", "--oneline"]).1.contains("atlas: делегация дела"),
            "the pending work was committed onto the task branch"
        );
        assert!(!wt2.exists(), "the accepted worktree is removed");
        assert!(
            !git_try(&repo2, &["worktree", "list"])
                .1
                .contains(&wt2.to_string_lossy().into_owned()),
            "the worktree is de-registered"
        );
        assert_eq!(board_delegation(&r, id2).await["state"], "done");

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&repo2);
    }

    /// accept with `merge:false` closes the item and names the leftover
    /// worktree in the notification — no git action at all.
    #[tokio::test]
    async fn delegate_accept_without_merge_keeps_the_worktree() {
        let r = test_router();
        let wt = std::env::temp_dir().join(format!("atlas-deleg-keep-{}", std::process::id()));
        std::fs::create_dir_all(&wt).unwrap();
        let id = seed_board_item("приём без мержа", Some("lamplife"), None);
        set_delegation(id, "review", Some("worktree"), Some(&wt.to_string_lossy()));
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/accept"),
            Some(r#"{"merge":false}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["ok"], true);
        assert_eq!(board_delegation(&r, id).await["state"], "done");
        assert!(wt.exists(), "merge:false never touches the worktree");
        // the notification names the leftover worktree (the shared test DB is
        // written by sibling tests in parallel — look for THIS text, not the
        // newest row)
        let needle = format!("принято, ворктри остался: {}", wt.to_string_lossy());
        let hits: i64 = crate::db::db_static()
            .lock()
            .query_row(
                "SELECT COUNT(*) FROM notifications WHERE text LIKE '%' || ?1 || '%'",
                [needle.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "«{needle}» missing from the notification ring");
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// delegate/reset: a review item goes back to pristine — every delegation
    /// column returns to its never-delegated default and the board renders
    /// `delegation:null` again.
    #[tokio::test]
    async fn delegate_reset_returns_review_item_to_pristine() {
        let r = test_router();
        let id = seed_board_item("сброс после ревью", Some("lamplife"), None);
        set_delegation(id, "review", Some("dir"), Some("/tmp/atlas-reset-review"));
        {
            let conn = crate::db::db_static().lock();
            conn.execute(
                "UPDATE items SET delegation_run_id=7, delegation_started=10.0, \
                 delegation_ended=11.5, delegation_exit=0, delegation_feedback='правки' \
                 WHERE id=?1",
                [id],
            )
            .unwrap();
        }
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/delegate/reset"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(out["state"], "", "{out}");
        // every delegation_* column is back to its schema default
        crate::db::db_static()
            .lock()
            .query_row(
                "SELECT delegation_state, delegation_target, delegation_ref, delegation_run_id, \
                 delegation_started, delegation_ended, delegation_exit, delegation_feedback \
                 FROM items WHERE id=?1",
                [id],
                |row| {
                    assert_eq!(row.get::<_, String>(0)?, "");
                    assert!(row.get::<_, Option<String>>(1)?.is_none());
                    assert!(row.get::<_, Option<String>>(2)?.is_none());
                    assert!(row.get::<_, Option<i64>>(3)?.is_none());
                    assert!(row.get::<_, Option<f64>>(4)?.is_none());
                    assert!(row.get::<_, Option<f64>>(5)?.is_none());
                    assert!(row.get::<_, Option<i64>>(6)?.is_none());
                    assert!(row.get::<_, Option<String>>(7)?.is_none());
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(board_delegation(&r, id).await, Value::Null);
    }

    /// delegate/reset refuses a live delegation atomically: 409 and the row
    /// is untouched — reset kills nothing, cancel/reap is the only way out.
    #[tokio::test]
    async fn delegate_reset_refuses_a_running_delegation() {
        let r = test_router();
        let id = seed_board_item("сброс при running", None, None);
        set_delegation(id, "running", Some("dir"), Some("/tmp/atlas-reset-live"));
        {
            let conn = crate::db::db_static().lock();
            conn.execute("UPDATE items SET delegation_started=42.0 WHERE id=?1", [id])
                .unwrap();
        }
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/delegate/reset"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{out}");
        assert!(
            out["detail"]
                .as_str()
                .unwrap()
                .contains("делегация выполняется"),
            "{out}"
        );
        // the guarded UPDATE left the row exactly as it was
        crate::db::db_static()
            .lock()
            .query_row(
                "SELECT delegation_state, delegation_target, delegation_ref, delegation_started \
                 FROM items WHERE id=?1",
                [id],
                |row| {
                    assert_eq!(row.get::<_, String>(0)?, "running");
                    assert_eq!(row.get::<_, Option<String>>(1)?.as_deref(), Some("dir"));
                    assert_eq!(
                        row.get::<_, Option<String>>(2)?.as_deref(),
                        Some("/tmp/atlas-reset-live")
                    );
                    assert_eq!(row.get::<_, Option<f64>>(3)?, Some(42.0));
                    Ok(())
                },
            )
            .unwrap();
    }

    /// delegate/reset on an unknown item → 404, same as the sibling verbs.
    #[tokio::test]
    async fn delegate_reset_unknown_item_is_404() {
        let r = test_router();
        let (st, out) = call(
            &r,
            "POST",
            "/api/board/item/999999/delegate/reset",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{out}");
    }

    /// The review gate diffs against the START HEAD recorded at delegation
    /// start — never a bare `git diff HEAD`: work the worker COMMITTED is
    /// visible (the item-24 blindness), the commit list rides along, and a
    /// legacy row without a recorded start says `stale:true` and keeps the old
    /// bare-HEAD behavior.
    #[tokio::test]
    async fn delegate_diff_is_relative_to_the_start_head() {
        let r = test_router();
        let repo = git_repo("diff-start");
        let start = git_in(&repo, &["rev-parse", "HEAD"]);
        let id = seed_board_item("диф от старта", Some("lamplife"), None);
        set_delegation(id, "review", Some("dir"), Some(&repo.to_string_lossy()));

        // the worker COMMITS its work — exactly what the old gate could not see
        std::fs::write(repo.join("file.txt"), "base\nworker edit\n").unwrap();
        std::fs::write(repo.join("added.txt"), "new file\n").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "воркер: правка и новый файл"]);
        {
            let conn = crate::db::db_static().lock();
            conn.execute(
                "UPDATE items SET delegation_head=?1 WHERE id=?2",
                rusqlite::params![start.trim(), id],
            )
            .unwrap();
        }
        let (st, out) = call(&r, "GET", &format!("/api/board/item/{id}/diff"), None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(out["stale"], false);
        assert_eq!(out["start"], start.trim());
        assert!(out["diff"].as_str().unwrap().contains("+worker edit"), "{out}");
        assert!(out["diff"].as_str().unwrap().contains("added.txt"), "{out}");
        assert!(out["stat"].as_str().unwrap().contains("file.txt"), "{out}");
        assert!(
            out["commits"].as_str().unwrap().contains("воркер: правка и новый файл"),
            "{out}"
        );

        // uncommitted and untracked work on top is visible too
        std::fs::write(repo.join("file.txt"), "base\nworker edit\n\nmore\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "brand new\n").unwrap();
        let (st, out) = call(&r, "GET", &format!("/api/board/item/{id}/diff"), None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert!(out["diff"].as_str().unwrap().contains("+more"), "{out}");
        assert!(out["diff"].as_str().unwrap().contains("untracked.txt"), "{out}");
        assert!(out["diff"].as_str().unwrap().contains("+brand new"), "{out}");

        // a legacy row without a recorded start: old bare-HEAD behavior, the
        // committed work is INVISIBLE, and the response says so
        let id2 = seed_board_item("легаси без старта", Some("lamplife"), None);
        set_delegation(id2, "review", Some("dir"), Some(&repo.to_string_lossy()));
        let (st, out) = call(&r, "GET", &format!("/api/board/item/{id2}/diff"), None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["ok"], true);
        assert_eq!(out["stale"], true);
        assert_eq!(out["start"], Value::Null);
        assert_eq!(out["commits"], "");
        assert!(
            !out["diff"].as_str().unwrap().contains("+worker edit"),
            "committed work is exactly the old gate's blind spot: {out}"
        );
        assert!(out["diff"].as_str().unwrap().contains("untracked.txt"), "{out}");

        // reset wipes the recorded start with the rest of the columns
        let (st, _) = call(&r, "POST", &format!("/api/board/item/{id}/delegate/reset"), None).await;
        assert_eq!(st, StatusCode::OK);
        let head: Option<String> = crate::db::db_static()
            .lock()
            .query_row("SELECT delegation_head FROM items WHERE id=?1", [id], |row| row.get(0))
            .unwrap();
        assert!(head.is_none(), "reset must clear the start HEAD");

        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The persisted ring tail: once the in-process run is gone (a server
    /// restart), `GET …/run` serves the tail a terminal run wrote into the row
    /// — the evidence of a run survives the process that made it.
    #[tokio::test]
    async fn delegate_run_serves_the_persisted_tail_after_the_ring_is_gone() {
        let r = test_router();
        let id = seed_board_item("хвост после рестарта", Some("lamplife"), None);
        let tail = serde_json::to_string(&vec![
            "— ход запущен —", "· bash cargo test", "! тишина 300 с — ход остановлен",
        ])
        .unwrap();
        {
            let conn = crate::db::db_static().lock();
            conn.execute(
                "UPDATE items SET delegation_state='failed', delegation_ended=200.0, \
                 delegation_started=100.0, delegation_exit=NULL, delegation_tail=?1 WHERE id=?2",
                rusqlite::params![tail, id],
            )
            .unwrap();
        }
        let (st, out) = call(&r, "GET", &format!("/api/board/item/{id}/run"), None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["state"], "failed");
        assert_eq!(
            out["lines"],
            json!(["— ход запущен —", "· bash cargo test", "! тишина 300 с — ход остановлен"]),
            "the persisted tail is served verbatim"
        );
        assert_eq!(out["truncated"], false);
        assert_eq!(out["duration_s"], 100.0);

        // reset wipes the tail along with the rest of the delegation columns
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/delegate/reset"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let stored: Option<String> = crate::db::db_static()
            .lock()
            .query_row("SELECT delegation_tail FROM items WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert!(stored.is_none(), "reset must clear the persisted tail");
        let (_, out) = call(&r, "GET", &format!("/api/board/item/{id}/run"), None).await;
        assert_eq!(out["lines"], json!([]));
    }

    /// `needs-feedback` is terminal-until-answered: a fresh delegate refuses,
    /// accept refuses, cancel says nothing runs — and reject (the answer
    /// channel) re-queues the item with the feedback in the next brief.
    #[tokio::test]
    async fn needs_feedback_answers_through_reject_only() {
        let r = test_router();
        let id = seed_board_item("воркер спросил", Some("lamplife"), Some("бриф"));
        set_delegation(id, "needs-feedback", Some("orchestrator"), Some("/tmp/nope"));

        // a fresh delegation refuses: the questions come first
        for target in ["orchestrator", "worktree", "dir"] {
            let (st, out) = call(
                &r,
                "POST",
                &format!("/api/board/item/{id}/delegate"),
                Some(&format!(r#"{{"target":"{target}"}}"#)),
            )
            .await;
            assert_eq!(st, StatusCode::CONFLICT, "target {target}: {out}");
            assert!(
                out["detail"].as_str().unwrap().contains("ждёт ответа"),
                "{out}"
            );
        }

        // accept refuses — questions are not work
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/accept"),
            Some(r#"{"merge":false}"#),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{out}");
        assert!(out["detail"].as_str().unwrap().contains("не на ревью"), "{out}");

        // nothing runs → cancel is a 409, the state stays
        let (st, _) = call(&r, "POST", &format!("/api/board/item/{id}/delegate/cancel"), None).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(board_delegation(&r, id).await["state"], "needs-feedback");

        // reject IS the answer channel: feedback stored, item back to queued,
        // the next brief carries it in «Фидбек»
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/reject"),
            Some(r#"{"feedback":"КВ — это календарная неделя"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let d = board_delegation(&r, id).await;
        assert_eq!(d["state"], "queued");
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/delegate/preview"),
            Some(r#"{"target":"dir"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert!(
            out["brief"].as_str().unwrap().contains("## Фидбек к прошлой попытке\nКВ — это календарная неделя\n"),
            "{out}"
        );

        // reset also works from needs-feedback (an explicit «забыть вопросы»)
        set_delegation(id, "needs-feedback", Some("orchestrator"), Some("/tmp/nope"));
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/delegate/reset"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(board_delegation(&r, id).await, Value::Null);
    }

    /// `failed` revives through the same reject: feedback stored, item back to
    /// `queued`, the next preview carries «Фидбек к прошлой попытке» AND the
    /// failed run's recorded session (`resume`) — the re-delegate continues
    /// the worker's own session. A live `running` run still refuses, and an
    /// item that was never delegated answers honestly instead of re-queuing.
    #[tokio::test]
    async fn failed_revives_through_reject_and_resumes_its_session() {
        let r = test_router();
        let id = seed_board_item("упал", Some("lamplife"), Some("бриф"));
        // the failed run's own session file: db_start wrote it, db_finish kept it
        let session = std::env::temp_dir().join(format!(
            "atlas-deleg-failed-test-r0-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(&session, "{\"dup\":\"session\"}").unwrap();
        {
            let conn = crate::db::db_static().lock();
            conn.execute(
                "UPDATE items SET delegation_state='failed', delegation_target='dir', \
                 delegation_ref='/tmp/nope', delegation_session=?1 WHERE id=?2",
                rusqlite::params![session.to_string_lossy(), id],
            )
            .expect("seed failed delegation");
        }

        // reject IS the revival: feedback stored, item back to queued
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/reject"),
            Some(r#"{"feedback":"продолжай"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let d = board_delegation(&r, id).await;
        assert_eq!(d["state"], "queued");
        assert_eq!(d["target"], "dir");

        // the next preview compiles the feedback AND the resume session
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/delegate/preview"),
            Some(r#"{"target":"dir"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let brief = out["brief"].as_str().unwrap();
        assert!(
            brief.contains("## Фидбек к прошлой попытке\nпродолжай\n"),
            "{brief}"
        );
        assert_eq!(out["resume"], session.to_string_lossy().as_ref(), "{out}");

        // a live run still refuses
        set_delegation(id, "running", Some("dir"), Some("/tmp/nope"));
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/reject"),
            Some(r#"{"feedback":"x"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{out}");
        assert!(
            out["detail"].as_str().unwrap().contains("сначала отмени"),
            "{out}"
        );

        // never delegated → honest error, not a silent re-queue
        let fresh = seed_board_item("не делегировали", Some("lamplife"), None);
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{fresh}/reject"),
            Some(r#"{"feedback":"x"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        assert!(
            out["detail"].as_str().unwrap().contains("делегации не было"),
            "{out}"
        );
        assert_eq!(board_delegation(&r, fresh).await, Value::Null);
        let _ = std::fs::remove_file(&session);
    }

    /// delegate/reset also works from `queued`: a rejected item returns to
    /// the backlog (`delegation:null`) without a worker ever running.
    #[tokio::test]
    async fn delegate_reset_clears_a_queued_item() {
        let r = test_router();
        let id = seed_board_item("сброс из очереди", Some("lamplife"), None);
        set_delegation(
            id,
            "queued",
            Some("orchestrator"),
            Some("/tmp/atlas-reset-queued"),
        );
        let (st, out) = call(
            &r,
            "POST",
            &format!("/api/board/item/{id}/delegate/reset"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(out["state"], "", "{out}");
        assert_eq!(board_delegation(&r, id).await, Value::Null);
    }
}
