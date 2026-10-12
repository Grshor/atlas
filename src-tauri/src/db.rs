//! SQLite via rusqlite(bundled): ONE shared connection behind a mutex.
//! Every read and write goes through it (the Python version deadlocked on
//! nested connections; serialising on a single connection rules that out).
use crate::cfg;
use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

/// Bumped per migration; `apply_schema` records it in PRAGMA user_version.
pub const SCHEMA_VERSION: i64 = 6;

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    fn open(path: &Path) -> rusqlite::Result<Db> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(10_000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }
}

static DB: OnceLock<Arc<Db>> = OnceLock::new();

/// Set once the schema is applied, so the GUI's window restore can wait for the
/// server thread's DB init instead of racing it (`--serve-only` and tests never
/// look at it).
static READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn mark_ready() {
    READY.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub fn is_ready() -> bool {
    READY.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn init() -> Result<Arc<Db>, String> {
    if let Some(db) = DB.get() {
        return Ok(Arc::clone(db));
    }
    let path = cfg::cfg().db_path.clone();
    let db = Db::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
    {
        let conn = db.lock();
        apply_schema(&conn).map_err(|e| format!("apply schema: {e}"))?;
    }
    let arc = Arc::new(db);
    let _ = DB.set(Arc::clone(&arc));
    mark_ready();
    Ok(arc)
}

pub fn db() -> Arc<Db> {
    Arc::clone(DB.get().expect("db not initialised"))
}

/// &'static handle so MutexGuards borrowed from it live for the request.
pub fn db_static() -> &'static Db {
    DB.get().expect("db not initialised")
}

/// CREATE IF NOT EXISTS + column migrations + PRAGMA user_version, idempotent
/// The project's most recently active omp session file (`sessions.latest_file`,
/// newest `latest_mtime` first; name matched `norm`-ed — lowercase, ё→е).
/// `None` = the project has no known sessions. Shared by `/api/open` (path
/// mapping) and the delegation worker (continue the project's conversation).
pub fn latest_session_file(conn: &Connection, project: &str) -> Option<String> {
    let name = crate::util::norm(project);
    conn.query_row(
        "SELECT s.latest_file FROM sessions s \
         WHERE lower(replace(s.project,'ё','е')) = ?1 \
         ORDER BY s.latest_mtime DESC LIMIT 1",
        [&name],
        |r| r.get(0),
    )
    .ok()
}

/// CREATE IF NOT EXISTS + column migrations + PRAGMA user_version, idempotent
/// (matches db.py). CREATE TABLE IF NOT EXISTS never touches an existing table,
/// so columns added later need an explicit, guarded ALTER.
pub fn apply_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(include_str!("../schema.sql"))?;
    migrate(conn)?;
    let cur: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if cur < SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

/// Column migrations, each guarded by PRAGMA table_info so re-running is a
/// no-op and a fresh DB (schema.sql already has the column) is untouched.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    if !has_column(conn, "items", "done")? {
        conn.execute_batch("ALTER TABLE items ADD COLUMN done INTEGER NOT NULL DEFAULT 0")?;
    }
    // «Этап A»: task brief on board items (v2/v3 DBs migrate; fresh DBs get
    // the column from schema.sql and skip this branch).
    if !has_column(conn, "items", "description")? {
        conn.execute_batch("ALTER TABLE items ADD COLUMN description TEXT")?;
    }
    // «Этап B»: idempotent-retry key for `atlas board add --dedupe-key`
    // (item_steps is CREATE IF NOT EXISTS in schema.sql — no column migration).
    if !has_column(conn, "items", "dedupe_key")? {
        conn.execute_batch("ALTER TABLE items ADD COLUMN dedupe_key TEXT")?;
    }
    // «Этап B»: delegation of a board item to a worker (see README «Делегация»).
    // The state column is NOT NULL DEFAULT '' so existing rows read "never
    // delegated"; every later column rides the same guarded ALTER.
    for (col, ty) in DELEGATION_COLUMNS {
        if !has_column(conn, "items", col)? {
            conn.execute_batch(&format!(
                "ALTER TABLE items ADD COLUMN {col} {ty}"
            ))?;
        }
    }
    // Избранное: `project_flags.favorite` pins a project to the top of the
    // list (v5 DBs get the guarded ALTER; fresh DBs carry it in schema.sql).
    if !has_column(conn, "project_flags", "favorite")? {
        conn.execute_batch(
            "ALTER TABLE project_flags ADD COLUMN favorite INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    Ok(())
}

/// `items` delegation columns («Этап B») — name and SQL type, in the order
/// schema.sql declares them; the fresh-DB table already carries all of them.
pub const DELEGATION_COLUMNS: [(&str, &str); 15] = [
    ("delegation_state", "TEXT NOT NULL DEFAULT ''"),
    ("delegation_target", "TEXT"),
    ("delegation_ref", "TEXT"),
    ("delegation_run_id", "INTEGER"),
    ("delegation_started", "REAL"),
    ("delegation_ended", "REAL"),
    ("delegation_exit", "INTEGER"),
    ("delegation_feedback", "TEXT"),
    ("delegation_head", "TEXT"),
    ("delegation_final", "TEXT"),
    ("delegation_session", "TEXT"),
    ("delegation_tail", "TEXT"),
    ("delegation_cmds", "TEXT"),
    ("delegation_review", "TEXT"),
    // Autopilot retry counter of the current cycle: incremented by every
    // auto-reject, zeroed by a manual reject («рули я»), accept and reset.
    ("delegation_attempts", "INTEGER NOT NULL DEFAULT 0"),
];

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// True when the error is a transient SQLITE_BUSY / SQLITE_LOCKED.
pub fn is_busy(e: &rusqlite::Error) -> bool {
    match e {
        rusqlite::Error::SqliteFailure(ffi, _) => {
            matches!(
                ffi.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
        }
        _ => false,
    }
}

// --------------------------------------------------------- notifications ----

/// The notification ring keeps only the newest NOTIFY_RING rows.
pub const NOTIFY_RING: usize = 200;

pub fn unread_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM notifications WHERE read = 0", [], |r| {
        r.get(0)
    })
}

