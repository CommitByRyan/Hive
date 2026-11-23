use rusqlite::Connection;

use crate::error::{StoreError, StoreResult};

pub const SCHEMA_VERSION: i64 = 2;
pub const BUSY_TIMEOUT_MS: u64 = 5000;

pub fn apply_pragmas(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(&format!(
        "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout={BUSY_TIMEOUT_MS};"
    ))?;
    Ok(())
}

pub fn migrate(conn: &mut Connection) -> StoreResult<()> {
    let mut version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::Conflict(format!(
            "database schema version {version} is newer than supported {SCHEMA_VERSION}"
        )));
    }
    while version < SCHEMA_VERSION {
        let next = version + 1;
        let tx = conn.transaction()?;
        apply_migration(&tx, next)?;
        tx.pragma_update(None, "user_version", next)?;
        tx.commit()?;
        version = next;
    }
    Ok(())
}

fn apply_migration(conn: &Connection, target: i64) -> StoreResult<()> {
    match target {
        1 => conn.execute_batch(V1_SCHEMA)?,
        2 => conn.execute_batch(V2_SCHEMA)?,
        other => {
            return Err(StoreError::Conflict(format!(
                "unknown migration version {other}"
            )))
        }
    }
    Ok(())
}

pub(crate) const V1_SCHEMA: &str = r#"
CREATE TABLE missions (
    mission_id    TEXT PRIMARY KEY,
    state         TEXT NOT NULL,
    title         TEXT NOT NULL DEFAULT '',
    goal          TEXT NOT NULL DEFAULT '',
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE runs (
    run_id        TEXT PRIMARY KEY,
    mission_id    TEXT NOT NULL REFERENCES missions(mission_id) ON DELETE CASCADE,
    state         TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_runs_mission ON runs(mission_id);

CREATE TABLE nodes (
    node_id             TEXT PRIMARY KEY,
    mission_id          TEXT NOT NULL REFERENCES missions(mission_id) ON DELETE CASCADE,
    run_id              TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    state               TEXT NOT NULL,
    attempt_no          INTEGER NOT NULL DEFAULT 1,
    retry_budget        INTEGER NOT NULL DEFAULT 3,
    lease_token         TEXT,
    lease_owner         TEXT,
    lease_expires_at_ms INTEGER,
    created_at_ms       INTEGER NOT NULL,
    updated_at_ms       INTEGER NOT NULL
);
CREATE INDEX idx_nodes_mission ON nodes(mission_id);
CREATE INDEX idx_nodes_run ON nodes(run_id);
CREATE INDEX idx_nodes_lease ON nodes(lease_expires_at_ms) WHERE lease_expires_at_ms IS NOT NULL;

CREATE TABLE approvals (
    approval_id        TEXT PRIMARY KEY,
    mission_id         TEXT NOT NULL REFERENCES missions(mission_id) ON DELETE CASCADE,
    run_id             TEXT,
    node_id            TEXT NOT NULL REFERENCES nodes(node_id) ON DELETE CASCADE,
    session_id         TEXT,
    tool_call_id       TEXT NOT NULL,
    tool_name          TEXT NOT NULL,
    tool_version       INTEGER NOT NULL,
    request_hash       TEXT NOT NULL,
    arguments_preview  TEXT NOT NULL,
    policy_version     TEXT NOT NULL,
    attempt_no         INTEGER NOT NULL,
    lease_token        TEXT NOT NULL,
    state              TEXT NOT NULL CHECK (state IN ('pending','approved','rejected','expired','cancelled')),
    expires_at_ms      INTEGER NOT NULL,
    created_at_ms      INTEGER NOT NULL,
    decided_at_ms      INTEGER,
    decided_by         TEXT,
    decision_note      TEXT,
    consumed_at_ms     INTEGER,
    UNIQUE (tool_call_id)
);
CREATE INDEX approvals_pending_expiry ON approvals(state, expires_at_ms);
CREATE INDEX approvals_by_node ON approvals(node_id, created_at_ms);

CREATE TABLE events (
    seq            INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id       TEXT NOT NULL UNIQUE,
    mission_id     TEXT NOT NULL,
    kind           TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    envelope_json  TEXT NOT NULL,
    created_at_ms  INTEGER NOT NULL
);
CREATE INDEX idx_events_mission_seq ON events(mission_id, seq);

CREATE TABLE snapshots (
    snapshot_id    TEXT PRIMARY KEY,
    schema_version INTEGER NOT NULL,
    mission_id     TEXT NOT NULL REFERENCES missions(mission_id) ON DELETE CASCADE,
    run_id         TEXT,
    node_id        TEXT,
    kind           TEXT NOT NULL,
    last_seq       INTEGER NOT NULL,
    payload_json   TEXT NOT NULL,
    created_at_ms  INTEGER NOT NULL
);
CREATE INDEX idx_snapshots_mission ON snapshots(mission_id);
CREATE INDEX idx_snapshots_lookup ON snapshots(mission_id, kind, node_id, last_seq);
"#;

const V2_SCHEMA: &str = r#"
CREATE TABLE tool_calls (
    tool_call_id   TEXT PRIMARY KEY,
    mission_id     TEXT NOT NULL REFERENCES missions(mission_id) ON DELETE CASCADE,
    run_id         TEXT,
    node_id        TEXT NOT NULL REFERENCES nodes(node_id) ON DELETE CASCADE,
    session_id     TEXT,
    tool_name      TEXT NOT NULL,
    tool_version   INTEGER NOT NULL,
    request_hash   TEXT NOT NULL,
    attempt_no     INTEGER NOT NULL,
    lease_token    TEXT NOT NULL,
    state          TEXT NOT NULL CHECK (state IN ('intent','started','completed','failed')),
    summary        TEXT NOT NULL DEFAULT '',
    truncated      INTEGER NOT NULL DEFAULT 0,
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
);
CREATE INDEX idx_tool_calls_node ON tool_calls(node_id, created_at_ms);
CREATE INDEX idx_tool_calls_mission ON tool_calls(mission_id, created_at_ms);

CREATE TABLE artifacts (
    artifact_id    TEXT PRIMARY KEY,
    mission_id     TEXT NOT NULL REFERENCES missions(mission_id) ON DELETE CASCADE,
    node_id        TEXT,
    tool_call_id   TEXT,
    path           TEXT NOT NULL,
    sha256         TEXT NOT NULL,
    size_bytes     INTEGER NOT NULL,
    created_at_ms  INTEGER NOT NULL
);
CREATE INDEX idx_artifacts_mission ON artifacts(mission_id, created_at_ms);

CREATE TABLE tool_audit (
    audit_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    mission_id     TEXT NOT NULL,
    tool_call_id   TEXT NOT NULL,
    event_kind     TEXT NOT NULL,
    record_json    TEXT NOT NULL,
    prev_hash      TEXT NOT NULL,
    entry_hash     TEXT NOT NULL,
    created_at_ms  INTEGER NOT NULL
);
CREATE INDEX idx_tool_audit_mission ON tool_audit(mission_id, audit_id);
"#;
