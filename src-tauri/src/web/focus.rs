//! Focus («Этап А» buttons): `POST /api/focus` — H/T/W targets over herdr
//! workspaces/tabs or an atlas terminal, with dry-run planning; plus the
//! herdr control panel endpoints (`GET /api/herdr`, `POST /api/herdr/focus`).

use super::projects::proj_row;
use super::{db_lock, herdr_snapshot_ok, join_result, ok_json, persist_ws, ApiError};
use crate::cfg;
use crate::focus;
use crate::importers;
use crate::term;
use crate::util::{run_cmd, run_out, short};
use axum::response::Response;
use serde_json::{json, Value};
use std::time::Duration;


/// Focus targets supported by this build. `herdr tab create --workspace` exists
/// (verified against `herdr tab create --help`), so herdr_tab can target the
/// project's own workspace and stays; see GET /api/focus/targets. W is the
/// atlas terminal session (`proj-<name>`), O is gone — the orchestrator runs
/// across the whole fleet and lives in its own Оркестратор panel (rpc chat).
const FOCUS_TARGETS: [(&str, &str, &str); 3] = [
    ("herdr_workspace", "H", "herdr workspace"),
    ("herdr_tab", "T", "herdr tab"),
    ("terminal", "W", "терминал atlas"),
];

pub(crate) async fn api_focus_targets() -> Result<Response, ApiError> {
    let targets: Vec<Value> = FOCUS_TARGETS
        .iter()
        .map(|(target, letter, label)| json!({"target": target, "letter": letter, "label": label}))
        .collect();
    Ok(ok_json(json!({"targets": targets})))
}

struct FocusRow {
    name: String,
    path: Option<String>,
    ws: Option<String>,
}

fn focus_resp(
    ok: bool,
    plan: focus::Plan,
    reused: bool,
    dry: bool,
    argv: &[String],
    detail: String,
    steps: Vec<Value>,
) -> Value {
    json!({
        "ok": ok,
        "detail": detail,
        "plan": plan.as_str(),
        "argv": argv,
        "reused": reused,
        "dry_run": dry,
        "steps": steps,
    })
}

/// One plan step for the response: `{"desc","argv"}`, with `"read_only":true`
/// added for discovery steps. Mutating steps carry no flag.
fn step_json(s: &focus::Step) -> Value {
    let mut v = json!({"desc": s.desc, "argv": s.argv});
    if s.read_only {
        v["read_only"] = Value::Bool(true);
    }
    v
}

fn ro_step_json(desc: &str, argv: Vec<String>) -> Value {
    json!({"desc": desc, "argv": argv, "read_only": true})
}

/// dry-run detail: the mutating commands that WOULD run, nothing executed.
fn dry_detail_steps(steps: &[focus::Step], reason: &str) -> String {
    let muts: Vec<String> = steps
        .iter()
        .filter(|s| !s.read_only && !s.argv.is_empty())
        .map(|s| format!("$ {}", s.argv.join(" ")))
        .collect();
    let mut s = muts.join(" | ");
    if !s.is_empty() {
        s.push_str(" → ");
    }
    s.push_str(if muts.len() > 1 {
        "(dry-run: команды не запускались"
    } else {
        "(dry-run: команда не запускалась"
    });
    if !reason.is_empty() {
        s.push_str(&format!("; {reason}"));
    }
    s.push(')');
    s
}

fn herdr_session_alive() -> (bool, Option<u32>) {
    match term::session_state(term::HERDR_SESSION_ID) {
        Some(s) => (s.alive, s.pid),
        None => (false, None),
    }
}

/// Substitute the plan placeholder `{new_ws_id}` in a step's argv from the
/// output of the previous mutating step: it is parsed from the
/// `herdr workspace create` output. None = not determinable.
async fn subst_placeholders(argv: &[String], prev_out: &str) -> Option<Vec<String>> {
    if !argv.iter().any(|a| a == focus::NEW_WS) {
        return Some(argv.to_vec());
    }
    let ws = focus::parse_created_ws_id(prev_out)?;
    Some(
        argv.iter()
            .map(|a| if a == focus::NEW_WS { ws.clone() } else { a.clone() })
            .collect(),
    )
}

