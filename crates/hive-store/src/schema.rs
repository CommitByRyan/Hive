use rusqlite::Connection;

use crate::error::{StoreError, StoreResult};

pub const SCHEMA_VERSION: i64 = 6;
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
        3 => conn.execute_batch(V3_SCHEMA)?,
        4 => conn.execute_batch(V4_SCHEMA)?,
        5 => conn.execute_batch(V5_SCHEMA)?,
        6 => conn.execute_batch(V6_SCHEMA)?,
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

pub(crate) const V2_SCHEMA: &str = r#"
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

pub(crate) const V3_SCHEMA: &str = r#"
CREATE TABLE agent_sessions (
    session_id     TEXT PRIMARY KEY,
    mission_id     TEXT NOT NULL REFERENCES missions(mission_id) ON DELETE CASCADE,
    run_id         TEXT REFERENCES runs(run_id) ON DELETE CASCADE,
    node_id        TEXT NOT NULL REFERENCES nodes(node_id) ON DELETE CASCADE,
    role           TEXT NOT NULL,
    model          TEXT NOT NULL,
    system_prompt  TEXT NOT NULL DEFAULT '',
    state          TEXT NOT NULL CHECK (state IN ('active','completed','failed','cancelled')),
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL,
    closed_at_ms   INTEGER
);
CREATE INDEX idx_agent_sessions_node ON agent_sessions(node_id, created_at_ms);
CREATE INDEX idx_agent_sessions_mission ON agent_sessions(mission_id, created_at_ms);

CREATE TABLE turns (
    turn_id         TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES agent_sessions(session_id) ON DELETE CASCADE,
    node_id         TEXT NOT NULL REFERENCES nodes(node_id) ON DELETE CASCADE,
    attempt_no      INTEGER NOT NULL,
    turn_index      INTEGER NOT NULL,
    request_model   TEXT NOT NULL,
    finish_reason   TEXT,
    state           TEXT NOT NULL CHECK (state IN ('started','completed','failed','cancelled')),
    output_preview  TEXT NOT NULL DEFAULT '',
    tool_call_count INTEGER NOT NULL DEFAULT 0,
    created_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL,
    UNIQUE (session_id, turn_index)
);
CREATE INDEX idx_turns_session ON turns(session_id, turn_index);
CREATE INDEX idx_turns_node ON turns(node_id, attempt_no);

CREATE TABLE turn_usage (
    turn_id           TEXT PRIMARY KEY REFERENCES turns(turn_id) ON DELETE CASCADE,
    session_id        TEXT NOT NULL REFERENCES agent_sessions(session_id) ON DELETE CASCADE,
    prompt_tokens     INTEGER NOT NULL,
    completion_tokens INTEGER NOT NULL,
    total_tokens      INTEGER NOT NULL,
    source            TEXT NOT NULL CHECK (source IN ('estimated','authoritative')),
    request_id        TEXT,
    created_at_ms     INTEGER NOT NULL
);
CREATE INDEX idx_turn_usage_session ON turn_usage(session_id);
"#;

pub(crate) const V4_SCHEMA: &str = r#"
CREATE TABLE handoffs (
    handoff_id            TEXT PRIMARY KEY,
    mission_id            TEXT NOT NULL REFERENCES missions(mission_id) ON DELETE CASCADE,
    run_id                TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    node_id               TEXT NOT NULL REFERENCES nodes(node_id) ON DELETE CASCADE,
    attempt_no            INTEGER NOT NULL,
    agent                 TEXT NOT NULL,
    schema_version        INTEGER NOT NULL,
    objective             TEXT NOT NULL,
    summary               TEXT NOT NULL,
    base_revision         TEXT NOT NULL,
    created_at_ms         INTEGER NOT NULL,
    inputs_json           TEXT NOT NULL DEFAULT '[]',
    artifacts_json        TEXT NOT NULL DEFAULT '[]',
    decisions_json        TEXT NOT NULL DEFAULT '[]',
    assumptions_json      TEXT NOT NULL DEFAULT '[]',
    risks_json            TEXT NOT NULL DEFAULT '[]',
    open_questions_json   TEXT NOT NULL DEFAULT '[]',
    acceptance_json       TEXT NOT NULL DEFAULT '[]',
    recommended_next_json TEXT NOT NULL DEFAULT '[]',
    UNIQUE (node_id, attempt_no)
);
CREATE INDEX idx_handoffs_mission ON handoffs(mission_id, created_at_ms);
"#;

pub(crate) const V5_SCHEMA: &str = r#"
CREATE TABLE budgets (
    scope_kind    TEXT NOT NULL CHECK (scope_kind IN ('mission','run','node')),
    scope_id      TEXT NOT NULL,
    dimension     TEXT NOT NULL CHECK (dimension IN ('tokens','cost','requests','tool_calls','wall_ms')),
    currency      TEXT,
    hard_limit    INTEGER,
    soft_limit    INTEGER,
    consumed      INTEGER NOT NULL DEFAULT 0,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (scope_kind, scope_id, dimension)
);
CREATE INDEX idx_budgets_scope ON budgets(scope_kind, scope_id);
"#;

pub(crate) const V6_SCHEMA: &str = r#"
CREATE TABLE routines (
    routine_id    TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    goal          TEXT NOT NULL,
    schedule      TEXT NOT NULL,
    enabled       INTEGER NOT NULL DEFAULT 1,
    timezone      TEXT NOT NULL DEFAULT 'Asia/Shanghai',
    last_run_ms   INTEGER,
    next_run_ms   INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_routines_due ON routines(enabled, next_run_ms);

CREATE TABLE routine_fires (
    routine_id    TEXT NOT NULL REFERENCES routines(routine_id) ON DELETE CASCADE,
    fire_at_ms    INTEGER NOT NULL,
    mission_id    TEXT,
    state         TEXT NOT NULL CHECK (state IN ('created','failed')),
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (routine_id, fire_at_ms)
);
"#;
