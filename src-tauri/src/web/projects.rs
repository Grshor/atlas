//! Projects: listing with deps/favorites, create/archive/favorite/weburl,
//! source backfill (`git`/`herdr`/`vault`), file-path completion, and the
//! canonical project-row lookup shared with the tasks module.

use super::{db_lock, herdr_snapshot_ok, join_result, ok_json, persist_ws, query_param, ApiError};
use crate::cfg;
use crate::focus;
use crate::importers;
use crate::util::{
    norm, resolve_parent_in_projects, run_out, short, valid_project_name, validate_web_url,
};
use axum::extract::Path as AxumPath;
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

// ------------------------------------------------------------- projects ----

fn project_rows(conn: &rusqlite::Connection) -> Vec<Value> {
    let mut links: HashMap<String, Vec<Value>> = HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT from_project, to_project, kind, note FROM links")
            .expect("select links");
        let rows: Vec<(String, String, String, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .expect("query links")
            .filter_map(|r| r.ok())
            .collect();
        for (from, to, kind, note) in rows {
            links.entry(from).or_default().push(json!({
                "to_project": to, "kind": kind, "note": note,
            }));
        }
    }
    let mut stmt = conn.prepare(
        "SELECT p.name, p.path, p.vault_path, p.zen_space, p.herdr_workspace_id, p.agent_status, \
         p.agents_count, p.sessions_count, p.last_activity, COALESCE(f.archived, 0), \
         COALESCE(f.favorite, 0), m.web_url \
         FROM projects p LEFT JOIN project_flags f ON f.name = p.name \
         LEFT JOIN project_meta m ON m.name = p.name ORDER BY p.name",
    ).expect("select projects");
    let rows: Vec<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        i64,
        f64,
        i64,
        i64,
        Option<String>,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
            ))
        })
        .expect("query projects")
        .filter_map(|r| r.ok())
        .collect();
    // Избранное: favorites first, everything else after them. `partition` is a
    // stable partition, so neither half is re-sorted — each keeps the
    // `ORDER BY p.name` order of the query above (archived rows keep whatever
    // position they have today; the UI moves them into its «подвал» itself).
    let (mut favorites, rest): (Vec<_>, Vec<_>) = rows.into_iter().partition(|r| r.10 != 0);
    favorites.extend(rest);
    favorites
        .into_iter()
        .map(
            |(name, path, vault_path, zen_space, ws, status, agents, sessions, last, archived, favorite, web_url)| {
                let empty = |s: &Option<String>| s.as_ref().map(|x| !x.is_empty()).unwrap_or(false);
                let git = path
                    .as_ref()
                    .filter(|p| !p.is_empty())
                    .map(|p| Path::new(p).join(".git").is_dir())
                    .unwrap_or(false);
                json!({
                    "name": name,
                    "sources": {
                        "herdr": empty(&ws),
                        "omp": sessions > 0,
                        "git": git,
                        "vault": empty(&vault_path),
                        "zen": empty(&zen_space),
                    },
                    "herdr_workspace_id": ws,
                    "agent_status": status,
                    "archived": archived != 0,
                    "favorite": favorite != 0,
                    "agents_count": agents,
                    "sessions_count": sessions,
                    "last_activity": last,
                    "path": path,
                    "vault_path": vault_path,
                    "zen_space": zen_space,
                    "web_url": web_url,
                    "deps": links.get(&name).cloned().unwrap_or_default(),
                })
            },
        )
        .collect()
}

pub(crate) async fn api_projects() -> Result<Response, ApiError> {
    let rows = tokio::task::spawn_blocking(|| {
        let conn = db_lock();
        project_rows(&conn)
    })
    .await
    .map_err(join_result)?;
    Ok(ok_json(json!({"projects": rows, "count": rows.len()})))
}

/// POST /api/projects/weburl — set/clear the Просмотр iframe URL. Only local
/// `http://127.0.0.1:<port>/…` / `http://localhost:<port>/…` passes; the rule
/// name goes into the 422. `web_url: null` (or "") clears the value.
pub(crate) async fn api_projects_weburl(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| ApiError::unprocessable("name must be a string"))?
        .to_string();
    let web_url = match v.get("web_url") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                validate_web_url(s).map_err(ApiError::unprocessable)?;
                Some(s.to_string())
            }
        }
        Some(_) => return Err(ApiError::unprocessable("web_url must be a string or null")),
    };
    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let conn = db_lock();
        let row = proj_row(&conn, &name)
            .ok_or_else(|| ApiError::not_found(format!("unknown project '{name}'")))?;
        conn.execute(
            "INSERT INTO project_meta(name, web_url, updated_at) VALUES(?1, ?2, ?3) \
             ON CONFLICT(name) DO UPDATE SET \
                 web_url = excluded.web_url, updated_at = excluded.updated_at",
            rusqlite::params![row.name, web_url, crate::util::now()],
        )
        .map_err(|e| ApiError::bad(format!("{e}")))?;
        Ok(json!({"ok": true, "name": row.name, "web_url": web_url}))
    })
    .await
    .map_err(join_result)??;
    Ok(ok_json(out))
}