/// Execute the mutating steps of a focus plan in order. Read-only steps are
/// skipped (the handler already ran them to decide). A step flagged
/// `atlas_session` is spawned through the terminal session registry
/// (`term::ensure_info` — a PTY the panel attaches to), not as a bare
/// subprocess. A failed optional step is a warning, not a failure. When
/// `project` is set and a `herdr workspace create` step succeeds, the parsed
/// id is persisted so the NEXT click focuses instead of creating a duplicate.
async fn exec_focus_steps(steps: &[focus::Step], project: Option<&str>) -> (bool, String) {
    let mut details: Vec<String> = Vec::new();
    let mut ok = true;
    let mut prev_out = String::new();
    for step in steps.iter().filter(|s| !s.read_only) {
        if step.argv.is_empty() {
            continue;
        }
        // the herdr client step: ensure the atlas terminal session `herdr`
        if step.atlas_session {
            let spec = term::herdr_spec();
            let spawned = tokio::task::spawn_blocking(move || term::ensure_info(&spec)).await;
            match spawned {
                Ok(Ok(info)) => {
                    details.push(format!(
                        "$ [сессия терминала {}] → {}",
                        info.id,
                        info.pid
                            .map(|p| format!("pid {p}, alive"))
                            .unwrap_or_else(|| "запущено".into())
                    ));
                    prev_out.clear();
                    continue;
                }
                Ok(Err(e)) => {
                    details.push(format!("$ [сессия терминала herdr] → ошибка: {e}"));
                    ok = false;
                    break;
                }
                Err(e) => {
                    details.push(format!("$ [сессия терминала herdr] → ошибка: {e}"));
                    ok = false;
                    break;
                }
            }
        }
        let argv = match subst_placeholders(&step.argv, &prev_out).await {
            Some(a) => a,
            None => {
                if step.argv.iter().any(|a| a == focus::NEW_WS) {
                    details.push("нет id созданного воркспейса — следующий шаг пропущен".into());
                    ok = false;
                    break;
                }
                details.push(format!("{}: id не определён — шаг пропущен", step.desc));
                continue;
            }
        };
        let argv_str = argv.join(" ");
        let (sok, out) = run_cmd(&argv, Duration::from_secs(10)).await;
        details.push(format!("$ {argv_str} → {}", short(&out, 400)));
        prev_out = out;
        // learn the created workspace id so the NEXT click reuses it
        if sok && step.argv.len() >= 3 && step.argv[1] == "workspace" && step.argv[2] == "create" {
            match focus::parse_created_ws_id(&prev_out) {
                Some(ws) => {
                    if let Some(name) = project {
                        match persist_ws(name, &ws) {
                            Ok(()) => details.push(format!("воркспейс {ws} создан, id сохранён")),
                            Err(e) => details.push(format!(
                                "воркспейс {ws} создан, но id не сохранён: {e}"
                            )),
                        }
                    }
                }
                None => details.push(
                    "не удалось распознать workspace_id из вывода — следующий клик может создать дубликат".into(),
                ),
            }
        }
        if !sok {
            ok = false;
            break;
        }
    }
    (ok, details.join(" | "))
}

async fn focus_herdr_workspace(row: &FocusRow, dry: bool) -> Value {
    let herdr = importers::import_herdr().await;
    let snap_ok = herdr_snapshot_ok(&herdr);
    let (alive, _) = herdr_session_alive();
    let p = focus::herdr_workspace_plan(
        if snap_ok { Some(&herdr) } else { None },
        alive,
        row.path.as_deref(),
        row.ws.as_deref(),
    );
    let steps: Vec<Value> = p.steps.iter().map(step_json).collect();
    let mut v = if p.plan == focus::Plan::None {
        focus_resp(false, focus::Plan::None, false, dry, &[], p.reason, steps)
    } else if dry {
        focus_resp(
            true,
            p.plan,
            p.reused,
            dry,
            &p.argv,
            dry_detail_steps(&p.steps, &p.reason),
            steps,
        )
    } else {
        let (ok, detail) = exec_focus_steps(&p.steps, Some(&row.name)).await;
        let mut detail = detail;
        if ok {
            if let Some(ws) = &p.persist {
                match persist_ws(&row.name, ws) {
                    Ok(()) => detail.push_str(&format!(" | сохранён herdr_workspace_id={ws}")),
                    Err(e) => detail.push_str(&format!(" | не удалось сохранить workspace id: {e}")),
                }
            }
        }
        focus_resp(ok, p.plan, p.reused, dry, &p.argv, detail, steps)
    };
    // the visible herdr client is the atlas terminal session: the frontend
    // opens the Terminal panel on it
    v["session"] = json!(term::HERDR_SESSION_ID);
    v
}

