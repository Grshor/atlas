//! Focus decisions — pure logic over parsed subprocess output, no side effects.
//! The HTTP layer (http.rs) runs the read-only discovery commands, applies a
//! decision here, and only then executes the mutating steps of the plan.
//!
//! Plan vocabulary (POST /api/focus `plan`): Focus = activate existing target
//! (`reused:true`), Create = spawn something new, None = nothing to do.
//!
//! Targets: H/T are herdr-backed (workspace/tab) and W is the atlas terminal
//! session (`proj-<name>`), decided in http.rs (no subprocess involved).
//! herdr has no windows of its own: its visible client is the atlas terminal
//! session `herdr` (see term.rs), which H/T ensure before touching the
//! workspace/tab — wezterm is not used anywhere any more.
use crate::cfg;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
/// Placeholder for the workspace id that only exists after the first mutating
/// step (`herdr workspace create`) has run; the HTTP layer substitutes it.
pub const NEW_WS: &str = "{new_ws_id}";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Focus,
    Create,
    None,
}
impl Plan {
    pub fn as_str(self) -> &'static str {
        match self {
            Plan::Focus => "focus",
            Plan::Create => "create",
            Plan::None => "none",
        }
    }
}
fn herdr_bin() -> String {
    cfg::HERDR_BIN.clone()
}

/// One step of a focus plan: a full argv, no shell. Read-only discovery steps
/// are never executed by the HTTP layer (it ran them to decide). A step with
/// `atlas_session: true` is NOT a subprocess: the HTTP layer spawns it through
/// the terminal session registry (a PTY the panel attaches to) — used only by
/// the herdr client step.
pub struct Step {
    pub desc: String,
    pub argv: Vec<String>,
    pub read_only: bool,
    pub atlas_session: bool,
}
impl Step {
    pub fn ro(desc: &str, argv: Vec<String>) -> Step {
        Step { desc: desc.into(), argv, read_only: true, atlas_session: false }
    }
    pub fn mv(desc: &str, argv: Vec<String>) -> Step {
        Step { desc: desc.into(), argv, read_only: false, atlas_session: false }
    }
}

/// First mutating step of a plan — the response's `argv` contract.
fn first_mutating(steps: &[Step]) -> Vec<String> {
    steps
        .iter()
        .find(|s| !s.read_only)
        .map(|s| s.argv.clone())
        .unwrap_or_default()
}