// -------------------------------------------------- projects / sources create ---

/// `git init <dir>` as an argv array (no shell), run synchronously on the
/// blocking pool. Ok = combined output; Err = the stderr/reason, never a panic.
fn git_init_blocking(target: &std::path::Path) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("init")
        .arg(target)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        Ok(text)
    } else if text.trim().is_empty() {
        Err("git init завершился с ненулевым кодом".into())
    } else {
        Err(text)
    }
}

/// Async wrapper for the sources/create handler (same argv, blocking pool).
async fn git_init_dir(target: std::path::PathBuf) -> Result<String, String> {
    tokio::task::spawn_blocking(move || git_init_blocking(&target))
        .await
        .map_err(|e| format!("blocking task: {e}"))?
}

/// POST /api/projects/create — `{"name":"…","parent":null|"sub/dir",
/// "git_init":true?}`: create the project directory under ~/Projects
/// (optionally nested), `git init` it (default true, argv array, no shell),
/// drop a two-line README stub and INSERT the `projects` row so the project
/// appears immediately. The row survives `POST /api/sync`: sync only upserts
/// rows it discovered and never drops unknown ones — and with `git_init` the
/// git importer (~/Projects, depth ≤ 4) rediscovers the directory anyway.
/// 422 = bad name / escaping parent; 409 = directory or project name exists.
pub(crate) async fn api_projects_create(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| ApiError::unprocessable("name must be a string"))?
        .trim()
        .to_string();
    if !valid_project_name(&name) {
        return Err(ApiError::unprocessable(format!(
            "имя проекта '{name}' не подходит: оно должно соответствовать шаблону \
             [A-Za-z0-9][A-Za-z0-9._-]* (ASCII буква/цифра в начале, затем буквы, цифры, . _ -)"
        )));
    }
    let parent = match v.get("parent") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(ApiError::unprocessable("parent must be a string or null")),
    };
    let parent_dir = resolve_parent_in_projects(&parent).map_err(ApiError::unprocessable)?;
    let git_init = v.get("git_init").and_then(|g| g.as_bool()).unwrap_or(true);
    let target = parent_dir.join(&name);

    // 409 before any filesystem change: same project name already in the DB
    {
        let probe = name.clone();
        let taken = tokio::task::spawn_blocking(move || proj_row(&db_lock(), &probe).is_some())
            .await
            .map_err(join_result)?;
        if taken {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                format!("проект с именем '{name}' уже есть"),
            ));
        }
    }
    if target.exists() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("каталог уже существует: {}", target.display()),
        ));
    }

    let target2 = target.clone();
    let name2 = name.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        std::fs::create_dir_all(&target2)
            .map_err(|e| ApiError::bad(format!("не удалось создать каталог: {e}")))?;
        if git_init {
            git_init_blocking(&target2)
                .map_err(|e| ApiError::bad(format!("git init не удался: {}", short(&e, 400))))?;
        }
        std::fs::write(target2.join("README.md"), format!("# {name2}\n\n"))
            .map_err(|e| ApiError::bad(format!("не удалось записать README.md: {e}")))?;
        let conn = db_lock();
        conn.execute(
            "INSERT INTO projects(name,path) VALUES(?1,?2)",
            rusqlite::params![name2, target2.to_string_lossy().into_owned()],
        )
        .map_err(|e| ApiError::bad(format!("insert project: {e}")))?;
        Ok(json!({
            "ok": true,
            "name": name2,
            "path": target2.to_string_lossy().into_owned(),
        }))
    })
    .await
    .map_err(join_result)??;
    Ok(ok_json(out))
}

/// POST /api/projects/{name}/sources/{kind}/create — create the project's
/// missing source in place. kind: `herdr` (workspace create without focus,
/// id persisted), `git` (`git init` when `<path>/.git` is absent), `vault`
/// (create `~/vault/Проекты/<name>` + minimal index.md, set vault_path).
/// `omp` and `zen` → 422 «источник появится позже».
pub(crate) async fn api_project_source_create(
    AxumPath((name, kind)): AxumPath<(String, String)>,
) -> Result<Response, ApiError> {
    match kind.as_str() {
        "herdr" | "git" | "vault" => {}
        "omp" | "zen" => {
            return Err(ApiError::unprocessable(format!(
                "источник '{kind}' появится позже"
            )))
        }
        other => {
            return Err(ApiError::unprocessable(format!(
                "unknown source kind '{other}'; supported: herdr, git, vault (omp/zen — позже)"
            )))
        }
    }
    let row = project_row_or_404(name).await?;
    let out = match kind.as_str() {
        "git" => source_create_git(&row).await?,
        "herdr" => source_create_herdr(&row).await?,
        _ => source_create_vault(&row).await?,
    };
    Ok(ok_json(out))
}