async fn focus_herdr_tab(row: &FocusRow, dry: bool) -> Value {
    let herdr = importers::import_herdr().await;
    let snap_ok = herdr_snapshot_ok(&herdr);
    let list_argv = vec![cfg::HERDR_BIN.clone(), "tab".into(), "list".into()];
    let list_str = list_argv.join(" ");
    let mut steps: Vec<Value> = vec![ro_step_json(
        "снимок живого состояния herdr",
        vec![cfg::HERDR_BIN.clone(), "api".into(), "snapshot".into()],
    )];
    steps.push(ro_step_json("список вкладок herdr", list_argv.clone()));
    if !snap_ok {
        return focus_resp(
            false,
            focus::Plan::None,
            false,
            dry,
            &[],
            "herdr api snapshot недоступен — живое состояние herdr неизвестно".into(),
            steps,
        );
    }
    let tabs = match run_out(&list_argv, Duration::from_secs(10)).await {
        Some(out) => focus::herdr_tabs(&out),
        None => {
            return focus_resp(
                false,
                focus::Plan::None,
                false,
                dry,
                &[],
                format!("$ {list_str} → failed"),
                steps,
            );
        }
    };
    let (alive, _) = herdr_session_alive();
    let p = focus::herdr_tab_plan(
        Some(&herdr),
        &tabs,
        alive,
        row.path.as_deref(),
        row.ws.as_deref(),
        &row.name,
    );
    let steps: Vec<Value> = p.steps.iter().map(step_json).collect();
    let mut v = if p.plan == focus::Plan::None {
        focus_resp(false, focus::Plan::None, false, dry, &[], p.reason, steps)
    } else if dry {
        focus_resp(
            true,
            p.plan,
            p.reused,
            dry,
            &p.argv,
            dry_detail_steps(&p.steps, &p.reason),
            steps,
        )
    } else {
        let (ok, mut detail) = exec_focus_steps(&p.steps, Some(&row.name)).await;
        if ok && p.plan == focus::Plan::Focus {
            if let Some(ws) = &p.persist {
                if let Err(e) = persist_ws(&row.name, ws) {
                    detail.push_str(&format!(" | не удалось сохранить workspace id: {e}"));
                }
            }
        }
        focus_resp(ok, p.plan, p.reused, dry, &p.argv, detail, steps)
    };
    // the visible herdr client is the atlas terminal session: the frontend
    // opens the Terminal panel on it
    v["session"] = json!(term::HERDR_SESSION_ID);
    v
}

/// W: focus the project's atlas terminal session — reuse the live one, or
/// spawn `$SHELL -l` in the project path right now (plan `create`). A project
/// without a path has no directory to run a shell in → 422. The session is a
/// server-owned PTY: the panel attaches to it over WS, nothing is focused
/// on the desktop.
async fn focus_terminal(row: &FocusRow, dry: bool) -> Result<Value, ApiError> {
    let Some(path) = row.path.as_deref().map(str::trim).filter(|p| !p.is_empty()) else {
        return Err(ApiError::unprocessable(format!(
            "у проекта '{}' нет пути — сессию терминала открыть нельзя",
            row.name
        )));
    };
    let spec = term::proj_spec(&row.name, std::path::Path::new(path));
    let alive = term::session_state(&spec.id).map(|s| s.alive).unwrap_or(false);
    let steps: Vec<focus::Step> = if alive {
        vec![focus::Step::ro(
            &format!("сессия {} уже запущена — переиспользуем", spec.id),
            vec![],
        )]
    } else {
        vec![focus::Step::mv(
            &format!("запустить сессию {} (cwd {path})", spec.id),
            spec.argv.clone(),
        )]
    };
    let step_jsons: Vec<Value> = steps.iter().map(step_json).collect();
    let argv = steps
        .iter()
        .find(|s| !s.read_only)
        .map(|s| s.argv.clone())
        .unwrap_or_default();
    if alive {
        let mut v = focus_resp(
            true,
            focus::Plan::Focus,
            true,
            dry,
            &[],
            format!("сессия {} уже запущена", spec.id),
            step_jsons,
        );
        v["session"] = json!(spec.id);
        return Ok(v);
    }
    if dry {
        let mut v = focus_resp(
            true,
            focus::Plan::Create,
            false,
            dry,
            &argv,
            dry_detail_steps(&steps, "сессия ещё не создана"),
            step_jsons,
        );
        v["session"] = json!(spec.id);
        return Ok(v);
    }
    let session_id = spec.id.clone();
    let spawned = tokio::task::spawn_blocking(move || term::ensure_info(&spec))
        .await
        .map_err(join_result)?;
    let mut v = match spawned {
        Ok(info) => focus_resp(
            true,
            focus::Plan::Create,
            false,
            dry,
            &argv,
            format!(
                "сессия {} запущена{}",
                info.id,
                info.pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
            ),
            step_jsons,
        ),
        Err(e) => focus_resp(
            false,
            focus::Plan::None,
            false,
            dry,
            &[],
            format!("не удалось запустить сессию: {e}"),
            step_jsons,
        ),
    };
    v["session"] = json!(session_id);
    Ok(v)
}