/// Full plan for the herdr-backed targets (H/T): discovery (read-only) +
/// ensure-client + workspace/tab steps.
pub struct HerdrPlan {
    pub plan: Plan,
    pub reused: bool,
    /// First mutating step (ensure-client when it runs, else the herdr step).
    pub argv: Vec<String>,
    /// FULL plan: read-only discovery steps + mutating steps, in order.
    pub steps: Vec<Step>,
    /// workspace_id to store into projects.herdr_workspace_id (Focus branch).
    pub persist: Option<String>,
    pub reason: String,
}
// ------------------------------------------------------------------ herdr ---
/// workspace_id of the agent's workspace (`pane_id` is `<ws>:<pane>`).
fn agent_ws(a: &Value) -> String {
    a.get("pane_id")
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_string()
}
/// Map workspace_id -> agent cwds from the snapshot (`herdr api snapshot`,
/// import_herdr shape: {ok, workspaces, agents}). Workspaces carry no cwd, so
/// path evidence comes from the agents running inside them.
fn ws_cwds(herdr: &Value) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for a in herdr
        .get("agents")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
    {
        let Some(cwd) = a.get("cwd").and_then(|c| c.as_str()) else {
            continue;
        };
        if cwd.is_empty() {
            continue;
        }
        let ws = agent_ws(a);
        if ws.is_empty() {
            continue;
        }
        out.entry(ws)
            .or_default()
            .push(cwd.trim_end_matches('/').to_string());
    }
    out
}
/// Workspace whose agent cwd matches `path`: exact match first, then prefix on
/// the trailing `/` boundary (same rule as wezterm pane matching).
pub fn herdr_ws_for_path(herdr: &Value, path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    let cwds = ws_cwds(herdr);
    let live: Vec<&str> = herdr
        .get("workspaces")
        .and_then(|w| w.as_array())
        .into_iter()
        .flatten()
        .filter_map(|w| w.get("workspace_id").and_then(|v| v.as_str()))
        .collect();
    let hit = |pred: &dyn Fn(&str) -> bool| -> Option<String> {
        for w in &live {
            if let Some(list) = cwds.get(*w) {
                if list.iter().any(|c| pred(c)) {
                    return Some((*w).to_string());
                }
            }
        }
        None
    };
    hit(&|c| c == path).or_else(|| hit(&|c| c.starts_with(&format!("{path}/"))))
}
pub fn herdr_ws_exists(herdr: &Value, ws: &str) -> bool {
    herdr
        .get("workspaces")
        .and_then(|w| w.as_array())
        .into_iter()
        .flatten()
        .any(|w| w.get("workspace_id").and_then(|v| v.as_str()) == Some(ws))
}
pub struct HerdrWsDecision {
    pub plan: Plan,
    /// Target workspace id (Focus) — the HTTP layer persists it.
    pub ws: Option<String>,
    /// First mutating command (Focus: `workspace focus`, Create: `workspace create`).
    pub argv: Vec<String>,
    /// workspace_id to store into projects.herdr_workspace_id.
    pub persist: Option<String>,
    pub reason: String,
}
/// H-button decision: live snapshot path match → persisted live id → create.
/// The create branch persists the parsed id so the NEXT click focuses instead
/// of creating a duplicate.
pub fn herdr_workspace_decision(
    herdr: &Value,
    path: Option<&str>,
    persisted: Option<&str>,
) -> HerdrWsDecision {
    let path = path.map(str::trim).filter(|p| !p.is_empty());
    if let Some(p) = path {
        if let Some(ws) = herdr_ws_for_path(herdr, p) {
            return HerdrWsDecision {
                plan: Plan::Focus,
                ws: Some(ws.clone()),
                persist: Some(ws.clone()),
                argv: vec![herdr_bin(), "workspace".into(), "focus".into(), ws.clone()],
                reason: "найден живой воркспейс по пути проекта".into(),
            };
        }
    }
    if let Some(ws) = persisted
        .map(str::trim)
        .filter(|w| !w.is_empty() && herdr_ws_exists(herdr, w))
    {
        return HerdrWsDecision {
            plan: Plan::Focus,
            persist: Some(ws.to_string()),
            argv: vec![
                herdr_bin(),
                "workspace".into(),
                "focus".into(),
                ws.to_string(),
            ],
            ws: Some(ws.to_string()),
            reason: "переиспользован сохранённый herdr_workspace_id".into(),
        };
    }
    if let Some(p) = path {
        return HerdrWsDecision {
            plan: Plan::Create,
            ws: None,
            persist: None, // parsed from `workspace create` output at runtime
            argv: vec![
                herdr_bin(),
                "workspace".into(),
                "create".into(),
                "--cwd".into(),
                p.to_string(),
                "--focus".into(),
            ],
            reason: "подходящего воркспейса нет — будет создан".into(),
        };
    }
    HerdrWsDecision {
        plan: Plan::None,
        ws: None,
        persist: None,
        argv: vec![],
        reason: "у проекта нет пути и живого herdr-воркспейса".into(),
    }
}