/// git: `<path>/.git` exists → 409 «уже репозиторий»; no path → 422; else
/// `git init` (argv array) → `{"ok":true,"repo":true}`. The badge turns on
/// immediately: GET /api/projects computes the git badge from `<path>/.git`.
async fn source_create_git(row: &ProjRow) -> Result<Value, ApiError> {
    let path = row
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            ApiError::unprocessable(format!(
                "у проекта '{}' нет пути — репозиторий создавать негде",
                row.name
            ))
        })?;
    let dir = std::path::Path::new(path);
    if dir.join(".git").exists() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("'{path}' уже репозиторий"),
        ));
    }
    git_init_dir(dir.to_path_buf())
        .await
        .map_err(|e| ApiError::bad(format!("git init не удался: {}", short(&e, 400))))?;
    Ok(json!({"ok": true, "repo": true}))
}

/// herdr: the project must have a live workspace already → 409 (checked
/// against a fresh `herdr api snapshot`: a persisted-but-dead id does not
/// block); a live workspace matching the project path counts too (creating a
/// duplicate would be wrong) — its id is persisted for the next focus. Else
/// `herdr workspace create --cwd <path>` (no --focus), the parsed id is
/// persisted → `{"ok":true,"workspace_id":"…"}`. Server down → 503, nothing
/// spawned.
async fn source_create_herdr(row: &ProjRow) -> Result<Value, ApiError> {
    let path = row
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            ApiError::unprocessable(format!(
                "у проекта '{}' нет пути — воркспейс herdr создавать негде",
                row.name
            ))
        })?;
    let herdr = importers::import_herdr().await;
    if !herdr_snapshot_ok(&herdr) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "herdr сервер недоступен — воркспейс не создавался",
        ));
    }
    // already has a live workspace: persisted live id first, then path match
    if let Some(ws) = row
        .ws
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty() && focus::herdr_ws_exists(&herdr, w))
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("у проекта уже есть живой herdr-воркспейс ({ws})"),
        ));
    }
    if let Some(ws) = focus::herdr_ws_for_path(&herdr, path) {
        persist_ws(&row.name, &ws).map_err(ApiError::bad)?;
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "у проекта уже есть живой herdr-воркспейс по пути проекта ({ws}); id сохранён"
            ),
        ));
    }
    let argv = vec![
        cfg::HERDR_BIN.clone(),
        "workspace".into(),
        "create".into(),
        "--cwd".into(),
        path.to_string(),
    ];
    let out = run_out(&argv, Duration::from_secs(25))
        .await
        .ok_or_else(|| ApiError::bad("herdr workspace create не ответил"))?;
    let ws = focus::parse_created_ws_id(&out).ok_or_else(|| {
        ApiError::bad(format!(
            "не удалось распознать workspace_id из вывода: {}",
            short(&out, 200)
        ))
    })?;
    persist_ws(&row.name, &ws).map_err(ApiError::bad)?;
    Ok(json!({"ok": true, "workspace_id": ws}))
}

/// vault: the convention is `importers::import_vault` — vault projects are
/// exactly the directories of `~/vault/Проекты/`, matched to projects by
/// directory name. So the honest creation is `mkdir -p ~/vault/Проекты/<имя>`
/// + a minimal `index.md`, then set `projects.vault_path` → the V badge turns
/// on and `POST /api/sync` keeps it (the vault importer rediscovers the
/// directory). Already set → 409.
async fn source_create_vault(row: &ProjRow) -> Result<Value, ApiError> {
    if row
        .vault
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .is_some()
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "у проекта '{}' уже задан vault_path ({})",
                row.name,
                row.vault.as_deref().unwrap_or_default()
            ),
        ));
    }
    let dir = crate::cfg::vault_projects_dir().join(&row.name);
    let proj_name = row.name.clone();
    tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        std::fs::create_dir_all(&dir)
            .map_err(|e| ApiError::bad(format!("не удалось создать каталог vault: {e}")))?;
        let index = dir.join("index.md");
        if !index.exists() {
            std::fs::write(&index, format!("# {proj_name}\n\n"))
                .map_err(|e| ApiError::bad(format!("не удалось записать index.md: {e}")))?;
        }
        let path = dir.to_string_lossy().into_owned();
        let conn = db_lock();
        conn.execute(
            "UPDATE projects SET vault_path = ?1 WHERE name = ?2",
            rusqlite::params![path, proj_name],
        )
        .map_err(|e| ApiError::bad(format!("update vault_path: {e}")))?;
        Ok(json!({"ok": true, "vault_path": path}))
    })
    .await
    .map_err(join_result)?
}

/// Canonical project row by normalized name (shared by focus, archive and
/// the sources/create handlers).
pub(crate) struct ProjRow {
    pub(crate) name: String,
    pub(crate) path: Option<String>,
    pub(crate) ws: Option<String>,
    pub(crate) vault: Option<String>,
}

