-- Atlas control plane schema. Applied idempotently at startup (CREATE IF NOT EXISTS + PRAGMA user_version).

CREATE TABLE IF NOT EXISTS projects (
    name                TEXT PRIMARY KEY,
    path                TEXT,
    vault_path          TEXT,
    zen_space           TEXT,
    memory_bank         TEXT,
    herdr_workspace_id  TEXT,
    agent_status        TEXT,
    agents_count        INTEGER NOT NULL DEFAULT 0,
    sessions_count      INTEGER NOT NULL DEFAULT 0,
    last_activity       REAL NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS links (
    from_project TEXT NOT NULL REFERENCES projects(name),
    to_project   TEXT NOT NULL REFERENCES projects(name),
    kind         TEXT NOT NULL CHECK (kind IN ('strict','soft')),
    note         TEXT,
    PRIMARY KEY (from_project, to_project, kind)
);

CREATE TABLE IF NOT EXISTS items (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    title      TEXT NOT NULL,
    quadrant   TEXT NOT NULL CHECK (quadrant IN ('q1','q2','q3','q4')),
    project    TEXT,
    -- «Дела»: 1 = закрыто (выполнено). Closed items stay in their quadrant
    -- arrays (the UI splits them) but leave search_docs.
    done       INTEGER NOT NULL DEFAULT 0,
    -- Task brief («Этап A»): delegated work description, trimmed, ≤10000 chars.
    description TEXT,
    -- Idempotent-retry key for `atlas board add --dedupe-key` («Этап B»):
    -- the CLI re-adding a task with the same key gets the existing id back.
    dedupe_key TEXT,
    -- «Этап B»: delegation of this item to a worker (see README «Делегация»).
    -- state: '' (never delegated) | queued | running | review | failed | done;
    -- target: orchestrator | worktree | dir; ref: worktree dir / project path
    -- (atlas dir for the orchestrator turn); *_run_id/started/ended/exit mirror
    -- the in-process run; feedback feeds the next brief's «Фидбек» section.
    delegation_state    TEXT NOT NULL DEFAULT '',
    delegation_target   TEXT,
    delegation_ref      TEXT,
    delegation_run_id   INTEGER,
    delegation_started  REAL,
    delegation_ended    REAL,
    delegation_exit     INTEGER,
    delegation_feedback TEXT,
    -- Project repo HEAD sha at delegation start: the review gate diffs the
    -- worker's changes against THIS, never against a moving `HEAD` — work the
    -- worker committed is visible, foreign edits are not mixed in.
    delegation_head     TEXT,
    -- The worker's final message (the СДЕЛАНО/ИЗМЕНЕНО/ПРОВЕРЕНО/ВОПРОСЫ
    -- contract block), written when the turn settles — the review card shows
    -- it above the diff; it survives a server restart.
    delegation_final    TEXT,
    -- Session file this run used (rpc child or headless worker): the resume
    -- base for a true follow-up (feedback exists → continue THIS item's
    -- session), never the project's latest session.
    delegation_session  TEXT,
    -- Last ~50 ring lines of the run, written at ANY terminal — a server
    -- restart must not erase the evidence of a run (the in-memory ring dies
    -- with the process).
    delegation_tail     TEXT,
    -- Proof-of-work ledger of the run's tool calls (rpc runs only): a JSON
    -- array of {command, error} — the review card shows «команд: N (падений:
    -- M)» and /run exposes count+last 20. NULL = no ledger (process targets,
    -- legacy rows).
    delegation_cmds     TEXT,
    -- The fresh-context reviewer's answer (`[delegation] reviewer`, off by
    -- default): a SECOND cold `omp -p` session re-reads the frozen diff after
    -- the run settles in review and answers the VERDICT: accept|block
    -- contract. Advisory only — no state change, the owner keeps the veto.
    delegation_review   TEXT,
    -- Autopilot retry counter of the current cycle: auto-reject increments,
    -- a manual reject / accept / reset zeroes («рули я»).
    delegation_attempts INTEGER NOT NULL DEFAULT 0,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL
);

-- Attachment paths of a board item (v1 semantics: PUT replaces the whole set).
-- References only — atlas never touches the files themselves.
CREATE TABLE IF NOT EXISTS item_files (
    item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    path    TEXT NOT NULL,
    label   TEXT,
    sort    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (item_id, path)
);