/// Parse `herdr tab list` output (JSON envelope {"result":{"tabs":[...]}}).
pub fn herdr_tabs(out: &str) -> Vec<Value> {
    out.find('{')
        .and_then(|i| serde_json::from_str::<Value>(&out[i..]).ok())
        .and_then(|d| {
            d.get("result")
                .and_then(|r| r.get("tabs"))
                .and_then(|t| t.as_array())
                .cloned()
        })
        .unwrap_or_default()
}
/// Tab of `ws` labelled `label` (a tab this focus flow created earlier).
pub fn tab_in_ws(tabs: &[Value], ws: &str, label: &str) -> Option<String> {
    tabs.iter()
        .filter(|t| t.get("workspace_id").and_then(|v| v.as_str()) == Some(ws))
        .find(|t| t.get("label").and_then(|v| v.as_str()) == Some(label))
        .and_then(|t| t.get("tab_id").and_then(|v| v.as_str()).map(String::from))
}
/// The workspace's own active tab (`active_tab_id` on the workspace entry).
pub fn ws_active_tab(herdr: &Value, ws: &str) -> Option<String> {
    herdr
        .get("workspaces")
        .and_then(|w| w.as_array())
        .into_iter()
        .flatten()
        .find(|w| w.get("workspace_id").and_then(|v| v.as_str()) == Some(ws))
        .and_then(|w| {
            w.get("active_tab_id")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
}
pub struct HerdrTabDecision {
    pub plan: Plan,
    /// Mutating sequence; may contain NEW_WS in the step after ws create.
    pub steps: Vec<Vec<String>>,
    pub persist: Option<String>,
    pub reused: bool,
    pub reason: String,
}
/// T-button decision: resolve the project's workspace exactly like H, then act
/// INSIDE it — focus the project-labelled tab, else the workspace's active
/// tab, else create a labelled tab in that workspace (`tab create --workspace`).
pub fn herdr_tab_decision(
    herdr: &Value,
    tabs: &[Value],
    path: Option<&str>,
    persisted: Option<&str>,
    name: &str,
) -> HerdrTabDecision {
    let ws_dec = herdr_workspace_decision(herdr, path, persisted);
    if ws_dec.plan == Plan::None {
        return HerdrTabDecision {
            plan: Plan::None,
            steps: vec![],
            persist: None,
            reused: false,
            reason: ws_dec.reason,
        };
    }
    if ws_dec.plan == Plan::Create {
        // Workspace will be created first; the tab goes into it (NEW_WS is
        // substituted by the HTTP layer after parsing the create output).
        let mut tab_create = vec![
            herdr_bin(),
            "tab".into(),
            "create".into(),
            "--workspace".into(),
            NEW_WS.to_string(),
        ];
        if let Some(p) = path.map(str::trim).filter(|p| !p.is_empty()) {
            tab_create.push("--cwd".into());
            tab_create.push(p.to_string());
        }
        tab_create.push("--label".into());
        tab_create.push(name.to_string());
        tab_create.push("--focus".into());
        return HerdrTabDecision {
            plan: Plan::Create,
            steps: vec![ws_dec.argv.clone(), tab_create],
            persist: None,
            reused: false,
            reason: "воркспейс будет создан, в нём — вкладка проекта".into(),
        };
    }
    let ws = ws_dec.ws.unwrap_or_default();
    if let Some(tid) = tab_in_ws(tabs, &ws, name) {
        return HerdrTabDecision {
            plan: Plan::Focus,
            persist: ws_dec.persist,
            reused: true,
            steps: vec![vec![herdr_bin(), "tab".into(), "focus".into(), tid]],
            reason: "в воркспейсе уже есть вкладка проекта".into(),
        };
    }
    if let Some(tid) = ws_active_tab(herdr, &ws) {
        return HerdrTabDecision {
            plan: Plan::Focus,
            persist: ws_dec.persist,
            reused: true,
            steps: vec![vec![herdr_bin(), "tab".into(), "focus".into(), tid]],
            reason: "фокус на активную вкладку воркспейса проекта".into(),
        };
    }
    if let Some(p) = path.map(str::trim).filter(|p| !p.is_empty()) {
        return HerdrTabDecision {
            plan: Plan::Create,
            persist: ws_dec.persist,
            reused: false,
            steps: vec![vec![
                herdr_bin(),
                "tab".into(),
                "create".into(),
                "--workspace".into(),
                ws.clone(),
                "--cwd".into(),
                p.to_string(),
                "--label".into(),
                name.to_string(),
                "--focus".into(),
            ]],
            reason: "воркспейс пуст — будет создана вкладка проекта".into(),
        };
    }
    HerdrTabDecision {
        plan: Plan::None,
        steps: vec![],
        persist: ws_dec.persist,
        reused: false,
        reason: "воркспейс пуст, а у проекта нет пути".into(),
    }
}
/// Parse the created workspace id from `herdr workspace create` output:
/// JSON envelope first (result.workspace.workspace_id | result.workspace_id),
/// then a bare `w<id>` token as a last resort.
pub fn parse_created_ws_id(out: &str) -> Option<String> {
    if let Some(i) = out.find('{') {
        if let Ok(d) = serde_json::from_str::<Value>(&out[i..]) {
            let r = d.get("result").unwrap_or(&Value::Null);
            for cand in [
                r.get("workspace").and_then(|w| w.get("workspace_id")),
                r.get("workspace_id"),
                d.get("workspace_id"),
            ]
            .into_iter()
            .flatten()
            {
                if let Some(s) = cand.as_str() {
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
        }
    }
    // Fallback: herdr workspace ids look like `w7`, `wQ`, `w15` — a `w`
    // followed by 1-3 alphanumerics as a standalone token ("workspace" fails).
    let mut token = String::new();
    for ch in out.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_alphanumeric() {
            token.push(ch);
        } else {
            if token.len() >= 2
                && token.len() <= 4
                && token.starts_with('w')
                && token[1..].chars().all(|c| c.is_ascii_alphanumeric())
            {
                return Some(token.clone());
            }
            token.clear();
        }
    }
    None
}
// ------------------------------------------------- herdr client (H/T) ---
/// The ensure-client step: the visible herdr client is the atlas terminal
/// session `herdr` (term.rs). Alive — nothing to do (read-only note);
/// otherwise it is spawned through the session registry (`atlas_session`
/// step — the HTTP layer runs `term::ensure_info`, never a bare subprocess:
/// the client needs a TTY and must survive panel close). herdr server down
/// never reaches this: the plans return None on a dead snapshot first.
pub fn herdr_session_step(alive: bool) -> Step {
    if alive {
        Step::ro(
            "клиент herdr уже запущен сессией терминала atlas — переиспользуем",
            vec![],
        )
    } else {
        Step {
            desc: "поднять клиент herdr сессией терминала atlas".into(),
            argv: vec![herdr_bin()],
            read_only: false,
            atlas_session: true,
        }
    }
}

/// Пульт herdr (`GET /api/herdr`): snapshot + the atlas `herdr` session state
/// composed into the panel shape. The client is the terminal session now, so
/// `attached` is its liveness and `pid` its PTY child — no ps/wezterm
/// discovery. When the snapshot is not `ok` every field is honestly
/// false/null: the panel shows no fake data.
pub fn herdr_panel(herdr: &Value, session_alive: bool, session_pid: Option<u32>) -> Value {
    if !herdr.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
        return json!({
            "ok": false,
            "server": false,
            "workspaces": [],
            "client": {"attached": false, "session": "herdr", "pid": null},
        });
    }
    let focused_ws = herdr.get("focused_workspace_id").and_then(|v| v.as_str());
    let focused_tab = herdr.get("focused_tab_id").and_then(|v| v.as_str());
    let empty: Vec<Value> = Vec::new();
    let ws_list = herdr.get("workspaces").and_then(|v| v.as_array()).unwrap_or(&empty);
    let tab_list = herdr.get("tabs").and_then(|v| v.as_array()).unwrap_or(&empty);
    let agents = herdr.get("agents").and_then(|v| v.as_array()).unwrap_or(&empty);

    let workspaces: Vec<Value> = ws_list
        .iter()
        .map(|w| {
            let id = w.get("workspace_id").and_then(|v| v.as_str()).unwrap_or("");
            let focused = w
                .get("focused")
                .and_then(|v| v.as_bool())
                .unwrap_or_else(|| focused_ws == Some(id));
            // agents grouped by status, deterministic order (status name)
            let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
            for a in agents {
                if agent_ws(a) != id {
                    continue;
                }
                let st = a.get("agent_status").and_then(|v| v.as_str()).unwrap_or("unknown");
                *counts.entry(st).or_insert(0) += 1;
            }
            let agents_json: Vec<Value> = counts
                .into_iter()
                .map(|(status, count)| json!({"status": status, "count": count}))
                .collect();
            let tabs: Vec<Value> = tab_list
                .iter()
                .filter(|t| t.get("workspace_id").and_then(|v| v.as_str()) == Some(id))
                .map(|t| {
                    let tid = t.get("tab_id").and_then(|v| v.as_str()).unwrap_or("");
                    let tfocused = t
                        .get("focused")
                        .and_then(|v| v.as_bool())
                        .unwrap_or_else(|| focused_tab == Some(tid));
                    json!({
                        "id": tid,
                        "label": t.get("label").and_then(|v| v.as_str()),
                        "focused": tfocused,
                        "pane_count": t.get("pane_count").and_then(|v| v.as_i64()).unwrap_or(0),
                    })
                })
                .collect();
            json!({
                "id": id,
                "label": w.get("label").and_then(|v| v.as_str()),
                "focused": focused,
                "agents": agents_json,
                "tabs": tabs,
            })
        })
        .collect();

    json!({
        "ok": true,
        "server": true,
        "workspaces": workspaces,
        "client": {
            "attached": session_alive,
            "session": "herdr",
            "pid": session_pid,
        },
    })
}

/// Full H plan: discovery (read-only) + ensure the atlas `herdr` session +
/// the workspace step. `herdr_session_alive` is the live state of the
/// terminal session `herdr` (term registry read; never spawns).
pub fn herdr_workspace_plan(
    herdr: Option<&Value>,
    herdr_session_alive: bool,
    path: Option<&str>,
    persisted: Option<&str>,
) -> HerdrPlan {
    let mut steps = vec![Step::ro(
        "снимок живого состояния herdr",
        vec![herdr_bin(), "api".into(), "snapshot".into()],
    )];
    let Some(herdr) = herdr else {
        // server down: honest failure, no spawn, no further discovery
        return HerdrPlan {
            plan: Plan::None,
            reused: false,
            argv: vec![],
            steps,
            persist: None,
            reason: "herdr api snapshot недоступен — живое состояние herdr неизвестно".into(),
        };
    };
    let dec = herdr_workspace_decision(herdr, path, persisted);
    if dec.plan == Plan::None {
        return HerdrPlan {
            plan: Plan::None,
            reused: false,
            argv: vec![],
            steps,
            persist: None,
            reason: dec.reason,
        };
    }
    steps.push(herdr_session_step(herdr_session_alive));
    steps.push(Step::mv("фокус воркспейса проекта", dec.argv.clone()));
    HerdrPlan {
        plan: dec.plan,
        reused: dec.plan == Plan::Focus,
        argv: first_mutating(&steps),
        steps,
        persist: dec.persist,
        reason: dec.reason,
    }
}

/// Full T plan: discovery (read-only) + ensure the atlas `herdr` session +
/// workspace/tab steps.
pub fn herdr_tab_plan(
    herdr: Option<&Value>,
    tabs: &[Value],
    herdr_session_alive: bool,
    path: Option<&str>,
    persisted: Option<&str>,
    name: &str,
) -> HerdrPlan {
    let mut steps = vec![
        Step::ro(
            "снимок живого состояния herdr",
            vec![herdr_bin(), "api".into(), "snapshot".into()],
        ),
        Step::ro("список вкладок herdr", vec![herdr_bin(), "tab".into(), "list".into()]),
    ];
    let Some(herdr) = herdr else {
        return HerdrPlan {
            plan: Plan::None,
            reused: false,
            argv: vec![],
            steps,
            persist: None,
            reason: "herdr api snapshot недоступен — живое состояние herdr неизвестно".into(),
        };
    };
    let dec = herdr_tab_decision(herdr, tabs, path, persisted, name);
    if dec.plan == Plan::None {
        return HerdrPlan {
            plan: Plan::None,
            reused: false,
            argv: vec![],
            steps,
            persist: None,
            reason: dec.reason,
        };
    }
    steps.push(herdr_session_step(herdr_session_alive));
    for step in &dec.steps {
        let desc = if step.len() >= 2 && step[1] == "workspace" {
            "создать воркспейс проекта"
        } else if step.len() >= 3 && step[2] == "create" {
            "создать вкладку проекта"
        } else {
            "фокус вкладки проекта"
        };
        steps.push(Step::mv(desc, step.clone()));
    }
    HerdrPlan {
        plan: dec.plan,
        reused: dec.reused,
        argv: first_mutating(&steps),
        steps,
        persist: dec.persist,
        reason: dec.reason,
    }
}

// ----------------------------------------------------------------- tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// import_herdr shape: workspaces carry no cwd; path evidence comes from
    /// the agents (pane_id `<ws>:<pane>`, cwd).
    fn fixture() -> Value {
        json!({
            "ok": true,
            "workspaces": [
                {"workspace_id": "w7", "label": "emigate", "active_tab_id": "w7:t1"},
                {"workspace_id": "w8", "label": "lamplife", "active_tab_id": "w8:t1"},
                {"workspace_id": "w14", "label": "finance", "active_tab_id": "w14:t2"},
                {"workspace_id": "w9", "label": "empty", "active_tab_id": null},
                {"workspace_id": "wN", "label": "herdctl", "active_tab_id": null}
            ],
            "agents": [
                {"pane_id": "w7:p1", "cwd": "/home/u/Projects/finance/emigate"},
                {"pane_id": "w8:p1", "cwd": "/home/u/Projects/apps/lamplife"},
                {"pane_id": "w14:p1", "cwd": "/home/u/Projects/finance"},
                {"pane_id": "w14:p2", "cwd": "/home/u/Projects/finance/sub"}
            ]
        })
    }

    #[test]
    fn ws_exact_path_beats_prefix() {
        // project "finance": w7 has a prefix match, w14 the exact one — exact wins
        let d = herdr_workspace_decision(
            &fixture(),
            Some("/home/u/Projects/finance"),
            None,
        );
        assert_eq!(d.plan, Plan::Focus);
        assert_eq!(d.ws.as_deref(), Some("w14"));
        assert!(d.argv.contains(&"w14".to_string()));
        assert_eq!(d.persist.as_deref(), Some("w14"));
    }

    #[test]
    fn ws_prefix_on_slash_boundary() {
        // project "emigate": agent cwd /Projects/finance/emigate matches exactly
        let d = herdr_workspace_decision(
            &fixture(),
            Some("/home/u/Projects/finance/emigate"),
            None,
        );
        assert_eq!(d.plan, Plan::Focus);
        assert_eq!(d.ws.as_deref(), Some("w7"));
    }

    #[test]
    fn ws_prefix_requires_full_segment() {
        // /home/u/Projects/financ must NOT match /home/u/Projects/finance
        let d = herdr_workspace_decision(&fixture(), Some("/home/u/Projects/financ"), None);
        assert_eq!(d.plan, Plan::Create);
        assert!(d.argv.contains(&"--cwd".to_string()));
        assert!(d.argv.contains(&"/home/u/Projects/financ".to_string()));
    }

    #[test]
    fn ws_no_match_creates_and_next_click_reuses() {
        // click 1: no workspace for herdctl → create...
        let empty = json!({"ok": true, "workspaces": [], "agents": []});
        let d1 = herdr_workspace_decision(
            &empty,
            Some("/home/u/Projects/herdctl"),
            None,
        );
        assert_eq!(d1.plan, Plan::Create);
        assert_eq!(d1.persist, None);
        assert_eq!(
            d1.argv,
            vec![
                cfg::HERDR_BIN.clone(),
                "workspace".to_string(),
                "create".to_string(),
                "--cwd".to_string(),
                "/home/u/Projects/herdctl".to_string(),
                "--focus".to_string(),
            ]
        );
        // ...the created id is parsed from the output and persisted...
        assert_eq!(parse_created_ws_id("{\"result\":{\"workspace\":{\"workspace_id\":\"wN\"}}}"), Some("wN".into()));
        // click 2: no path evidence (the fresh ws has no agents yet), but the
        // id was persisted → focus, never a second create
        let d2 = herdr_workspace_decision(
            &fixture(),
            Some("/home/u/Projects/herdctl"),
            Some("wN"),
        );
        assert_eq!(d2.plan, Plan::Focus);
        assert_eq!(d2.ws.as_deref(), Some("wN"));
        // once agents appear in the workspace, the path match alone suffices:
        let with_agent = json!({
            "ok": true,
            "workspaces": [{"workspace_id": "wN", "label": "herdctl", "active_tab_id": null}],
            "agents": [{"pane_id": "wN:p1", "cwd": "/home/u/Projects/herdctl"}]
        });
        let d2b = herdr_workspace_decision(&with_agent, Some("/home/u/Projects/herdctl"), None);
        assert_eq!(d2b.plan, Plan::Focus);
        assert_eq!(d2b.ws.as_deref(), Some("wN"));
        // same via the persisted id even when the agent vanished (empty ws):
        let d3 = herdr_workspace_decision(&fixture(), Some("/no/such/path"), Some("wN"));
        assert_eq!(d3.plan, Plan::Focus);
        assert_eq!(d3.ws.as_deref(), Some("wN"));
    }

    #[test]
    fn ws_stale_persisted_id_falls_back_to_create() {
        let d = herdr_workspace_decision(&fixture(), Some("/home/x"), Some("w99"));
        assert_eq!(d.plan, Plan::Create);
    }

    #[test]
    fn ws_no_path_and_no_live_ws_is_none() {
        let d = herdr_workspace_decision(&fixture(), None, Some("w99"));
        assert_eq!(d.plan, Plan::None);
        let d2 = herdr_workspace_decision(&fixture(), Some(""), None);
        assert_eq!(d2.plan, Plan::None);
    }

    #[test]
    fn ws_created_id_parsing() {
        // JSON envelope forms
        assert_eq!(
            parse_created_ws_id("{\"id\":\"cli:workspace:create\",\"result\":{\"workspace\":{\"workspace_id\":\"wZ\"}}}"),
            Some("wZ".into())
        );
        assert_eq!(
            parse_created_ws_id("{\"result\":{\"workspace_id\":\"wY\"}}"),
            Some("wY".into())
        );
        // plain text fallback
        assert_eq!(parse_created_ws_id("Created workspace wQ"), Some("wQ".into()));
        // no false positives on the word "workspace" or longer tokens
        assert_eq!(parse_created_ws_id("workspace created"), None);
    }

    #[test]
    fn tab_focuses_project_label_then_active_then_creates() {
        let tabs = json!([
            {"tab_id": "w7:t2", "workspace_id": "w7", "label": "emigate"},
            {"tab_id": "w7:t1", "workspace_id": "w7", "label": "1"},
            {"tab_id": "w8:t1", "workspace_id": "w8", "label": "emigate"}
        ]);
        // existing project tab inside w7 → focus it, not the one in w8
        let d = herdr_tab_decision(
            &fixture(),
            tabs.as_array().unwrap(),
            Some("/home/u/Projects/finance/emigate"),
            None,
            "emigate",
        );
        assert_eq!(d.plan, Plan::Focus);
        assert!(d.reused);
        assert!(d.steps[0].contains(&"w7:t2".to_string()));
        // no labelled tab → the workspace's active tab
        let d2 = herdr_tab_decision(
            &fixture(),
            tabs.as_array().unwrap(),
            Some("/home/u/Projects/finance/emigate"),
            None,
            "other",
        );
        assert_eq!(d2.plan, Plan::Focus);
        assert!(d2.steps[0].contains(&"w7:t1".to_string()));
        // empty workspace + path → create a labelled tab INSIDE it
        let d3 = herdr_tab_decision(
            &fixture(),
            &[],
            Some("/home/u/Projects/herdctl"),
            Some("wN"),
            "herdctl",
        );
        assert_eq!(d3.plan, Plan::Create);
        assert!(d3.steps[0].windows(2).any(|w| w[0] == "--workspace" && w[1] == "wN"));
        // workspace missing → two steps, placeholder for the new ws id
        let empty = json!({"ok": true, "workspaces": [], "agents": []});
        let d4 = herdr_tab_decision(
            &empty,
            &[],
            Some("/home/u/Projects/herdctl"),
            None,
            "herdctl",
        );
        assert_eq!(d4.plan, Plan::Create);
        assert_eq!(d4.steps.len(), 2);
        assert!(d4.steps[1].iter().any(|a| a == NEW_WS));
    }

    // ------------------------------------------ herdr client = atlas session ---

    /// The ensure-client step: the herdr client is the atlas terminal session
    /// `herdr`. Alive → a read-only no-op; dead → an `atlas_session` mutating
    /// step the HTTP layer executes through the session registry (never a
    /// bare subprocess — the client needs a TTY).
    #[test]
    fn herdr_session_step_reuses_or_spawns_via_registry() {
        let alive = herdr_session_step(true);
        assert!(alive.read_only);
        assert!(!alive.atlas_session);
        assert!(alive.argv.is_empty(), "reuse adds no command");

        let dead = herdr_session_step(false);
        assert!(!dead.read_only);
        assert!(dead.atlas_session, "the client is spawned as a terminal session");
        assert_eq!(dead.argv, vec![cfg::HERDR_BIN.clone()], "bare `herdr` = launch or attach");
    }

    #[test]
    fn workspace_plan_server_down_is_none_without_spawn() {
        let p = herdr_workspace_plan(None, true, Some("/p"), None);
        assert_eq!(p.plan, Plan::None);
        assert!(p.argv.is_empty());
        assert!(
            p.steps.iter().all(|s| s.read_only),
            "a dead server allows read-only discovery only"
        );
        // a dead herdr session changes nothing: the server gate comes first
        let p2 = herdr_workspace_plan(None, false, Some("/p"), None);
        assert_eq!(p2.plan, Plan::None);
    }

    #[test]
    fn workspace_plan_puts_herdr_session_before_focus() {
        // no live ws, no persisted id → ws create; no terminal session →
        // the herdr client is spawned through the session registry first
        let herdr = json!({"ok": true, "workspaces": [], "agents": []});
        let p = herdr_workspace_plan(
            Some(&herdr),
            false,
            Some("/home/u/Projects/herdctl"),
            None,
        );
        assert_eq!(p.plan, Plan::Create);
        let kinds: Vec<bool> = p.steps.iter().map(|s| s.read_only).collect();
        assert_eq!(kinds, vec![true, false, false]);
        assert_eq!(p.argv, p.steps[1].argv, "argv is the first mutating step");
        assert!(p.steps[1].atlas_session, "the client step goes through the registry");
        assert!(p.steps[2].argv.windows(2).any(|w| w[0] == "workspace" && w[1] == "create"));
        // the session already alive → reuse, only the workspace step mutates
        let p2 = herdr_workspace_plan(
            Some(&herdr),
            true,
            Some("/home/u/Projects/herdctl"),
            None,
        );
        let kinds2: Vec<bool> = p2.steps.iter().map(|s| s.read_only).collect();
        assert_eq!(kinds2, vec![true, true, false]);
        assert_eq!(p2.argv, p2.steps[2].argv);
    }

    #[test]
    fn tab_plan_server_down_is_none_without_spawn() {
        let p = herdr_tab_plan(None, &[], true, Some("/p"), None, "x");
        assert_eq!(p.plan, Plan::None);
        assert!(p.argv.is_empty());
        assert!(p.steps.iter().all(|s| s.read_only));
        // with the server up, the herdr session step precedes the herdr steps
        let p2 = herdr_tab_plan(
            Some(&fixture()),
            &[],
            false,
            Some("/home/u/Projects/herdctl"),
            Some("wN"),
            "herdctl",
        );
        assert_eq!(p2.plan, Plan::Create);
        assert!(p2.steps[2].atlas_session, "ensure the client session first");
        assert!(p2.steps[3].argv.windows(2).any(|w| w[0] == "tab" && w[1] == "create"));
    }

    // ------------------------------------------------------------- panel ---

    #[test]
    fn herdr_panel_composes_snapshot_and_client() {
        let snap = json!({
            "ok": true,
            "focused_workspace_id": "w8",
            "focused_tab_id": "w8:t1",
            "workspaces": [
                {"workspace_id": "w7", "label": "emigate", "focused": false},
                {"workspace_id": "w8", "label": "home", "focused": true},
            ],
            "tabs": [
                {"tab_id": "w7:t1", "workspace_id": "w7", "label": "zsh",
                 "pane_count": 2, "focused": false},
                {"tab_id": "w8:t1", "workspace_id": "w8", "label": "zsh",
                 "pane_count": 1, "focused": true},
            ],
            "agents": [
                {"pane_id": "w7:p1", "agent_status": "working"},
                {"pane_id": "w7:p2", "agent_status": "working"},
                {"pane_id": "w7:pN", "agent_status": "idle"},
                {"pane_id": "w8:p1", "agent_status": "idle"},
            ],
        });
        // the client is the atlas terminal session `herdr`: liveness + pid
        let p = herdr_panel(&snap, true, Some(4242));
        assert_eq!(p["ok"], true);
        assert_eq!(p["server"], true);
        assert_eq!(p["workspaces"][0]["id"], "w7");
        assert_eq!(p["workspaces"][0]["label"], "emigate");
        assert_eq!(p["workspaces"][0]["focused"], false);
        assert_eq!(
            p["workspaces"][0]["agents"],
            json!([{"status": "idle", "count": 1}, {"status": "working", "count": 2}])
        );
        assert_eq!(
            p["workspaces"][0]["tabs"],
            json!([{"id": "w7:t1", "label": "zsh", "focused": false, "pane_count": 2}])
        );
        assert_eq!(p["workspaces"][1]["focused"], true);
        assert_eq!(p["workspaces"][1]["agents"], json!([{"status": "idle", "count": 1}]));
        assert_eq!(p["client"], json!({"attached": true, "session": "herdr", "pid": 4242}));
        // session not running: honest false, no pid
        let p2 = herdr_panel(&snap, false, None);
        assert_eq!(p2["client"], json!({"attached": false, "session": "herdr", "pid": null}));
    }

    #[test]
    fn herdr_panel_down_is_all_false() {
        let down = json!({"ok": false, "workspaces": [], "agents": [], "tabs": []});
        let p = herdr_panel(&down, true, Some(1));
        assert_eq!(p["ok"], false);
        assert_eq!(p["server"], false);
        assert_eq!(p["workspaces"], json!([]));
        assert_eq!(
            p["client"],
            json!({"attached": false, "session": "herdr", "pid": null})
        );
    }
}