pub(crate) fn proj_row(conn: &rusqlite::Connection, project: &str) -> Option<ProjRow> {
    // Compare in Rust: SQLite lower() is ASCII-only, so Cyrillic names
    // ("Игры", "Собесы-Go") never match inside SQL.
    let want = norm(project);
    let mut stmt = conn
        .prepare("SELECT name, path, herdr_workspace_id, vault_path FROM projects")
        .ok()?;
    let rows: Vec<(String, Option<String>, Option<String>, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .ok()?
        .filter_map(|r| r.ok())
        .collect();
    rows.into_iter()
        .find(|(name, _, _, _)| norm(name) == want)
        .map(|(name, path, ws, vault)| ProjRow { name, path, ws, vault })
}

// ------------------------------------------------------------- archive ----

pub(crate) async fn api_projects_archive(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let project = v
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| ApiError::unprocessable("name must be a string"))?
        .to_string();
    let archived = v
        .get("archived")
        .and_then(|a| a.as_bool())
        .ok_or_else(|| ApiError::unprocessable("archived must be a boolean"))?;
    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let conn = db_lock();
        let name = proj_row(&conn, &project)
            .ok_or_else(|| ApiError::not_found(format!("unknown project '{project}'")))?
            .name;
        // Flag only: projects is recomputed by sync, nothing is ever deleted.
        conn.execute(
            "INSERT INTO project_flags(name, archived, updated_at) VALUES(?1, ?2, ?3) \
             ON CONFLICT(name) DO UPDATE SET \
                 archived = excluded.archived, updated_at = excluded.updated_at",
            rusqlite::params![name, archived as i64, crate::util::now()],
        )
        .map_err(|e| ApiError::bad(format!("{e}")))?;
        Ok(json!({"ok": true, "name": name, "archived": archived}))
    })
    .await
    .map_err(join_result)??;
    Ok(ok_json(out))
}

/// POST /api/projects/favorite — pin/unpin a project at the TOP of the
/// projects list. Same lookup as `api_projects_archive` (Cyrillic-safe
/// `norm` comparison): unknown project → 404, non-bool `favorite` → 422.
/// Flag only (project_flags): projects is recomputed by sync, nothing is
/// ever deleted, and re-setting the same value is a no-op.
pub(crate) async fn api_projects_favorite(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let project = v
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| ApiError::unprocessable("name must be a string"))?
        .to_string();
    let favorite = v
        .get("favorite")
        .and_then(|f| f.as_bool())
        .ok_or_else(|| ApiError::unprocessable("favorite must be a boolean"))?;
    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let conn = db_lock();
        let name = proj_row(&conn, &project)
            .ok_or_else(|| ApiError::not_found(format!("unknown project '{project}'")))?
            .name;
        // Flag only: projects is recomputed by sync, nothing is ever deleted.
        // The conflict branch touches `favorite` alone, so a row's `archived`
        // (and vice versa in api_projects_archive) is left intact.
        conn.execute(
            "INSERT INTO project_flags(name, favorite, updated_at) VALUES(?1, ?2, ?3) \
             ON CONFLICT(name) DO UPDATE SET \
                 favorite = excluded.favorite, updated_at = excluded.updated_at",
            rusqlite::params![name, favorite as i64, crate::util::now()],
        )
        .map_err(|e| ApiError::bad(format!("{e}")))?;
        Ok(json!({"ok": true, "name": name, "favorite": favorite}))
    })
    .await
    .map_err(join_result)??;
    Ok(ok_json(out))
}


// ----------------------------------------------------------------- tasks ---
fn unknown_project(name: &str) -> ApiError {
    ApiError::not_found(format!("unknown project '{name}'"))
}

/// Canonical project row, or 404.
pub(crate) async fn project_row_or_404(name: String) -> Result<ProjRow, ApiError> {
    let lookup = name.clone();
    tokio::task::spawn_blocking(move || {
        let conn = db_lock();
        proj_row(&conn, &lookup)
    })
    .await
    .map_err(join_result)?
    .ok_or_else(|| unknown_project(&name))
}

// ---------------------------------------------------- project files (A) ----

/// Skip-list + caps of the completion walker («Этап А»).
const FILES_WALK_SKIP: [&str; 4] = [".git", "node_modules", "target", "dist"];
const FILES_WALK_MAX_DEPTH: usize = 6;
const FILES_WALK_MAX_SCANNED: usize = 2000;
const FILES_COMPLETION_LIMIT: usize = 20;