-- Checklist steps of a board item («Этап B»: PUT replaces the whole set).
-- pos keeps the PUT order; done is the frontend's completion counter input.
CREATE TABLE IF NOT EXISTS item_steps (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    text    TEXT NOT NULL,
    done    INTEGER NOT NULL DEFAULT 0,
    pos     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS sources (
    name      TEXT PRIMARY KEY,
    count     INTEGER NOT NULL DEFAULT 0,
    ok        INTEGER NOT NULL DEFAULT 1,
    last_sync REAL NOT NULL DEFAULT 0,
    meta      TEXT
);

CREATE TABLE IF NOT EXISTS sessions (
    slug          TEXT PRIMARY KEY,
    project       TEXT NOT NULL,
    dir_path      TEXT,
    count         INTEGER NOT NULL DEFAULT 0,
    latest_file   TEXT,
    latest_mtime  REAL NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS sessions_project ON sessions(project);

CREATE TABLE IF NOT EXISTS search_docs (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    title      TEXT,
    body       TEXT,
    source_ref TEXT
);

CREATE VIRTUAL TABLE IF NOT EXISTS fts_docs USING fts5 (
    title, body,
    kind       UNINDEXED,
    source_ref UNINDEXED,
    content='search_docs', content_rowid='rowid'
);

CREATE TRIGGER IF NOT EXISTS search_docs_ai AFTER INSERT ON search_docs BEGIN
    INSERT INTO fts_docs(rowid, title, body, kind, source_ref)
    VALUES (new.rowid, new.title, new.body, new.kind, new.source_ref);
END;

CREATE TRIGGER IF NOT EXISTS search_docs_ad AFTER DELETE ON search_docs BEGIN
    INSERT INTO fts_docs(fts_docs, rowid, title, body, kind, source_ref)
    VALUES ('delete', old.rowid, old.title, old.body, old.kind, old.source_ref);
END;

CREATE TRIGGER IF NOT EXISTS search_docs_au AFTER UPDATE ON search_docs BEGIN
    INSERT INTO fts_docs(fts_docs, rowid, title, body, kind, source_ref)
    VALUES ('delete', old.rowid, old.title, old.body, old.kind, old.source_ref);
    INSERT INTO fts_docs(rowid, title, body, kind, source_ref)
    VALUES (new.rowid, new.title, new.body, new.kind, new.source_ref);
END;

CREATE TABLE IF NOT EXISTS embeddings (
    doc_id  TEXT PRIMARY KEY REFERENCES search_docs(id) ON DELETE CASCADE,
    model   TEXT NOT NULL,
    dim     INTEGER NOT NULL,
    vec     BLOB NOT NULL,
    hash    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Archive / favorite flags of a project. Never deletes: projects is
-- recomputed by sync; the flags live here and survive resyncs. `favorite`
-- pins the project to the TOP of the projects list.
CREATE TABLE IF NOT EXISTS project_flags (
    name       TEXT PRIMARY KEY,
    archived   INTEGER NOT NULL DEFAULT 0,
    favorite   INTEGER NOT NULL DEFAULT 0,
    updated_at REAL NOT NULL DEFAULT 0
);

-- Key-value store for UI settings (values are JSON scalars) and window state
-- (ui.tabs — JSON array of panel labels minimised into the host window).
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Per-project UI metadata the sync never recomputes (currently the "Просмотр"
-- iframe URL): the importer upserts `projects` wholesale, so user-set values
-- must live beside it, not inside it.
CREATE TABLE IF NOT EXISTS project_meta (
    name       TEXT PRIMARY KEY REFERENCES projects(name),
    web_url    TEXT,
    updated_at REAL NOT NULL DEFAULT 0
);

-- Feedback from agents (the orchestrator) into the UI. Ring buffer: inserts
-- trim to the newest 200 rows.
CREATE TABLE IF NOT EXISTS notifications (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    ts      REAL NOT NULL,
    level   TEXT NOT NULL DEFAULT 'info' CHECK (level IN ('info','warn','err')),
    project TEXT,
    text    TEXT NOT NULL,
    read    INTEGER NOT NULL DEFAULT 0
);