/// Insert one notification and trim the ring. Shared by `POST /api/notify` and
/// the make-task runner (which reports exits here).
pub fn insert_notification(
    conn: &Connection,
    level: &str,
    project: Option<&str>,
    text: &str,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO notifications(ts, level, project, text) VALUES(?1, ?2, ?3, ?4)",
        rusqlite::params![crate::util::now(), level, project, text],
    )?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "DELETE FROM notifications WHERE id NOT IN \
         (SELECT id FROM notifications ORDER BY id DESC LIMIT ?1)",
        [NOTIFY_RING],
    )?;
    Ok(id)
}

// ---------------------------------------------------------------- meta -----

/// Read one `meta` row (nil when absent). Used by the fleet patrol digest.
pub fn meta_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| {
        r.get::<_, String>(0)
    })
    .ok()
}

/// Upsert one `meta` row.
pub fn meta_set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )
}

/// Shared throwaway DB for the whole test binary: ONE CFG + ONE init, behind
/// a mutex — parallel `#[tokio::test]`s double-opening the same sqlite file
/// trip «database is locked» (both http and orch tests funnel through this).
#[cfg(test)]
pub(crate) fn test_init_once() {
    use parking_lot::Mutex;
    use std::sync::Once;
    static ONCE: Once = Once::new();
    static GUARD: Mutex<()> = Mutex::new(());
    let _g = GUARD.lock();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("atlas-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let _ = crate::cfg::CFG.set(crate::cfg::Cfg {
            port: 0,
            db_path: dir.join("control.db"),
            serve_only: true,
        });
        init().expect("db init");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items_columns(conn: &Connection) -> Vec<String> {
        let mut stmt = conn.prepare("PRAGMA table_info(items)").unwrap();
        let mut rows = stmt.query([]).unwrap();
        let mut out = Vec::new();
        while let Some(r) = rows.next().unwrap() {
            out.push(r.get::<_, String>(1).unwrap());
        }
        out
    }

    #[test]
    fn done_migration_is_idempotent_on_fresh_and_legacy_db() {
        // fresh DB: schema.sql already carries `done`
        let fresh = Connection::open_in_memory().unwrap();
        apply_schema(&fresh).unwrap();
        assert!(items_columns(&fresh).contains(&"done".to_string()));
        let ver: i64 = fresh
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, SCHEMA_VERSION);
        // re-applying changes nothing (no duplicate column error)
        apply_schema(&fresh).unwrap();
        apply_schema(&fresh).unwrap();
        assert_eq!(
            items_columns(&fresh)
                .iter()
                .filter(|c| *c == "done")
                .count(),
            1
        );

        // legacy DB: items without `done` (v1 shape) + an existing row
        let legacy = Connection::open_in_memory().unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, \
                 quadrant TEXT NOT NULL CHECK (quadrant IN ('q1','q2','q3','q4')), project TEXT, \
                 created_at REAL NOT NULL, updated_at REAL NOT NULL);
                 INSERT INTO items(title,quadrant,project,created_at,updated_at) \
                 VALUES('старое дело','q1',NULL,1.0,1.0);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        assert!(!items_columns(&legacy).contains(&"done".to_string()));
        apply_schema(&legacy).unwrap();
        assert!(items_columns(&legacy).contains(&"done".to_string()));
        let (title, done): (String, i64) = legacy
            .query_row("SELECT title, done FROM items", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((title.as_str(), done), ("старое дело", 0), "existing rows start open");
        let ver: i64 = legacy
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, SCHEMA_VERSION);
        // and it stays idempotent on the migrated DB
        apply_schema(&legacy).unwrap();
        assert_eq!(
            items_columns(&legacy)
                .iter()
                .filter(|c| *c == "done")
                .count(),
            1
        );
    }

    /// «Этап А»: `items.description` migrates like `done` — fresh DBs carry it
    /// in schema.sql, legacy v2 DBs get the guarded ALTER, re-running is a
    /// no-op; `item_files` is CREATE IF NOT EXISTS and survives re-apply.
    #[test]
    fn description_migration_is_idempotent_and_item_files_table_exists() {
        let fresh = Connection::open_in_memory().unwrap();
        apply_schema(&fresh).unwrap();
        assert!(items_columns(&fresh).contains(&"description".to_string()));
        fresh
            .execute(
                "INSERT INTO items(title,quadrant,created_at,updated_at) \
                 VALUES('дело','q1',1.0,1.0)",
                [],
            )
            .unwrap();
        fresh
            .execute(
                "INSERT INTO item_files(item_id, path, label, sort) VALUES(1, '/tmp/x', NULL, 0)",
                [],
            )
            .unwrap();
        // re-apply twice: no duplicate column / table errors, data intact
        apply_schema(&fresh).unwrap();
        apply_schema(&fresh).unwrap();
        assert_eq!(
            items_columns(&fresh)
                .iter()
                .filter(|c| *c == "description")
                .count(),
            1
        );
        let n: i64 = fresh
            .query_row("SELECT COUNT(*) FROM item_files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);

        // legacy v2 DB: items without `description`, one existing row
        let legacy = Connection::open_in_memory().unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, \
                 quadrant TEXT NOT NULL CHECK (quadrant IN ('q1','q2','q3','q4')), project TEXT, \
                 done INTEGER NOT NULL DEFAULT 0, created_at REAL NOT NULL, updated_at REAL NOT NULL);
                 INSERT INTO items(title,quadrant,done,created_at,updated_at) \
                 VALUES('дело без брифа','q2',0,1.0,1.0);
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        assert!(!items_columns(&legacy).contains(&"description".to_string()));
        apply_schema(&legacy).unwrap();
        assert!(items_columns(&legacy).contains(&"description".to_string()));
        let (title, desc): (String, Option<String>) = legacy
            .query_row("SELECT title, description FROM items", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(title, "дело без брифа");
        assert_eq!(desc, None, "existing rows start with a null brief");
        apply_schema(&legacy).unwrap();
        assert_eq!(
            items_columns(&legacy)
                .iter()
                .filter(|c| *c == "description")
                .count(),
            1
        );
    }

    /// «Этап B»: the eight `items.delegation_*` columns migrate like the rest —
    /// a fresh DB carries them from schema.sql, a legacy v4 DB gets every one
    /// through the guarded ALTER (existing rows read "never delegated"), and
    /// re-applying is a no-op.
    #[test]
    fn delegation_columns_migrate_idempotently() {
        for (col, _) in DELEGATION_COLUMNS {
            let fresh = Connection::open_in_memory().unwrap();
            apply_schema(&fresh).unwrap();
            assert!(
                items_columns(&fresh).contains(&col.to_string()),
                "fresh DB misses {col}"
            );
        }

        let legacy = Connection::open_in_memory().unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, \
                 quadrant TEXT NOT NULL CHECK (quadrant IN ('q1','q2','q3','q4')), project TEXT, \
                 done INTEGER NOT NULL DEFAULT 0, description TEXT, dedupe_key TEXT, \
                 created_at REAL NOT NULL, updated_at REAL NOT NULL);
                 INSERT INTO items(title,quadrant,dedupe_key,created_at,updated_at) \
                 VALUES('старое дело','q1','old-key',1.0,1.0);
                 PRAGMA user_version = 4;",
            )
            .unwrap();
        for (col, _) in DELEGATION_COLUMNS {
            assert!(!items_columns(&legacy).contains(&col.to_string()));
        }
        apply_schema(&legacy).unwrap();
        apply_schema(&legacy).unwrap();
        let cols = items_columns(&legacy);
        for (col, _) in DELEGATION_COLUMNS {
            assert_eq!(
                cols.iter().filter(|c| *c == col).count(),
                1,
                "exactly one {col} after re-apply"
            );
        }
        let (title, state): (String, String) = legacy
            .query_row("SELECT title, delegation_state FROM items", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((title.as_str(), state.as_str()), ("старое дело", ""));
        let targets: i64 = legacy
            .query_row(
                "SELECT COUNT(*) FROM items WHERE delegation_target IS NULL \
                 AND delegation_ref IS NULL AND delegation_run_id IS NULL \
                 AND delegation_started IS NULL AND delegation_ended IS NULL \
                 AND delegation_exit IS NULL AND delegation_feedback IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(targets, 1, "existing rows start with every ref null");
    }

    fn project_flags_columns(conn: &Connection) -> Vec<String> {
        let mut stmt = conn.prepare("PRAGMA table_info(project_flags)").unwrap();
        let mut rows = stmt.query([]).unwrap();
        let mut out = Vec::new();
        while let Some(r) = rows.next().unwrap() {
            out.push(r.get::<_, String>(1).unwrap());
        }
        out
    }

    /// Избранное: `project_flags.favorite` migrates like the rest — a fresh DB
    /// carries it from schema.sql, a legacy v5 DB gets the guarded ALTER
    /// (existing flag rows read "not favorite"), and re-applying is a no-op.
    #[test]
    fn favorite_migration_is_idempotent_on_fresh_and_legacy_db() {
        let fresh = Connection::open_in_memory().unwrap();
        apply_schema(&fresh).unwrap();
        assert!(project_flags_columns(&fresh).contains(&"favorite".to_string()));
        let ver: i64 = fresh
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, SCHEMA_VERSION);
        // re-applying twice changes nothing (no duplicate column error)
        apply_schema(&fresh).unwrap();
        apply_schema(&fresh).unwrap();
        assert_eq!(
            project_flags_columns(&fresh)
                .iter()
                .filter(|c| *c == "favorite")
                .count(),
            1
        );

        // legacy v5 DB: project_flags without `favorite` + an archived row
        let legacy = Connection::open_in_memory().unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE project_flags (name TEXT PRIMARY KEY, \
                 archived INTEGER NOT NULL DEFAULT 0, updated_at REAL NOT NULL DEFAULT 0);
                 INSERT INTO project_flags(name, archived) VALUES('Игры', 1);
                 PRAGMA user_version = 5;",
            )
            .unwrap();
        assert!(!project_flags_columns(&legacy).contains(&"favorite".to_string()));
        apply_schema(&legacy).unwrap();
        assert!(project_flags_columns(&legacy).contains(&"favorite".to_string()));
        let (name, archived, favorite): (String, i64, i64) = legacy
            .query_row("SELECT name, archived, favorite FROM project_flags", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(
            (name.as_str(), archived, favorite),
            ("Игры", 1, 0),
            "existing rows keep archived and start unfavorited"
        );
        let ver: i64 = legacy
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, SCHEMA_VERSION);
        // and it stays idempotent on the migrated DB
        apply_schema(&legacy).unwrap();
        assert_eq!(
            project_flags_columns(&legacy)
                .iter()
                .filter(|c| *c == "favorite")
                .count(),
            1
        );
    }
}
