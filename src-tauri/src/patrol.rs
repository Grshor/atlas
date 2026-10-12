//! «Обход флота» — a deterministic fleet patrol. No model, no heuristics: a
//! tokio task re-runs the SAME sync pipeline `POST /api/sync` uses, then
//! computes a digest, posts ONE notification and stores it in `meta`.
//!
//! Schedule: the first cycle runs ~2 min after server start
//! (`ATLAS_PATROL_FIRST_RUN_SECS`, test-only override), then every
//! `patrol.interval_min` minutes. Settings are re-read every loop, so an
//! interval/enabled change takes effect on the next cycle. A cycle is skipped
//! when `patrol.enabled` is false or a manual sync already holds the lock.
use crate::db;
use crate::importers;
use crate::settings;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// First cycle delay after server start (seconds).
pub const DEFAULT_FIRST_RUN_SECS: u64 = 120;

/// Settings key holding the last digest JSON in the `meta` table.
const META_KEY: &str = "patrol.last";

const QUADRANTS: [&str; 4] = ["q1", "q2", "q3", "q4"];

/// First-run delay; `ATLAS_PATROL_FIRST_RUN_SECS` overrides it (live checks).
fn first_run_secs() -> u64 {
    std::env::var("ATLAS_PATROL_FIRST_RUN_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_FIRST_RUN_SECS)
}

/// Spawn the patrol task once per server process. Call after the DB is ready.
pub fn spawn() {
    let first = first_run_secs();
    log::info!("patrol: first cycle in {first}s");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(first)).await;
        loop {
            let (enabled, interval_min) = tokio::task::spawn_blocking(snapshot)
                .await
                .unwrap_or((true, 360));
            if enabled {
                run_cycle().await;
            } else {
                log::debug!("patrol: disabled — sleeping");
            }
            let secs = (interval_min.max(1) as u64) * 60;
            tokio::time::sleep(Duration::from_secs(secs)).await;
        }
    });
}

/// `(enabled, interval_min)` from the effective settings (validated, defaults
/// applied).
fn snapshot() -> (bool, i64) {
    let conn = db::db_static().lock();
    snapshot_conn(&conn)
}

fn snapshot_conn(conn: &Connection) -> (bool, i64) {
    let eff = settings::effective(conn, None);
    let s = eff.get("settings");
    let enabled = s
        .and_then(|v| v.get("patrol.enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let interval_min = s
        .and_then(|v| v.get("patrol.interval_min"))
        .and_then(|v| v.as_i64())
        .unwrap_or(360);
    (enabled, interval_min)
}

/// `GET /api/patrol` — schedule + last digest (sync; call on the blocking pool).
pub fn status() -> Value {
    let conn = db::db_static().lock();
    let (enabled, interval_min) = snapshot_conn(&conn);
    let last = db::meta_get(&conn, META_KEY).and_then(|s| serde_json::from_str::<Value>(&s).ok());
    json!({"enabled": enabled, "interval_min": interval_min, "last": last})
}

/// One patrol cycle: reuse the sync pipeline, then digest + notify + store.
async fn run_cycle() {
    // Skip (never queue) when a manual sync holds the lock.
    let Ok(_guard) = importers::SYNC_LOCK.try_lock() else {
        log::info!("patrol: sync busy — cycle skipped");
        return;
    };
    let t0 = Instant::now();
    // The same herdr snapshot path `/api/herdr` uses (for "agents working").
    let herdr = importers::import_herdr().await;
    if let Err(e) = importers::sync_all(db::db()).await {
        log::warn!("patrol: sync failed: {e}");
        return;
    }
    let now = crate::util::now();
    let content = tokio::task::spawn_blocking(move || {
        let conn = db::db_static().lock();
        digest_content(&conn, &herdr, now)
    })
    .await
    .unwrap_or(Value::Null);
    if content.is_null() {
        log::warn!("patrol: digest task failed");
        return;
    }

    let level = notification_level(&content);
    let line = notification_text(&content);
    let duration_s = (t0.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
    let digest = json!({
        "ts": now,
        "duration_s": duration_s,
        "notified_level": level,
        "projects": content["projects"],
        "agents_working": content["agents_working"],
        "board": content["board"],
        "delegated": content["delegated"],
        "sources_err": content["sources_err"],
        "stale": content["stale"],
    });

    let stored = digest.clone();
    let text = line.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let conn = db::db_static().lock();
        if let Err(e) = db::meta_set(&conn, META_KEY, &stored.to_string()) {
            log::warn!("patrol: meta write failed: {e}");
        }
        if let Err(e) = db::insert_notification(&conn, level, None, &text) {
            log::warn!("patrol: notification failed: {e}");
        }
    })
    .await;
    log::info!("patrol: {line}");
}

/// The digest content (no timing/level, which the caller supplies).
fn digest_content(conn: &Connection, herdr: &Value, now: f64) -> Value {
    let projects: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM projects p LEFT JOIN project_flags f ON f.name = p.name \
             WHERE COALESCE(f.archived, 0) = 0",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    // herdr snapshot: agents whose `agent_status` is `working`, as /api/herdr
    // groups them. herdr down → no data → 0 (and `herdr` lands in sources_err).
    let agents_working = herdr
        .get("agents")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter(|x| x.get("agent_status").and_then(|s| s.as_str()) == Some("working"))
                .count() as i64
        })
        .unwrap_or(0);

    // open board counts per quadrant (done = 0), always all four keys
    let mut board = serde_json::Map::new();
    for q in QUADRANTS {
        board.insert(q.to_string(), json!(0));
    }
    if let Ok(mut stmt) = conn.prepare("SELECT quadrant, COUNT(*) FROM items WHERE done = 0 GROUP BY quadrant")
    {
        if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
            for (q, c) in rows.flatten() {
                if board.contains_key(&q) {
                    board.insert(q, json!(c));
                }
            }
        }
    }

    // «Этап B»: delegations in flight (queued/running) and awaiting review —
    // the segment appears in the digest line only when one of them is non-zero.
    let active = |state: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM items WHERE delegation_state = ?1",
            [state],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    let delegated = json!({
        "queued": active("queued"),
        "running": active("running"),
        "review": active("review"),
    });

    let sources_err: Vec<String> = conn
        .prepare("SELECT name FROM sources WHERE ok = 0 ORDER BY name")
        .and_then(|mut stmt| {
            stmt.query_map([], |r| r.get::<_, String>(0))
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();

    // up to 3 non-archived projects with a path, idle >= 3 days, largest first
    let mut stale_candidates: Vec<(i64, String)> = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT p.name, p.last_activity FROM projects p \
         LEFT JOIN project_flags f ON f.name = p.name \
         WHERE COALESCE(f.archived, 0) = 0 AND p.path IS NOT NULL AND TRIM(p.path) <> '' \
           AND p.last_activity > 0",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))) {
            for (name, last) in rows.flatten() {
                let days = ((now - last) / 86_400.0).floor() as i64;
                if days >= 3 {
                    stale_candidates.push((days, name));
                }
            }
        }
    }
    stale_candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let stale: Vec<String> = stale_candidates
        .into_iter()
        .take(3)
        .map(|(days, name)| format!("{name} ({days}д)"))
        .collect();

    json!({
        "projects": projects,
        "agents_working": agents_working,
        "board": Value::Object(board),
        "delegated": delegated,
        "sources_err": sources_err,
        "stale": stale,
    })
}