pub(crate) async fn api_focus(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let project = v
        .get("project")
        .and_then(|p| p.as_str())
        .ok_or_else(|| ApiError::unprocessable("project must be a string"))?
        .to_string();
    let supported = FOCUS_TARGETS
        .iter()
        .map(|t| t.0)
        .collect::<Vec<_>>()
        .join(", ");
    let target = v
        .get("target")
        .and_then(|t| t.as_str())
        .ok_or_else(|| ApiError::unprocessable(format!("target must be one of {supported}")))?
        .to_string();
    if !FOCUS_TARGETS.iter().any(|t| t.0 == target) {
        return Err(ApiError::unprocessable(format!(
            "unknown target '{target}'; supported: {supported}"
        )));
    }
    let dry = v.get("dry_run").and_then(|d| d.as_bool()).unwrap_or(false);

    let row = {
        let project = project.clone();
        tokio::task::spawn_blocking(move || -> Result<FocusRow, ApiError> {
            let conn = db_lock();
            proj_row(&conn, &project)
                .map(|p| FocusRow {
                    name: p.name,
                    path: p.path,
                    ws: p.ws,
                })
                .ok_or_else(|| ApiError::not_found(format!("unknown project '{project}'")))
        })
        .await
        .map_err(join_result)??
    };

    let out = match target.as_str() {
        "herdr_workspace" => focus_herdr_workspace(&row, dry).await,
        "herdr_tab" => focus_herdr_tab(&row, dry).await,
        _ => focus_terminal(&row, dry).await?,
    };
    Ok(ok_json(out))
}

// ----------------------------------------------------------------- herdr ---

/// GET /api/herdr — the herdr control panel: one `herdr api snapshot` (via the
/// importer) + the atlas `herdr` terminal session state (the visible client).
/// Server down → ok:false, server: false and nothing invented.
pub(crate) async fn api_herdr() -> Result<Response, ApiError> {
    let herdr = importers::import_herdr().await;
    let (alive, pid) = if herdr_snapshot_ok(&herdr) {
        herdr_session_alive()
    } else {
        (false, None)
    };
    Ok(ok_json(focus::herdr_panel(&herdr, alive, pid)))
}

/// POST /api/herdr/focus — `{"kind":"workspace"|"tab","id":"…"}`: ensure the
/// atlas `herdr` terminal session (the visible client, same lazy spawn as
/// H/T), then focus the object. Server down → ok:false and NOTHING is
/// spawned, not even the client session.
pub(crate) async fn api_herdr_focus(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    if kind != "workspace" && kind != "tab" {
        return Err(ApiError::unprocessable("kind must be 'workspace' or 'tab'"));
    }
    let id = v
        .get("id")
        .and_then(|i| i.as_str())
        .map(str::trim)
        .filter(|i| !i.is_empty())
        .ok_or_else(|| ApiError::unprocessable("id must be a non-empty string"))?;

    let herdr = importers::import_herdr().await;
    if !herdr_snapshot_ok(&herdr) {
        return Ok(ok_json(json!({
            "ok": false,
            "detail": "herdr сервер недоступен",
        })));
    }
    let (alive, _) = herdr_session_alive();
    // `reused` answers "was the client session reused?" — the ensure-session
    // decision, exactly like H/T
    let reused = alive;
    let mut steps: Vec<focus::Step> = Vec::new();
    steps.push(focus::herdr_session_step(alive));
    let focus_argv = vec![
        cfg::HERDR_BIN.clone(),
        kind.to_string(),
        "focus".into(),
        id.to_string(),
    ];
    steps.push(focus::Step::mv(
        &format!("фокус {kind} {id}"),
        focus_argv.clone(),
    ));
    let (ok, detail) = exec_focus_steps(&steps, None).await;
    Ok(ok_json(json!({
        "ok": ok,
        "detail": detail,
        "argv": focus_argv,
        "reused": reused,
        "session": term::HERDR_SESSION_ID,
    })))
}