/// BFS over `roots` (project path + vault dir), files only, sorted children
/// for a deterministic order, skip-listed dir names never entered, ≤6 levels,
/// ≤2000 entries scanned. Case-insensitive substring match on the absolute
/// path; first 20 hits.
fn collect_project_files(roots: &[std::path::PathBuf], q_lower: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    let mut queue: std::collections::VecDeque<(std::path::PathBuf, usize)> = roots
        .iter()
        .filter(|r| r.is_dir())
        .map(|r| (r.clone(), 0))
        .collect();
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<std::path::PathBuf> =
            rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            scanned += 1;
            if scanned > FILES_WALK_MAX_SCANNED {
                return out;
            }
            let Ok(ft) = std::fs::symlink_metadata(&p).map(|m| m.file_type()) else {
                continue;
            };
            if ft.is_dir() {
                let skipped = p
                    .file_name()
                    .map(|n| FILES_WALK_SKIP.contains(&n.to_string_lossy().as_ref()))
                    .unwrap_or(true);
                if !skipped && depth < FILES_WALK_MAX_DEPTH {
                    queue.push_back((p, depth + 1));
                }
            } else if ft.is_file() {
                let s = p.to_string_lossy().into_owned();
                if q_lower.is_empty() || s.to_lowercase().contains(q_lower) {
                    out.push(s);
                    if out.len() >= FILES_COMPLETION_LIMIT {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// GET /api/project/{name}/files?q=<substr> — completion source for the UI:
/// absolute file paths under the project's `path` (and its vault dir, when
/// `projects.vault_path` is set). No path → 422, unknown project → 404.
pub(crate) async fn api_project_files(
    AxumPath(name): AxumPath<String>,
    uri: Uri,
) -> Result<Response, ApiError> {
    let q = query_param(&uri, "q").unwrap_or_default();
    let q_lower = q.trim().to_lowercase();
    let row = project_row_or_404(name).await?;
    let path = row
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            ApiError::unprocessable(format!("у проекта '{}' нет пути", row.name))
        })?;
    let mut roots = vec![std::path::PathBuf::from(path)];
    if let Some(v) = row
        .vault
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        roots.push(std::path::PathBuf::from(v));
    }
    let files = tokio::task::spawn_blocking(move || collect_project_files(&roots, &q_lower))
        .await
        .map_err(join_result)?;
    Ok(ok_json(json!({"files": files})))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::web::support::{call, test_router};
    use axum::http::StatusCode;
    /// POST /api/projects/create: the name rule and the parent containment
    /// rule are 422s that name the rule; a taken project name is 409 BEFORE
    /// any filesystem change. The happy path writes under the real
    /// ~/Projects, so it is covered by live checks with a disposable name,
    /// not here.
    #[tokio::test]
    async fn project_create_rejects_before_touching_the_filesystem() {
        let r = test_router();

        // name rule
        for bad in ["", ".hidden", "..", "-x", "a/b", "проект"] {
            let body = serde_json::json!({"name": bad}).to_string();
            let (st, out) = call(&r, "POST", "/api/projects/create", Some(&body)).await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{bad} → {out}");
            let detail = out["detail"].as_str().unwrap_or_default();
            assert!(
                detail.contains("[A-Za-z0-9][A-Za-z0-9._-]*"),
                "422 must name the rule: {detail}"
            );
        }

        // parent containment rule (both spellings of an escape)
        for parent in ["..", "../escape", "sub/../../escape", "/tmp/escape"] {
            let body = serde_json::json!({"name": "zz-ok-name", "parent": parent}).to_string();
            let (st, out) = call(&r, "POST", "/api/projects/create", Some(&body)).await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{parent} → {out}");
            assert!(
                out["detail"].as_str().unwrap_or_default().contains("~/Projects"),
                "422 must name the rule: {out}"
            );
        }

        // taken name → 409, and nothing appears on disk anywhere
        let (st, out) = call(
            &r,
            "POST",
            "/api/projects/create",
            Some(r#"{"name":"lamplife"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{out}");
        assert!(out["detail"].as_str().unwrap_or_default().contains("уже есть"));
    }

    /// Sync survival: the row created by POST /api/projects/create must
    /// survive `POST /api/sync`. sync recomputes DISCOVERED projects and
    /// upserts them with COALESCE — it never drops unknown rows. This runs
    /// the real `sync_all_blocking` with empty imports (nothing discovered)
    /// over the seeded row.
    #[tokio::test]
    async fn created_row_survives_sync() {
        let r = test_router();
        let db = crate::db::db();

        {
            let conn = crate::db::db_static().lock();
            conn.execute(
                "INSERT INTO projects(name,path,vault_path) VALUES('zz-sync-test', '/tmp/zz-sync-test', '/tmp/vault/zz')",
                [],
            )
            .expect("seed created row");
            // links are seeded only when the table is empty (production: by
            // the first real sync every referenced project exists); a
            // placeholder row keeps the seeder off in this empty-import run
            conn.execute(
                "INSERT OR IGNORE INTO links(from_project,to_project,kind) \
                 VALUES('zz-sync-test','zz-sync-test','soft')",
                [],
            )
            .expect("seed link placeholder");
            // favorite + archived flags: they live in project_flags, which sync
            // never recomputes (the flags analogue of the row above).
            conn.execute(
                "INSERT INTO project_flags(name, archived, favorite) VALUES('zz-sync-test', 1, 1) \
                 ON CONFLICT(name) DO UPDATE SET archived = 1, favorite = 1",
                [],
            )
            .expect("seed flags");
        }

        // empty discovery: herdr down, no sessions, no git repos, no vault
        crate::importers::sync_all_blocking(
            db,
            serde_json::json!({"ok": false, "workspaces": [], "agents": []}),
            (Vec::new(), 0),
            Vec::new(),
            crate::importers::VaultImport { projects: Vec::new(), areas: 0, inbox: 0, total: 0 },
            serde_json::json!({"ok": false, "spaces": []}),
            serde_json::json!({"ok": false}),
            0, 0, 0, 0, 0, 0, false, 0, false, false, 0, 0, 0,
        )
        .expect("sync with empty imports");

        let conn = crate::db::db_static().lock();
        let (name, path, vault): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT name,path,vault_path FROM projects WHERE name = 'zz-sync-test'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("the created row must survive sync");
        assert_eq!(name, "zz-sync-test");
        assert_eq!(path.as_deref(), Some("/tmp/zz-sync-test"));
        assert_eq!(vault.as_deref(), Some("/tmp/vault/zz"), "COALESCE keeps vault_path");
        let (archived, favorite): (i64, i64) = conn
            .query_row(
                "SELECT archived, favorite FROM project_flags WHERE name = 'zz-sync-test'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the flags row must survive sync");
        assert_eq!(
            (archived, favorite),
            (1, 1),
            "sync recomputes projects but never project_flags, so favorite survives like archived"
        );
        drop(conn);

        // and the flag shows through GET /api/projects
        let (st, list) = call(&r, "GET", "/api/projects", None).await;
        assert_eq!(st, StatusCode::OK, "{list}");
        let row = list["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "zz-sync-test")
            .expect("zz-sync-test row");
        assert_eq!(row["favorite"], true);
        assert_eq!(row["archived"], true);
    }

    /// POST /api/projects/favorite: 422 for a non-bool `favorite` (or a
    /// non-string `name`), 404 for an unknown project, and the canonical
    /// Cyrillic-safe lookup shared with `/api/projects/archive`; setting the
    /// same value twice is fine, nothing is ever deleted, and the favorite
    /// upsert leaves `archived` (and vice versa) untouched.
    #[tokio::test]
    async fn project_favorite_contract() {
        let r = test_router();
        {
            let conn = crate::db::db_static().lock();
            conn.execute(
                "INSERT INTO projects(name, path) VALUES('Игры-Тест', NULL) \
                 ON CONFLICT(name) DO NOTHING",
                [],
            )
            .expect("seed cyrillic project");
        }

        // 422: `favorite` must be a boolean (missing or wrong type)
        for body in [
            r#"{"name":"lamplife"}"#,
            r#"{"name":"lamplife","favorite":"yes"}"#,
            r#"{"name":"lamplife","favorite":1}"#,
            r#"{"name":123,"favorite":true}"#,
        ] {
            let (st, out) = call(&r, "POST", "/api/projects/favorite", Some(body)).await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body} → {out}");
        }

        // 404: unknown project (nothing is created)
        let (st, out) = call(
            &r,
            "POST",
            "/api/projects/favorite",
            Some(r#"{"name":"zz-nope-nope","favorite":true}"#),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{out}");
        assert!(out["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("zz-nope-nope"));

        // happy path + idempotence (twice the same value is fine)
        for _ in 0..2 {
            let (st, out) = call(
                &r,
                "POST",
                "/api/projects/favorite",
                Some(r#"{"name":"lamplife","favorite":true}"#),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{out}");
            assert_eq!(out["ok"], true);
            assert_eq!(out["name"], "lamplife");
            assert_eq!(out["favorite"], true);
        }
        let (st, list) = call(&r, "GET", "/api/projects", None).await;
        assert_eq!(st, StatusCode::OK);
        let row = list["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "lamplife")
            .expect("lamplife row");
        assert_eq!(row["favorite"], true);

        // canonical Cyrillic-safe lookup: a lowercased name resolves to the row
        let (st, out) = call(
            &r,
            "POST",
            "/api/projects/favorite",
            Some(r#"{"name":"игры-тест","favorite":true}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["name"], "Игры-Тест", "canonical name is returned");

        // favorite and archive coexist (each upsert touches its own column)
        call(
            &r,
            "POST",
            "/api/projects/archive",
            Some(r#"{"name":"Игры-Тест","archived":true}"#),
        )
        .await;
        let (st, list) = call(&r, "GET", "/api/projects", None).await;
        assert_eq!(st, StatusCode::OK);
        let row = list["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "Игры-Тест")
            .expect("Игры-Тест row");
        assert_eq!(row["favorite"], true);
        assert_eq!(row["archived"], true, "favorite upsert must not clear archived");
        call(
            &r,
            "POST",
            "/api/projects/archive",
            Some(r#"{"name":"Игры-Тест","archived":false}"#),
        )
        .await;

        // unset is idempotent too, and the row is never deleted
        for _ in 0..2 {
            let (st, out) = call(
                &r,
                "POST",
                "/api/projects/favorite",
                Some(r#"{"name":"lamplife","favorite":false}"#),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{out}");
            assert_eq!(out["favorite"], false);
        }
        let (st, list) = call(&r, "GET", "/api/projects", None).await;
        assert_eq!(st, StatusCode::OK);
        let row = list["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "lamplife")
            .expect("lamplife row");
        assert_eq!(row["favorite"], false);
    }

    /// Избранное: `GET /api/projects` (here `project_rows` over a fixture) puts
    /// favorites first and leaves BOTH halves in exactly today's `ORDER BY
    /// p.name` order — no alphabetical or activity re-sort — and archived rows
    /// keep the position they have today.
    #[test]
    fn project_rows_partitions_favorites_stably() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::apply_schema(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO projects(name, last_activity) VALUES \
                 ('zeta', 900.0), ('alpha', 100.0), ('mid', 500.0), \
                 ('beta', 300.0), ('omega', 700.0);
             INSERT INTO project_flags(name, archived, favorite) VALUES \
                 ('mid', 0, 1), ('omega', 0, 1), ('alpha', 1, 0);",
        )
        .unwrap();

        let rows = project_rows(&conn);
        let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
        // today's order is `ORDER BY p.name`: alpha, beta, mid, omega, zeta.
        // Favorites {mid, omega} move to the front in that same name order
        // (activity desc would have given zeta, omega, mid, beta, alpha), and
        // the archived `alpha` keeps its inner name position.
        assert_eq!(names, ["mid", "omega", "alpha", "beta", "zeta"]);
        assert_eq!(rows[0]["favorite"], true);
        assert_eq!(rows[1]["favorite"], true);
        assert_eq!(rows[2]["favorite"], false);
        assert_eq!(rows[2]["archived"], true, "archived keeps its name position");
        // every row carries the flag
        assert!(rows.iter().all(|r| r["favorite"].is_boolean()));
    }

    /// POST /api/projects/{name}/sources/{kind}/create, git branch: a /tmp
    /// fixture project gets `git init` (argv array), then 409 «уже
    /// репозиторий»; no-path projects → 422; unknown → 404.
    #[tokio::test]
    async fn sources_git_create_contract() {
        let r = test_router();
        let dir = std::env::temp_dir().join(format!("atlas-src-git-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("fixture dir");
        {
            let conn = crate::db::db_static().lock();
            conn.execute(
                "INSERT INTO projects(name,path) VALUES('zz-git-fixt', ?1) \
                 ON CONFLICT(name) DO UPDATE SET path = excluded.path",
                rusqlite::params![dir.to_string_lossy().into_owned()],
            )
            .expect("seed fixture project");
        }

        // no path → 422; unknown project → 404
        let (st, out) = call(&r, "POST", "/api/projects/phone-access/sources/git/create", None).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        assert!(out["detail"].as_str().unwrap_or_default().contains("нет пути"));
        let (st, _) = call(&r, "POST", "/api/projects/ghost/sources/git/create", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        // create → ok + repo:true + .git really exists (the badge's source)
        let (st, out) = call(&r, "POST", "/api/projects/zz-git-fixt/sources/git/create", None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["ok"], true);
        assert_eq!(out["repo"], true);
        assert!(dir.join(".git").exists(), "git init must have run in the fixture");

        // second create → 409 «уже репозиторий»
        let (st, out) = call(&r, "POST", "/api/projects/zz-git-fixt/sources/git/create", None).await;
        assert_eq!(st, StatusCode::CONFLICT, "{out}");
        assert!(out["detail"].as_str().unwrap_or_default().contains("уже репозиторий"));

        // cleanup
        let _ = std::fs::remove_dir_all(&dir);
        crate::db::db_static()
            .lock()
            .execute("DELETE FROM projects WHERE name = 'zz-git-fixt'", [])
            .expect("cleanup row");
    }

    /// sources/create: `omp`/`zen` are 422 «источник появится позже», an
    /// unknown kind is 422 with the supported list, and an already-set
    /// vault_path is 409 without touching the real vault.
    #[tokio::test]
    async fn sources_create_kinds_and_vault_conflict() {
        let r = test_router();
        for kind in ["omp", "zen"] {
            let (st, out) = call(
                &r,
                "POST",
                &format!("/api/projects/lamplife/sources/{kind}/create"),
                None,
            )
            .await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{kind} → {out}");
            assert_eq!(
                out["detail"],
                format!("источник '{kind}' появится позже"),
                "{kind} → {out}"
            );
        }
        let (st, out) = call(
            &r,
            "POST",
            "/api/projects/lamplife/sources/nope/create",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        assert!(out["detail"].as_str().unwrap_or_default().contains("herdr, git, vault"));

        // herdr without a project path → 422 (nothing to point --cwd at)
        let (st, out) = call(
            &r,
            "POST",
            "/api/projects/phone-access/sources/herdr/create",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        assert!(out["detail"].as_str().unwrap_or_default().contains("нет пути"));

        // vault already set → 409, nothing created in the real ~/vault
        {
            let conn = crate::db::db_static().lock();
            conn.execute(
                "UPDATE projects SET vault_path = '/tmp/already-vault' WHERE name = 'lamplife'",
                [],
            )
            .expect("seed vault_path");
        }
        let (st, out) = call(
            &r,
            "POST",
            "/api/projects/lamplife/sources/vault/create",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{out}");
        assert!(
            out["detail"].as_str().unwrap_or_default().contains("уже задан vault_path"),
            "{out}"
        );
    }

    /// GET /api/project/{name}/files: the completion walker — skip-listed
    /// dirs, depth cap, case-insensitive substring, vault root included,
    /// 20-hit cap; no path → 422, unknown project → 404.
    #[tokio::test]
    async fn project_files_completion_walker() {
        let r = test_router();
        let root = std::env::temp_dir().join(format!("atlas-walk-{}", std::process::id()));
        let src = root.join("src");
        let deep = src.join("a").join("b").join("c").join("d").join("e");
        std::fs::create_dir_all(&deep).expect("deep tree");
        std::fs::write(root.join("README.md"), "x").unwrap();
        std::fs::write(src.join("main.RS"), "x").unwrap();
        std::fs::write(deep.join("nested.txt"), "x").unwrap();
        // skipped dirs are never entered
        for skip in [".git", "node_modules", "target", "dist"] {
            let d = root.join(skip);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("hidden.md"), "x").unwrap();
        }
        // beyond the depth cap: dirs are entered to level 6, files inside a
        // level-7 directory are not
        let too_deep = root.join("l1").join("l2").join("l3")
            .join("l4").join("l5").join("l6").join("l7");
        std::fs::create_dir_all(&too_deep).unwrap();
        std::fs::write(too_deep.join("too-far.txt"), "x").unwrap();
        // vault root with its own file
        let vault = std::env::temp_dir().join(format!("atlas-walk-vault-{}", std::process::id()));
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::write(vault.join("note.md"), "x").unwrap();

        {
            let conn = crate::db::db_static().lock();
            // Dedicated row: `test_router()` re-seeds `lamplife` on every
            // parallel test, which would stomp the path mid-test.
            conn.execute(
                "INSERT INTO projects(name,path,vault_path) VALUES('walk-proj', ?1, ?2)",
                rusqlite::params![root.to_string_lossy().into_owned(), vault.to_string_lossy().into_owned()],
            )
            .expect("seed walk project");
        }

        // empty q → first 20 of the scan; everything visible is ours
        let (st, out) = call(&r, "GET", "/api/project/walk-proj/files", None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let files: Vec<String> = out["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap().to_string())
            .collect();
        assert!(files.len() <= 20);
        assert!(
            files.contains(&root.join("README.md").to_string_lossy().to_string()),
            "absolute paths: {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.contains("hidden.md")),
            "skip-listed dirs are not entered: {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.contains("too-far.txt")),
            "depth cap holds: {files:?}"
        );
        assert!(
            files.iter().any(|f| f.ends_with("nested.txt")),
            "6 levels are fine: {files:?}"
        );
        assert!(
            files.iter().any(|f| f.starts_with(vault.to_string_lossy().as_ref())),
            "vault root is scanned too: {files:?}"
        );

        // case-insensitive substring on the absolute path
        let (st, out) = call(&r, "GET", "/api/project/walk-proj/files?q=MAIN.RS", None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let files: Vec<String> = out["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap().to_string())
            .collect();
        assert_eq!(files, vec![src.join("main.RS").to_string_lossy().to_string()]);

        // vault-only substring
        let (st, out) = call(&r, "GET", "/api/project/walk-proj/files?q=note.md", None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["files"].as_array().unwrap().len(), 1);

        // 20-hit cap with a wide match
        for i in 0..25 {
            std::fs::write(root.join(format!("hit{i}.log")), "x").unwrap();
        }
        let (st, out) = call(&r, "GET", "/api/project/walk-proj/files?q=hit", None).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert_eq!(out["files"].as_array().unwrap().len(), 20);

        // no path → 422; unknown project → 404
        let (st, out) = call(&r, "GET", "/api/project/phone-access/files", None).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        assert!(out["detail"].as_str().unwrap_or_default().contains("нет пути"));
        let (st, _) = call(&r, "GET", "/api/project/ghost/files", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&vault);
    }
}