/// `warn` when any source is unhealthy, else `info`.
fn notification_level(content: &Value) -> &'static str {
    if content["sources_err"]
        .as_array()
        .map(|a| !a.is_empty())
        .unwrap_or(false)
    {
        "warn"
    } else {
        "info"
    }
}

/// One Russian line, e.g.
/// `обход флота: 54 проекта · агентов: 2 · q1: 3 · ошибки источников: нет ·
/// заброшенные: mind-limit (12д), mods (9д), tmp (7д)`.
fn notification_text(content: &Value) -> String {
    let projects = content["projects"].as_i64().unwrap_or(0);
    let agents_working = content["agents_working"].as_i64().unwrap_or(0);
    let board = &content["board"];
    let sources_err: Vec<String> = content["sources_err"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let stale: Vec<String> = content["stale"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let mut line = format!(
        "обход флота: {} · агентов: {agents_working}",
        plural_projects(projects)
    );
    // a quadrant segment only where the count is non-zero
    for q in QUADRANTS {
        let c = board.get(q).and_then(|v| v.as_i64()).unwrap_or(0);
        if c > 0 {
            line.push_str(&format!(" · {q}: {c}"));
        }
    }
    // «Этап B»: delegations appear only when one is in flight or on review
    let delegated = &content["delegated"];
    let (queued, running, review) = (
        delegated["queued"].as_i64().unwrap_or(0),
        delegated["running"].as_i64().unwrap_or(0),
        delegated["review"].as_i64().unwrap_or(0),
    );
    if queued + running + review > 0 {
        line.push_str(&format!(
            " · делегировано: queued {queued} · running {running} · review {review}"
        ));
    }
    let errs = if sources_err.is_empty() {
        "нет".to_string()
    } else {
        sources_err.join(", ")
    };
    line.push_str(&format!(" · ошибки источников: {errs}"));
    let stale_txt = if stale.is_empty() {
        "нет".to_string()
    } else {
        stale.join(", ")
    };
    line.push_str(&format!(" · заброшенные: {stale_txt}"));
    line
}

/// Russian plural of «проект»: 1 проект, 2–4 проекта, 5+ проектов.
fn plural_projects(n: i64) -> String {
    let abs100 = (n.abs() % 100) as i64;
    let abs10 = abs100 % 10;
    let word = if (11..=14).contains(&abs100) {
        "проектов"
    } else if abs10 == 1 {
        "проект"
    } else if (2..=4).contains(&abs10) {
        "проекта"
    } else {
        "проектов"
    };
    format!("{n} {word}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::apply_schema(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO projects(name, path, last_activity) VALUES \
               ('fresh', '/p/fresh', 1000000.0), \
               ('old',   '/p/old',   1000000.0), \
               ('older', '/p/older', 1000000.0), \
               ('oldest','/p/oldest',1000000.0), \
               ('nopath', NULL,      1000000.0); \
             INSERT INTO project_flags(name, archived) VALUES ('older', 1); \
             INSERT INTO items(title, quadrant, done, created_at, updated_at) VALUES \
               ('a','q1',0,0,0), ('b','q1',0,0,0), ('c','q2',0,0,0), ('d','q3',1,0,0); \
             INSERT INTO sources(name, ok) VALUES ('herdr',1), ('memory',0), ('zen',0);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn digest_over_a_fixture_db_and_herdr() {
        let conn = seeded_db();
        // now = base + N days; last_activity of every project is `base`
        let base = 1_000_000.0;
        let now = base + 12.0 * 86_400.0 + 60.0;
        // distinct idle ages: fresh is active today, others stale by path order
        conn.execute("UPDATE projects SET last_activity = ?1 WHERE name = 'fresh'", [now])
            .unwrap();
        conn.execute(
            "UPDATE projects SET last_activity = ?1 WHERE name = 'old'",
            [now - 9.0 * 86_400.0],
        )
        .unwrap();
        conn.execute(
            "UPDATE projects SET last_activity = ?1 WHERE name = 'oldest'",
            [now - 20.0 * 86_400.0],
        )
        .unwrap();
        // `older` is archived → excluded despite being old

        let herdr = json!({"ok": true, "agents": [
            {"agent_status": "working"}, {"agent_status": "idle"},
            {"agent_status": "working"}, {"agent_status": "unknown"},
        ]});
        let d = digest_content(&conn, &herdr, now);

        // fresh + old + oldest + older(archived) + nopath(no path) = 5 total,
        // but only 4 non-archived
        assert_eq!(d["projects"], 4);
        assert_eq!(d["agents_working"], 2);
        assert_eq!(d["board"], json!({"q1": 2, "q2": 1, "q3": 0, "q4": 0}));
        assert_eq!(d["sources_err"], json!(["memory", "zen"]));
        // oldest (20d) then old (9d); fresh (0d) and nopath (no path) excluded
        assert_eq!(d["stale"], json!(["oldest (20д)", "old (9д)"]));

        let line = notification_text(&d);
        assert_eq!(notification_level(&d), "warn", "sources with errors → warn");
        assert_eq!(
            line,
            "обход флота: 4 проекта · агентов: 2 · q1: 2 · q2: 1 · \
             ошибки источников: memory, zen · заброшенные: oldest (20д), old (9д)"
        );
    }

    /// «Этап B»: the digest line grows a delegation segment exactly while a
    /// delegation is queued, running or awaiting review; a settled board
    /// (only failed/done delegations) keeps the old line.
    #[test]
    fn digest_reports_active_delegations() {
        let conn = seeded_db();
        conn.execute_batch(
            "UPDATE items SET delegation_state='queued'  WHERE title='a'; \
             UPDATE items SET delegation_state='running' WHERE title='b'; \
             UPDATE items SET delegation_state='review'  WHERE title='c'; \
             UPDATE items SET delegation_state='failed'  WHERE title='d';",
        )
        .unwrap();
        let d = digest_content(&conn, &json!({"ok": true, "agents": []}), 1_000_000.0);
        assert_eq!(
            d["delegated"],
            json!({"queued": 1, "running": 1, "review": 1})
        );
        let line = notification_text(&d);
        assert!(
            line.contains("делегировано: queued 1 · running 1 · review 1"),
            "{line}"
        );

        // a settled board: failed keeps the count at zero → no segment
        conn.execute_batch("UPDATE items SET delegation_state='failed';")
            .unwrap();
        let d = digest_content(&conn, &json!({"ok": false, "agents": []}), 1_000_000.0);
        assert_eq!(
            d["delegated"],
            json!({"queued": 0, "running": 0, "review": 0})
        );
        assert!(!notification_text(&d).contains("делегировано"));
    }

    #[test]
    fn healthy_sources_keep_the_info_level() {
        let content = json!({
            "projects": 1, "agents_working": 0,
            "board": {"q1": 0, "q2": 0, "q3": 0, "q4": 0},
            "sources_err": [], "stale": [],
        });
        assert_eq!(notification_level(&content), "info");
        assert_eq!(
            notification_text(&content),
            "обход флота: 1 проект · агентов: 0 · ошибки источников: нет · заброшенные: нет"
        );
    }

    #[test]
    fn digest_is_honest_when_herdr_is_down() {
        let conn = seeded_db();
        let herdr = json!({"ok": false, "agents": []});
        let d = digest_content(&conn, &herdr, 1_000_000.0);
        assert_eq!(d["agents_working"], 0);
        assert_eq!(d["stale"], json!([]));
        assert!(notification_text(&d).contains("заброшенные: нет"));
    }

    #[test]
    fn plural_projects_is_russian() {
        assert_eq!(plural_projects(1), "1 проект");
        assert_eq!(plural_projects(2), "2 проекта");
        assert_eq!(plural_projects(54), "54 проекта");
        assert_eq!(plural_projects(11), "11 проектов");
        assert_eq!(plural_projects(0), "0 проектов");
    }
}
