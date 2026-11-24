use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use hive_types::{
    AgentSessionId, ApprovalId, ApprovalState, AttemptNo, EventEnvelope, EventId, EventInput,
    EventKind, Handoff, HandoffId, LeaseToken, MissionId, MissionState, NodeId, NodeState,
    RoutineId, RunId, RunState, ToolCallId, TurnId, EVENT_SCHEMA_VERSION, SYSTEM_SCOPE_MISSION_ID,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::records::{
    AgentSessionRecord, AgentSessionState, ApprovalDecision, ApprovalInput, ApprovalRecord,
    ArtifactRecord, BudgetDimension, BudgetLimits, BudgetRecord, BudgetSummary, HandoffRecord,
    LeaseGuard, MissionAggregate, MissionListRecord, MissionRecord, MissionSnapshotRecord,
    NewAgentSession, NewTurnUsage, NodeRecord, RoutineRecord, RunRecord, SideEffectState,
    SnapshotInput, SnapshotRecord, StateSnapshotRecord, ToolAuditRecord, TurnRecord, TurnState,
    TurnUsageRecord, UsageSource,
};
use crate::routine;
use crate::schema;
use crate::time::{now_ms, now_rfc3339};
use crate::{MAX_SEQ, SNAPSHOT_SCHEMA_VERSION};

const MAX_ROUTINES: u64 = 50;

const DEFAULT_RETENTION_DAYS: i64 = 30;

#[derive(Debug)]
pub struct Store {
    conn: Connection,
    /// Lowest sequence that a client can safely resume from after retention.
    /// Zero means no archive has happened yet.
    retained_floor: u64,
    /// Sidecar that persists [`Store::retained_floor`] across restarts.
    retention_path: Option<PathBuf>,
}

/// Result of attempting to fire one routine window.
#[derive(Debug)]
pub enum RoutineFireOutcome {
    /// A Mission was created and `routine.triggered` was emitted.
    Fired {
        mission: MissionRecord,
        fire_at_ms: i64,
        envelope: EventEnvelope,
    },
    /// The window was already claimed by another tick.
    AlreadyFired,
    /// The routine is not due yet (or has been disabled).
    NotDue,
    /// The routine no longer exists.
    NotFound,
    /// The stored schedule became invalid; the row was disabled and
    /// `routine.error` was emitted.
    Disabled { envelope: EventEnvelope },
    /// Mission creation failed; the window was recorded as failed without
    /// replaying it.
    Failed { fire_at_ms: i64 },
}

#[derive(Clone, Debug)]
pub struct ArchiveReport {
    pub mission_id: MissionId,
    pub events_archived: u64,
    pub audits_archived: u64,
    pub retained_since_seq: u64,
    pub archive_path: PathBuf,
}

impl Store {
    pub fn open(path: &Path) -> StoreResult<Self> {
        let retention_path = path.parent().map(|parent| parent.join("retention.json"));
        let retained_floor = match retention_path.as_deref() {
            Some(retention_path) => load_retention_floor(retention_path)?,
            None => 0,
        };
        let conn = Connection::open(path)?;
        Self::from_connection(conn, retention_path, retained_floor)
    }

    pub fn open_in_memory() -> StoreResult<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn, None, 0)
    }

    fn from_connection(
        conn: Connection,
        retention_path: Option<PathBuf>,
        retained_floor: u64,
    ) -> StoreResult<Self> {
        schema::apply_pragmas(&conn)?;
        let mut conn = conn;
        schema::migrate(&mut conn)?;
        Ok(Self {
            conn,
            retained_floor,
            retention_path,
        })
    }

    pub fn journal_mode(&self) -> StoreResult<String> {
        let mode = self
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        Ok(mode)
    }

    pub fn archive_expired_missions(
        &mut self,
        archive_root: &Path,
        now_ms: i64,
    ) -> StoreResult<Vec<ArchiveReport>> {
        self.archive_expired_missions_with_days(archive_root, now_ms, DEFAULT_RETENTION_DAYS)
    }

    /// Archive terminal missions whose `updated_at_ms` is older than `days`.
    ///
    /// `days == 0` disables automatic archiving and never deletes rows. A
    /// mission that no longer owns events, audit rows or snapshots is skipped,
    /// so repeated periodic passes are idempotent.
    pub fn archive_expired_missions_with_days(
        &mut self,
        archive_root: &Path,
        now_ms: i64,
        days: i64,
    ) -> StoreResult<Vec<ArchiveReport>> {
        if days == 0 {
            return Ok(Vec::new());
        }
        if days < 0 {
            return Err(StoreError::Archive(
                "retention days must be non-negative".to_owned(),
            ));
        }
        let retention_ms = days
            .checked_mul(24 * 60 * 60 * 1000)
            .ok_or_else(|| StoreError::Archive("retention window overflow".to_owned()))?;
        let threshold_ms = now_ms
            .checked_sub(retention_ms)
            .ok_or_else(|| StoreError::Archive("retention threshold underflow".to_owned()))?;
        let mission_ids: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT mission_id FROM missions WHERE state IN ('done','failed','cancelled') \
                 AND updated_at_ms < ?1 \
                 AND (EXISTS (SELECT 1 FROM events WHERE events.mission_id = missions.mission_id) \
                   OR EXISTS (SELECT 1 FROM tool_audit WHERE tool_audit.mission_id = missions.mission_id) \
                   OR EXISTS (SELECT 1 FROM snapshots WHERE snapshots.mission_id = missions.mission_id)) \
                 ORDER BY updated_at_ms ASC",
            )?;
            let rows = stmt.query_map(params![threshold_ms], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };

        let mut reports = Vec::new();
        for mission_id in mission_ids {
            let mission_id = MissionId::parse(&mission_id)?;
            reports.push(self.archive_mission(archive_root, mission_id, now_ms)?);
        }
        Ok(reports)
    }

    pub fn archive_mission(
        &mut self,
        archive_root: &Path,
        mission_id: MissionId,
        now_ms: i64,
    ) -> StoreResult<ArchiveReport> {
        let mission = self
            .get_mission(mission_id)?
            .ok_or_else(|| StoreError::NotFound(format!("mission {mission_id}")))?;
        if !mission.state.is_terminal() {
            return Err(StoreError::Archive(format!(
                "mission {mission_id} is active"
            )));
        }

        let mission_dir = archive_root.join(mission_id.to_string());
        std::fs::create_dir_all(&mission_dir)
            .map_err(|error| StoreError::Archive(error.to_string()))?;
        let events_path = mission_dir.join("events.jsonl");

        let envelope_rows: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT seq, envelope_json FROM events WHERE mission_id = ?1 ORDER BY seq ASC",
            )?;
            let rows = stmt.query_map(params![mission_id.to_string()], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };

        let audit_rows: Vec<(i64, String, String, String, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT audit_id, tool_call_id, record_json, prev_hash, entry_hash FROM tool_audit WHERE mission_id = ?1 ORDER BY audit_id ASC",
            )?;
            let rows = stmt.query_map(params![mission_id.to_string()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };

        let mut writer = BufWriter::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&events_path)
                .map_err(|error| StoreError::Archive(error.to_string()))?,
        );
        let segment_genesis_hash = audit_rows
            .first()
            .map(|row| row.3.clone())
            .unwrap_or_else(|| "GENESIS".to_owned());
        let archive_header = serde_json::json!({
            "mission_id": mission_id.to_string(),
            "archived_at_ms": now_ms,
            "event_count": envelope_rows.len(),
            "audit_count": audit_rows.len(),
            "segment_genesis_hash": segment_genesis_hash,
        });
        writer
            .write_all(archive_header.to_string().as_bytes())
            .and_then(|_| writer.write_all(b"\n"))
            .map_err(io_archive_error)?;
        for (_, envelope_json) in envelope_rows.iter() {
            writer
                .write_all(envelope_json.as_bytes())
                .and_then(|_| writer.write_all(b"\n"))
                .map_err(io_archive_error)?;
        }
        for (_, _, record_json, prev_hash, entry_hash) in audit_rows.iter() {
            let archived_audit = serde_json::json!({
                "record_json": record_json,
                "prev_hash": prev_hash,
                "entry_hash": entry_hash,
            });
            writer
                .write_all(archived_audit.to_string().as_bytes())
                .and_then(|_| writer.write_all(b"\n"))
                .map_err(io_archive_error)?;
        }
        writer.flush().map_err(io_archive_error)?;
        let file = writer
            .into_inner()
            .map_err(|_| StoreError::Archive("failed to flush archive writer".to_owned()))?;
        file.sync_all().map_err(io_archive_error)?;

        let boundary = envelope_rows
            .iter()
            .map(|row| row.0.saturating_add(1) as u64)
            .max()
            .unwrap_or(0);
        let new_floor = self.retained_floor.max(boundary);
        // Persist before deleting: a failed sidecar write must leave every row
        // in place. If deletion fails after this point the floor stays ahead,
        // which only forces clients to fetch a fresh snapshot.
        if new_floor > self.retained_floor {
            self.persist_retained_floor(new_floor)?;
            self.retained_floor = new_floor;
        }

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM events WHERE mission_id = ?1",
            params![mission_id.to_string()],
        )?;
        tx.execute(
            "DELETE FROM tool_audit WHERE mission_id = ?1",
            params![mission_id.to_string()],
        )?;
        tx.execute(
            "DELETE FROM snapshots WHERE mission_id = ?1",
            params![mission_id.to_string()],
        )?;
        tx.commit()?;

        Ok(ArchiveReport {
            mission_id,
            events_archived: envelope_rows.len() as u64,
            audits_archived: audit_rows.len() as u64,
            retained_since_seq: self.retained_floor,
            archive_path: events_path,
        })
    }

    pub fn verify_archived_mission_audit(
        &self,
        archive_path: &Path,
        mission_id: MissionId,
    ) -> StoreResult<bool> {
        let lines: Vec<String> = std::fs::read_to_string(archive_path)
            .map_err(|error| StoreError::Archive(error.to_string()))?
            .lines()
            .map(str::to_owned)
            .collect();
        let Some((header_json, rest)) = lines.split_first() else {
            return Ok(false);
        };
        let header: serde_json::Value = serde_json::from_str(header_json)?;
        if header["mission_id"] != mission_id.to_string() {
            return Ok(false);
        }
        let event_count = header["event_count"]
            .as_u64()
            .ok_or_else(|| StoreError::Archive("invalid archive event_count".to_owned()))?
            as usize;
        if rest.len() < event_count {
            return Ok(false);
        }
        let audit_records: Vec<&str> = rest[event_count..].iter().map(String::as_str).collect();
        let expected_audit_count = header["audit_count"].as_u64().unwrap_or_default() as usize;
        if audit_records.len() != expected_audit_count {
            return Ok(false);
        }
        let mut expected_prev = header["segment_genesis_hash"]
            .as_str()
            .unwrap_or("GENESIS")
            .to_owned();
        for audit_json in audit_records {
            let audit: serde_json::Value = serde_json::from_str(audit_json)?;
            let record_json = audit["record_json"]
                .as_str()
                .ok_or_else(|| StoreError::Archive("missing record_json".to_owned()))?;
            let prev_hash = audit["prev_hash"]
                .as_str()
                .ok_or_else(|| StoreError::Archive("missing prev_hash".to_owned()))?;
            let entry_hash = audit["entry_hash"]
                .as_str()
                .ok_or_else(|| StoreError::Archive("missing entry_hash".to_owned()))?;
            if prev_hash != expected_prev
                || audit_entry_hash(&expected_prev, record_json) != entry_hash
            {
                return Ok(false);
            }
            expected_prev = entry_hash.to_owned();
        }
        Ok(true)
    }

    pub fn retained_since_seq(&self) -> StoreResult<u64> {
        Ok(self.retained_floor)
    }

    fn persist_retained_floor(&self, floor: u64) -> StoreResult<()> {
        let Some(path) = self.retention_path.as_deref() else {
            return Ok(());
        };
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                StoreError::Retention(format!(
                    "failed to create retention directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("retention.json");
        let temp = path.with_file_name(format!(".{file_name}.hive-tmp-{}", std::process::id()));
        let payload = json!({ "retained_since_seq": floor }).to_string();
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temp)
                .map_err(|error| {
                    StoreError::Retention(format!(
                        "failed to open temporary retention file {}: {error}",
                        temp.display()
                    ))
                })?;
            file.write_all(payload.as_bytes())
                .and_then(|_| file.write_all(b"\n"))
                .map_err(|error| {
                    StoreError::Retention(format!(
                        "failed to write temporary retention file {}: {error}",
                        temp.display()
                    ))
                })?;
            file.sync_all().map_err(|error| {
                StoreError::Retention(format!(
                    "failed to sync temporary retention file {}: {error}",
                    temp.display()
                ))
            })?;
            drop(file);
            std::fs::rename(&temp, path).map_err(|error| {
                StoreError::Retention(format!(
                    "failed to replace retention sidecar {}: {error}",
                    path.display()
                ))
            })
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    pub fn schema_version(&self) -> StoreResult<i64> {
        let version = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        Ok(version)
    }

    pub fn create_mission(
        &mut self,
        title: &str,
        goal: &str,
        now_ms: i64,
    ) -> StoreResult<MissionRecord> {
        let record = MissionRecord {
            mission_id: MissionId::new(),
            state: MissionState::Queued,
            title: title.to_owned(),
            goal: goal.to_owned(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        self.conn.execute(
            "INSERT INTO missions (mission_id, state, title, goal, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![record.mission_id.to_string(), record.state.as_str(), record.title, record.goal, record.created_at_ms, record.updated_at_ms],
        )?;
        Ok(record)
    }

    pub fn create_run(&mut self, mission_id: MissionId, now_ms: i64) -> StoreResult<RunRecord> {
        let record = RunRecord {
            run_id: RunId::new(),
            mission_id,
            state: RunState::Created,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        self.conn.execute(
            "INSERT INTO runs (run_id, mission_id, state, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![record.run_id.to_string(), mission_id.to_string(), record.state.as_str(), record.created_at_ms, record.updated_at_ms],
        )?;
        Ok(record)
    }

    pub fn add_node(
        &mut self,
        mission_id: MissionId,
        run_id: RunId,
        retry_budget: u32,
        now_ms: i64,
    ) -> StoreResult<NodeRecord> {
        self.add_node_with_id(NodeId::new(), mission_id, run_id, retry_budget, now_ms)
    }

    /// Create a pending node with a caller-chosen id, so a plan can persist
    /// the exact node identity the scheduler drives.
    pub fn add_node_with_id(
        &mut self,
        node_id: NodeId,
        mission_id: MissionId,
        run_id: RunId,
        retry_budget: u32,
        now_ms: i64,
    ) -> StoreResult<NodeRecord> {
        let record = NodeRecord {
            node_id,
            mission_id,
            run_id,
            state: NodeState::Pending,
            attempt_no: AttemptNo::FIRST,
            retry_budget,
            lease_token: None,
            lease_owner: None,
            lease_expires_at_ms: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        self.conn.execute(
            "INSERT INTO nodes (node_id, mission_id, run_id, state, attempt_no, retry_budget, lease_token, lease_owner, lease_expires_at_ms, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                record.node_id.to_string(),
                mission_id.to_string(),
                run_id.to_string(),
                record.state.as_str(),
                record.attempt_no.get(),
                retry_budget,
                Option::<String>::None,
                Option::<String>::None,
                Option::<i64>::None,
                record.created_at_ms,
                record.updated_at_ms,
            ],
        )?;
        Ok(record)
    }

    pub fn get_mission(&self, mission_id: MissionId) -> StoreResult<Option<MissionRecord>> {
        query_mission(&self.conn, mission_id)
    }

    /// Advance a mission through its lifecycle. Transitions are validated
    /// against [`MissionState`] so a terminal mission cannot move again.
    pub fn transition_mission(
        &mut self,
        mission_id: MissionId,
        next: MissionState,
        now_ms: i64,
    ) -> StoreResult<MissionRecord> {
        let mission = self
            .get_mission(mission_id)?
            .ok_or_else(|| StoreError::NotFound(format!("mission {mission_id}")))?;
        mission.state.transition(next)?;
        self.conn.execute(
            "UPDATE missions SET state = ?1, updated_at_ms = ?2 WHERE mission_id = ?3",
            params![next.as_str(), now_ms, mission_id.to_string()],
        )?;
        self.get_mission(mission_id)?
            .ok_or_else(|| StoreError::NotFound(format!("mission {mission_id}")))
    }

    pub fn get_run(&self, run_id: RunId) -> StoreResult<Option<RunRecord>> {
        query_run(&self.conn, run_id)
    }

    pub fn get_node(&self, node_id: NodeId) -> StoreResult<Option<NodeRecord>> {
        query_node(&self.conn, node_id)
    }

    pub fn get_mission_aggregate(
        &self,
        mission_id: MissionId,
    ) -> StoreResult<Option<MissionAggregate>> {
        let Some(mission) = query_mission(&self.conn, mission_id)? else {
            return Ok(None);
        };
        let runs = query_runs(&self.conn, mission_id)?;
        let nodes = query_nodes_by_mission(&self.conn, mission_id)?;
        let last_seq: Option<i64> = self.conn.query_row(
            "SELECT MAX(seq) FROM events WHERE mission_id = ?1",
            params![mission_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(Some(MissionAggregate {
            mission,
            runs,
            nodes,
            last_seq: last_seq.map(|seq| seq as u64),
        }))
    }

    pub fn update_node_state(
        &mut self,
        node_id: NodeId,
        new_state: NodeState,
        now_ms: i64,
    ) -> StoreResult<NodeRecord> {
        update_node_state_on(&self.conn, node_id, new_state, now_ms)
    }

    pub fn lease_node(
        &mut self,
        node_id: NodeId,
        lease_token: &LeaseToken,
        lease_owner: &str,
        lease_expires_at_ms: i64,
        now_ms: i64,
    ) -> StoreResult<NodeRecord> {
        let current = query_node(&self.conn, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if current.state != NodeState::Ready {
            return Err(StoreError::Conflict(format!(
                "node {node_id} is {}, expected ready",
                current.state
            )));
        }
        let next_state = NodeState::Ready.transition(NodeState::Leased)?;
        self.conn.execute(
            "UPDATE nodes SET state = ?1, lease_token = ?2, lease_owner = ?3, lease_expires_at_ms = ?4, updated_at_ms = ?5 WHERE node_id = ?6",
            params![next_state.as_str(), lease_token.as_str(), lease_owner, lease_expires_at_ms, now_ms, node_id.to_string()],
        )?;
        query_node(&self.conn, node_id).map(|record| record.expect("node was just updated"))
    }

    pub fn renew_lease(
        &mut self,
        node_id: NodeId,
        lease_token: &LeaseToken,
        lease_owner: &str,
        lease_expires_at_ms: i64,
        now_ms: i64,
    ) -> StoreResult<NodeRecord> {
        let current = query_node(&self.conn, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if !matches!(current.state, NodeState::Leased | NodeState::Running) {
            return Err(StoreError::Conflict(format!(
                "node {node_id} is {}, expected leased or running",
                current.state
            )));
        }
        match &current.lease_token {
            Some(existing) if existing == lease_token => {}
            _ => return Err(StoreError::LeaseTokenMismatch),
        }
        self.conn.execute(
            "UPDATE nodes SET lease_owner = ?1, lease_expires_at_ms = ?2, updated_at_ms = ?3 WHERE node_id = ?4",
            params![lease_owner, lease_expires_at_ms, now_ms, node_id.to_string()],
        )?;
        query_node(&self.conn, node_id).map(|record| record.expect("node was just updated"))
    }

    /// Claim a ready node for execution: `ready -> leased`, stamping the next
    /// logical `attempt_no` and recording the new lease in one statement.
    ///
    /// This is the single place where `attempt_no` advances on the retry path:
    /// the previous attempt failed and left the node `ready` again, and the
    /// scheduler passes the next attempt number computed from the retry budget.
    /// The first claim keeps `attempt_no` at 1.
    pub fn claim_node(
        &mut self,
        node_id: NodeId,
        lease_token: &LeaseToken,
        lease_owner: &str,
        lease_expires_at_ms: i64,
        next_attempt_no: AttemptNo,
        now_ms: i64,
    ) -> StoreResult<NodeRecord> {
        let current = query_node(&self.conn, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if current.state != NodeState::Ready {
            return Err(StoreError::Conflict(format!(
                "node {node_id} is {}, expected ready",
                current.state
            )));
        }
        if next_attempt_no.get() < current.attempt_no.get() {
            return Err(StoreError::Conflict(format!(
                "node {node_id} attempt {} cannot regress to {}",
                current.attempt_no.get(),
                next_attempt_no.get()
            )));
        }
        let next_state = NodeState::Ready.transition(NodeState::Leased)?;
        self.conn.execute(
            "UPDATE nodes SET state = ?1, attempt_no = ?2, lease_token = ?3, lease_owner = ?4, lease_expires_at_ms = ?5, updated_at_ms = ?6 WHERE node_id = ?7",
            params![
                next_state.as_str(),
                next_attempt_no.get(),
                lease_token.as_str(),
                lease_owner,
                lease_expires_at_ms,
                now_ms,
                node_id.to_string()
            ],
        )?;
        query_node(&self.conn, node_id).map(|record| record.expect("node was just updated"))
    }

    pub fn append_event(&mut self, input: &EventInput) -> StoreResult<EventEnvelope> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let envelope = insert_event(&tx, input)?;
        tx.commit()?;
        Ok(envelope)
    }

    pub fn transition_node_and_emit(
        &mut self,
        node_id: NodeId,
        new_state: NodeState,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let node = update_node_state_on(&tx, node_id, new_state, now_ms)?;
        let envelope = insert_event(&tx, event)?;
        tx.commit()?;
        Ok((node, envelope))
    }

    /// Compare-and-set node transition guarded by the executor's lease.
    ///
    /// The guard must match the node's current `(attempt_no, lease_token)` and
    /// the event (when it carries `node_id`/`attempt_no`) must agree with the
    /// guard. All checks, the state update and the event insert run in one
    /// transaction, so a stale executor can never overwrite a newer attempt.
    pub fn transition_node_cas(
        &mut self,
        node_id: NodeId,
        guard: &LeaseGuard,
        next: NodeState,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = query_node(&tx, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if current.attempt_no != guard.attempt_no
            || current.lease_token.as_ref() != Some(&guard.lease_token)
        {
            return Err(StoreError::StaleExecution);
        }
        if let Some(event_node) = event.node_id {
            if event_node != node_id {
                return Err(StoreError::Conflict(format!(
                    "event node {event_node} does not match {node_id}"
                )));
            }
        }
        if let Some(event_attempt) = event.attempt_no {
            if event_attempt != guard.attempt_no {
                return Err(StoreError::Conflict(
                    "event attempt_no does not match lease guard".to_owned(),
                ));
            }
        }
        let next_state = current.state.transition(next)?;
        if next_state.is_terminal() {
            tx.execute(
                "UPDATE nodes SET state = ?1, lease_token = NULL, lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?2 WHERE node_id = ?3",
                params![next_state.as_str(), now_ms, node_id.to_string()],
            )?;
        } else {
            tx.execute(
                "UPDATE nodes SET state = ?1, updated_at_ms = ?2 WHERE node_id = ?3",
                params![next_state.as_str(), now_ms, node_id.to_string()],
            )?;
        }
        let envelope = insert_event(&tx, event)?;
        tx.commit()?;
        let node = query_node(&self.conn, node_id)?.expect("node was just updated");
        Ok((node, envelope))
    }

    pub fn read_events_after(&self, seq: u64) -> StoreResult<Vec<EventEnvelope>> {
        let seq = validate_cursor(seq)?;
        let mut stmt = self
            .conn
            .prepare("SELECT envelope_json FROM events WHERE seq > ?1 ORDER BY seq ASC")?;
        let rows = stmt.query_map(params![seq], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    /// Append an event that must be committed by the executor currently
    /// holding `guard`. Unlike [`Self::append_event`] this rejects stale
    /// attempts/tokens and terminal nodes in the same transaction.
    pub fn append_event_guarded(
        &mut self,
        node_id: NodeId,
        guard: &LeaseGuard,
        event: &EventInput,
    ) -> StoreResult<EventEnvelope> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = query_node(&tx, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if current.attempt_no != guard.attempt_no
            || current.lease_token.as_ref() != Some(&guard.lease_token)
        {
            return Err(StoreError::StaleExecution);
        }
        if current.state.is_terminal() {
            return Err(StoreError::StaleExecution);
        }
        if let Some(event_node) = event.node_id {
            if event_node != node_id {
                return Err(StoreError::Conflict(format!(
                    "event node {event_node} does not match {node_id}"
                )));
            }
        }
        if let Some(event_attempt) = event.attempt_no {
            if event_attempt != guard.attempt_no {
                return Err(StoreError::Conflict(
                    "event attempt_no does not match lease guard".to_owned(),
                ));
            }
        }
        let envelope = insert_event(&tx, event)?;
        tx.commit()?;
        Ok(envelope)
    }

    pub fn read_all_events(&self) -> StoreResult<Vec<EventEnvelope>> {
        let mut stmt = self
            .conn
            .prepare("SELECT envelope_json FROM events ORDER BY seq ASC")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    pub fn read_mission_events(
        &self,
        mission_id: MissionId,
        after_seq: u64,
    ) -> StoreResult<Vec<EventEnvelope>> {
        let after_seq = validate_cursor(after_seq)?;
        let mut stmt = self.conn.prepare(
            "SELECT envelope_json FROM events WHERE mission_id = ?1 AND seq > ?2 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![mission_id.to_string(), after_seq], |row| {
            row.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    pub fn latest_seq(&self) -> StoreResult<Option<u64>> {
        latest_seq_on(&self.conn)
    }

    pub fn save_snapshot(&mut self, input: &SnapshotInput<'_>) -> StoreResult<SnapshotRecord> {
        if input.last_seq > MAX_SEQ {
            return Err(StoreError::SequenceExhausted);
        }
        let last_seq_i64 =
            i64::try_from(input.last_seq).map_err(|_| StoreError::SequenceExhausted)?;
        let record = SnapshotRecord {
            snapshot_id: Uuid::new_v4().to_string(),
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            mission_id: input.mission_id,
            run_id: input.run_id,
            node_id: input.node_id,
            kind: input.kind.to_owned(),
            last_seq: input.last_seq,
            payload: input.payload.clone(),
            created_at_ms: input.now_ms,
        };
        let payload_json = serde_json::to_string(&record.payload)?;
        self.conn.execute(
            "INSERT INTO snapshots (snapshot_id, schema_version, mission_id, run_id, node_id, kind, last_seq, payload_json, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.snapshot_id.clone(),
                record.schema_version,
                input.mission_id.to_string(),
                input.run_id.map(|id| id.to_string()),
                input.node_id.map(|id| id.to_string()),
                record.kind.clone(),
                last_seq_i64,
                payload_json,
                input.now_ms,
            ],
        )?;
        Ok(record)
    }

    pub fn load_snapshot(
        &self,
        mission_id: MissionId,
        kind: &str,
        node_id: Option<NodeId>,
    ) -> StoreResult<Option<SnapshotRecord>> {
        let row = self.conn.query_row(
            "SELECT snapshot_id, schema_version, mission_id, run_id, node_id, kind, last_seq, payload_json, created_at_ms FROM snapshots WHERE mission_id = ?1 AND kind = ?2 AND ((?3 IS NULL AND node_id IS NULL) OR (?3 IS NOT NULL AND node_id = ?3)) ORDER BY last_seq DESC, created_at_ms DESC LIMIT 1",
            params![mission_id.to_string(), kind, node_id.map(|id| id.to_string())],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            },
        ).optional()?;

        match row {
            None => Ok(None),
            Some((
                snapshot_id,
                schema_version,
                mission_id_s,
                run_id_s,
                node_id_s,
                kind,
                last_seq,
                payload_json,
                created_at_ms,
            )) => {
                let run_id = match run_id_s {
                    Some(value) => Some(RunId::parse(&value)?),
                    None => None,
                };
                let node_id = match node_id_s {
                    Some(value) => Some(NodeId::parse(&value)?),
                    None => None,
                };
                let last_seq =
                    u64::try_from(last_seq).map_err(|_| StoreError::SequenceExhausted)?;
                if last_seq > MAX_SEQ {
                    return Err(StoreError::SequenceExhausted);
                }
                Ok(Some(SnapshotRecord {
                    snapshot_id,
                    schema_version,
                    mission_id: MissionId::parse(&mission_id_s)?,
                    run_id,
                    node_id,
                    kind,
                    last_seq,
                    payload: serde_json::from_str(&payload_json)?,
                    created_at_ms,
                }))
            }
        }
    }

    pub fn create_approval(&mut self, input: &ApprovalInput) -> StoreResult<ApprovalRecord> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = create_approval_on(&tx, input, now_ms())?;
        tx.commit()?;
        Ok(record)
    }

    pub fn transition_node_and_request_approval(
        &mut self,
        node_id: NodeId,
        input: &ApprovalInput,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, ApprovalRecord, EventEnvelope)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let node = update_node_state_on(&tx, node_id, NodeState::WaitingApproval, now_ms)?;
        let approval = create_approval_on(&tx, input, now_ms)?;
        let envelope = insert_event(&tx, event)?;
        tx.commit()?;
        Ok((node, approval, envelope))
    }

    /// Lease-guarded variant of [`Self::transition_node_and_request_approval`].
    ///
    /// A stale executor cannot move a newer attempt into `waiting_approval` or
    /// create an approval bound to an obsolete lease.
    pub fn transition_node_cas_and_request_approval(
        &mut self,
        node_id: NodeId,
        guard: &LeaseGuard,
        input: &ApprovalInput,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, ApprovalRecord, EventEnvelope)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = query_node(&tx, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if current.attempt_no != guard.attempt_no
            || current.lease_token.as_ref() != Some(&guard.lease_token)
        {
            return Err(StoreError::StaleExecution);
        }
        if let Some(event_node) = event.node_id {
            if event_node != node_id {
                return Err(StoreError::Conflict(format!(
                    "event node {event_node} does not match {node_id}"
                )));
            }
        }
        if let Some(event_attempt) = event.attempt_no {
            if event_attempt != guard.attempt_no {
                return Err(StoreError::Conflict(
                    "event attempt_no does not match lease guard".to_owned(),
                ));
            }
        }
        let next_state = current.state.transition(NodeState::WaitingApproval)?;
        tx.execute(
            "UPDATE nodes SET state = ?1, updated_at_ms = ?2 WHERE node_id = ?3",
            params![next_state.as_str(), now_ms, node_id.to_string()],
        )?;
        let approval = create_approval_on(&tx, input, now_ms)?;
        let envelope = insert_event(&tx, event)?;
        tx.commit()?;
        let node = query_node(&self.conn, node_id)?.expect("node was just updated");
        Ok((node, approval, envelope))
    }

    pub fn get_approval(&self, approval_id: ApprovalId) -> StoreResult<Option<ApprovalRecord>> {
        query_approval(&self.conn, approval_id)
    }

    pub fn decide_approval(
        &mut self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        decided_by: &str,
        now_ms: i64,
    ) -> StoreResult<ApprovalRecord> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let expired = tx.execute(
            "UPDATE approvals SET state = 'expired', decided_at_ms = ?1 WHERE approval_id = ?2 AND state = 'pending' AND ?1 >= expires_at_ms",
            params![now_ms, approval_id.to_string()],
        )?;
        if expired == 1 {
            tx.commit()?;
            return Err(StoreError::ApprovalConflict(format!(
                "approval {approval_id} expired before decision"
            )));
        }

        let affected = tx.execute(
            "UPDATE approvals SET state = ?1, decided_at_ms = ?2, decided_by = ?3 WHERE approval_id = ?4 AND state = 'pending'",
            params![decision.as_state().as_str(), now_ms, decided_by, approval_id.to_string()],
        )?;
        if affected != 1 {
            tx.rollback()?;
            return Err(StoreError::ApprovalConflict(format!(
                "approval {approval_id} is not pending"
            )));
        }
        tx.commit()?;

        query_approval(&self.conn, approval_id)?
            .ok_or_else(|| StoreError::NotFound(format!("approval {approval_id}")))
    }

    pub fn list_pending_approvals(&self, now_ms: i64) -> StoreResult<Vec<ApprovalRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {APPROVAL_COLUMNS} FROM approvals WHERE state = 'pending' AND expires_at_ms > ?1 ORDER BY expires_at_ms ASC"
        ))?;
        let rows = stmt.query_map(params![now_ms], raw_approval_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(parse_approval(row?)?);
        }
        Ok(out)
    }

    /// Consume an approved approval exactly once, guarded by the tool call,
    /// request hash, attempt and lease token. All checks and the update happen
    /// in one immediate transaction, so concurrent consumers cannot both win.
    pub fn consume_approval(
        &mut self,
        approval_id: ApprovalId,
        tool_call_id: ToolCallId,
        request_hash: &str,
        attempt_no: AttemptNo,
        lease_token: &LeaseToken,
        now_ms: i64,
    ) -> StoreResult<ApprovalRecord> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = query_approval(&tx, approval_id)?
            .ok_or_else(|| StoreError::NotFound(format!("approval {approval_id}")))?;

        if current.consumed_at_ms.is_some() {
            return Err(StoreError::ApprovalReplayed(format!(
                "approval {approval_id} was already consumed"
            )));
        }
        if current.state != ApprovalState::Approved {
            return Err(StoreError::ApprovalConflict(format!(
                "approval {approval_id} is {}, expected approved",
                current.state
            )));
        }
        if now_ms >= current.expires_at_ms {
            return Err(StoreError::ApprovalConflict(format!(
                "approval {approval_id} expired before consumption"
            )));
        }
        if current.tool_call_id != tool_call_id
            || current.request_hash != request_hash
            || current.attempt_no != attempt_no
            || current.lease_token.as_str() != lease_token.as_str()
        {
            return Err(StoreError::StaleExecution);
        }

        let affected = tx.execute(
            "UPDATE approvals SET consumed_at_ms = ?1 WHERE approval_id = ?2 AND state = 'approved' AND consumed_at_ms IS NULL AND tool_call_id = ?3 AND request_hash = ?4 AND attempt_no = ?5 AND lease_token = ?6",
            params![
                now_ms,
                approval_id.to_string(),
                tool_call_id.to_string(),
                request_hash,
                attempt_no.get(),
                lease_token.as_str(),
            ],
        )?;
        if affected != 1 {
            let latest = query_approval(&tx, approval_id)?;
            if latest.and_then(|record| record.consumed_at_ms).is_some() {
                return Err(StoreError::ApprovalReplayed(format!(
                    "approval {approval_id} was consumed concurrently"
                )));
            }
            return Err(StoreError::ApprovalConflict(format!(
                "approval {approval_id} could not be consumed"
            )));
        }
        tx.commit()?;
        query_approval(&self.conn, approval_id)?
            .ok_or_else(|| StoreError::NotFound(format!("approval {approval_id}")))
    }

    /// Append one link to the append-only tool audit hash chain.
    pub fn append_tool_audit(
        &mut self,
        mission_id: MissionId,
        tool_call_id: ToolCallId,
        event_kind: &str,
        record: serde_json::Value,
        now_ms: i64,
    ) -> StoreResult<ToolAuditRecord> {
        if event_kind.is_empty() {
            return Err(StoreError::Conflict(
                "tool audit event kind must not be empty".to_owned(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prev_hash: Option<String> = tx
            .query_row(
                "SELECT entry_hash FROM tool_audit ORDER BY audit_id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let prev_hash = prev_hash.unwrap_or_else(|| "GENESIS".to_owned());
        let record_json = serde_json::to_string(&record)?;
        let entry_hash = audit_entry_hash(&prev_hash, &record_json);
        tx.execute(
            "INSERT INTO tool_audit (mission_id, tool_call_id, event_kind, record_json, prev_hash, entry_hash, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                mission_id.to_string(),
                tool_call_id.to_string(),
                event_kind,
                record_json,
                prev_hash,
                entry_hash,
                now_ms,
            ],
        )?;
        let audit_id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(ToolAuditRecord {
            audit_id,
            mission_id,
            tool_call_id,
            event_kind: event_kind.to_owned(),
            record,
            prev_hash,
            entry_hash,
            created_at_ms: now_ms,
        })
    }

    pub fn verify_tool_audit_chain(&self) -> StoreResult<bool> {
        let mut stmt = self.conn.prepare(
            "SELECT record_json, prev_hash, entry_hash FROM tool_audit ORDER BY audit_id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut expected_prev = "GENESIS".to_owned();
        for row in rows {
            let (record_json, prev_hash, entry_hash) = row?;
            if prev_hash != expected_prev {
                return Ok(false);
            }
            if audit_entry_hash(&prev_hash, &record_json) != entry_hash {
                return Ok(false);
            }
            expected_prev = entry_hash;
        }
        Ok(true)
    }

    pub fn expire_overdue_approvals(&mut self, now_ms: i64) -> StoreResult<Vec<ApprovalRecord>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidates: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT approval_id FROM approvals WHERE state = 'pending' AND expires_at_ms <= ?1 ORDER BY expires_at_ms ASC",
            )?;
            let rows = stmt.query_map(params![now_ms], |row| row.get::<_, String>(0))?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row?);
            }
            ids
        };
        let mut expired_ids = Vec::new();
        for id in candidates {
            let affected = tx.execute(
                "UPDATE approvals SET state = 'expired', decided_at_ms = ?1 WHERE approval_id = ?2 AND state = 'pending'",
                params![now_ms, &id],
            )?;
            if affected == 1 {
                expired_ids.push(id);
            }
        }
        tx.commit()?;

        let mut out = Vec::with_capacity(expired_ids.len());
        for id in expired_ids {
            let approval_id = ApprovalId::parse(&id)?;
            out.push(query_approval(&self.conn, approval_id)?.expect("approval exists"));
        }
        Ok(out)
    }

    pub fn find_expired_leases(&self, now_ms: i64) -> StoreResult<Vec<NodeRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE state IN ('leased', 'running') AND lease_expires_at_ms IS NOT NULL AND lease_expires_at_ms < ?1 ORDER BY lease_expires_at_ms ASC"
        ))?;
        let rows = stmt.query_map(params![now_ms], raw_node_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(parse_node(row?)?);
        }
        Ok(out)
    }

    /// Reclaim expired leases: `leased -> ready` emits `node.ready` and
    /// `running -> needs_recovery` emits `node.needs_recovery` (reason
    /// `lease_expired`). Each state transition and its event commit in one
    /// `IMMEDIATE` transaction, and the row update is state-guarded so a row
    /// already reclaimed elsewhere never emits a duplicate event.
    pub fn reclaim_expired_leases(&mut self, now_ms: i64) -> StoreResult<Vec<NodeRecord>> {
        let expired = self.find_expired_leases(now_ms)?;
        let mut reclaimed = Vec::with_capacity(expired.len());
        for node in expired {
            let (next_state, kind, reason) = match node.state {
                NodeState::Leased => (NodeState::Ready, EventKind::NodeReady, None),
                NodeState::Running => (
                    NodeState::NeedsRecovery,
                    EventKind::NodeNeedsRecovery,
                    Some("lease_expired"),
                ),
                _ => continue,
            };
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let validated = node.state.transition(next_state)?;
            let affected = tx.execute(
                "UPDATE nodes SET state = ?1, lease_token = NULL, lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?2 WHERE node_id = ?3 AND state = ?4",
                params![validated.as_str(), now_ms, node.node_id.to_string(), node.state.as_str()],
            )?;
            if affected != 1 {
                tx.commit()?;
                continue;
            }
            let payload = match reason {
                Some(reason) => serde_json::json!({
                    "reason": reason,
                    "last_state": node.state.as_str(),
                }),
                None => serde_json::json!({}),
            };
            let event = EventInput {
                event_id: EventId::new(),
                kind,
                mission_id: node.mission_id,
                run_id: Some(node.run_id),
                node_id: Some(node.node_id),
                session_id: None,
                attempt_no: Some(node.attempt_no),
                trace_id: None,
                payload,
            };
            insert_event(&tx, &event)?;
            tx.commit()?;
            reclaimed.push(query_node(&self.conn, node.node_id)?.expect("node exists"));
        }
        Ok(reclaimed)
    }

    pub fn retry_node(&mut self, node_id: NodeId, now_ms: i64) -> StoreResult<NodeRecord> {
        let current = query_node(&self.conn, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if current.state != NodeState::NeedsRecovery {
            return Err(StoreError::Conflict(format!(
                "node {node_id} is {}, expected needs_recovery",
                current.state
            )));
        }
        let next_state = NodeState::NeedsRecovery.transition(NodeState::Ready)?;
        let next_attempt = current
            .attempt_no
            .checked_next()
            .ok_or_else(|| StoreError::Conflict("attempt number exhausted".to_owned()))?;
        self.conn.execute(
            "UPDATE nodes SET state = ?1, attempt_no = ?2, lease_token = NULL, lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?3 WHERE node_id = ?4",
            params![next_state.as_str(), next_attempt.get(), now_ms, node_id.to_string()],
        )?;
        query_node(&self.conn, node_id).map(|record| record.expect("node was just updated"))
    }

    pub fn abort_node(&mut self, node_id: NodeId, now_ms: i64) -> StoreResult<NodeRecord> {
        let current = query_node(&self.conn, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if current.state != NodeState::NeedsRecovery {
            return Err(StoreError::Conflict(format!(
                "node {node_id} is {}, expected needs_recovery",
                current.state
            )));
        }
        let next_state = NodeState::NeedsRecovery.transition(NodeState::FailedFinal)?;
        self.conn.execute(
            "UPDATE nodes SET state = ?1, lease_token = NULL, lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?2 WHERE node_id = ?3",
            params![next_state.as_str(), now_ms, node_id.to_string()],
        )?;
        query_node(&self.conn, node_id).map(|record| record.expect("node was just updated"))
    }

    /// Retry a node after a failed attempt: `failed -> retry_ready -> ready`
    /// and clear the lease in one transaction, keeping `attempt_no` unchanged.
    ///
    /// `attempt_no` advances only when the node is next claimed
    /// ([`Self::claim_node`]), not when it re-enters `ready`.
    pub fn retry_failed_node(&mut self, node_id: NodeId, now_ms: i64) -> StoreResult<NodeRecord> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = query_node(&tx, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if current.state != NodeState::Failed {
            return Err(StoreError::Conflict(format!(
                "node {node_id} is {}, expected failed",
                current.state
            )));
        }
        let _ = NodeState::Failed.transition(NodeState::RetryReady)?;
        let ready = NodeState::RetryReady.transition(NodeState::Ready)?;
        tx.execute(
            "UPDATE nodes SET state = ?1, lease_token = NULL, lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?2 WHERE node_id = ?3",
            params![ready.as_str(), now_ms, node_id.to_string()],
        )?;
        tx.commit()?;
        query_node(&self.conn, node_id).map(|record| record.expect("node was just updated"))
    }

    /// Retry a node after a failed attempt and emit its `node.ready` event in
    /// the same transaction: `failed -> retry_ready -> ready` + event insert.
    ///
    /// This is the guarded entry for the retry path so the state change and
    /// the event can never be observed separately (PROTOCOL §2).
    pub fn retry_failed_node_and_emit(
        &mut self,
        node_id: NodeId,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = query_node(&tx, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if current.state != NodeState::Failed {
            return Err(StoreError::Conflict(format!(
                "node {node_id} is {}, expected failed",
                current.state
            )));
        }
        let _ = NodeState::Failed.transition(NodeState::RetryReady)?;
        let ready = NodeState::RetryReady.transition(NodeState::Ready)?;
        tx.execute(
            "UPDATE nodes SET state = ?1, lease_token = NULL, lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?2 WHERE node_id = ?3",
            params![ready.as_str(), now_ms, node_id.to_string()],
        )?;
        let envelope = insert_event(&tx, event)?;
        tx.commit()?;
        let node = query_node(&self.conn, node_id)?.expect("node was just updated");
        Ok((node, envelope))
    }

    /// Finalize a node after a failed attempt whose retry budget is exhausted:
    /// `failed -> failed_final`, clearing the lease in one transaction.
    /// Emits no event; `node.failed` was already emitted for this attempt.
    pub fn abort_failed_node(&mut self, node_id: NodeId, now_ms: i64) -> StoreResult<NodeRecord> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = query_node(&tx, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if current.state != NodeState::Failed {
            return Err(StoreError::Conflict(format!(
                "node {node_id} is {}, expected failed",
                current.state
            )));
        }
        let next_state = NodeState::Failed.transition(NodeState::FailedFinal)?;
        tx.execute(
            "UPDATE nodes SET state = ?1, lease_token = NULL, lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?2 WHERE node_id = ?3",
            params![next_state.as_str(), now_ms, node_id.to_string()],
        )?;
        tx.commit()?;
        query_node(&self.conn, node_id).map(|record| record.expect("node was just updated"))
    }

    pub fn create_agent_session(
        &mut self,
        input: &NewAgentSession<'_>,
    ) -> StoreResult<AgentSessionRecord> {
        let record = AgentSessionRecord {
            session_id: AgentSessionId::new(),
            mission_id: input.mission_id,
            run_id: input.run_id,
            node_id: input.node_id,
            role: input.role.to_owned(),
            model: input.model.to_owned(),
            system_prompt: input.system_prompt.to_owned(),
            state: AgentSessionState::Active,
            created_at_ms: input.now_ms,
            updated_at_ms: input.now_ms,
            closed_at_ms: None,
        };
        self.conn.execute(
            "INSERT INTO agent_sessions (session_id, mission_id, run_id, node_id, role, model, system_prompt, state, created_at_ms, updated_at_ms, closed_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)",
            params![
                record.session_id.to_string(),
                input.mission_id.to_string(),
                input.run_id.map(|id| id.to_string()),
                input.node_id.to_string(),
                record.role,
                record.model,
                record.system_prompt,
                record.state.as_str(),
                input.now_ms,
                input.now_ms,
            ],
        )?;
        Ok(record)
    }

    pub fn get_agent_session(
        &self,
        session_id: AgentSessionId,
    ) -> StoreResult<Option<AgentSessionRecord>> {
        query_agent_session(&self.conn, session_id)
    }

    pub fn set_agent_session_state(
        &mut self,
        session_id: AgentSessionId,
        state: AgentSessionState,
        now_ms: i64,
    ) -> StoreResult<AgentSessionRecord> {
        let closed_at = state.is_terminal().then_some(now_ms);
        let affected = self.conn.execute(
            "UPDATE agent_sessions SET state = ?1, updated_at_ms = ?2, closed_at_ms = ?3 WHERE session_id = ?4",
            params![state.as_str(), now_ms, closed_at, session_id.to_string()],
        )?;
        if affected != 1 {
            return Err(StoreError::NotFound(format!("session {session_id}")));
        }
        query_agent_session(&self.conn, session_id)?
            .ok_or_else(|| StoreError::NotFound(format!("session {session_id}")))
    }

    pub fn start_turn(
        &mut self,
        session_id: AgentSessionId,
        node_id: NodeId,
        attempt_no: AttemptNo,
        turn_index: u32,
        request_model: &str,
        now_ms: i64,
    ) -> StoreResult<TurnRecord> {
        if turn_index == 0 {
            return Err(StoreError::Conflict("turn index starts at 1".to_owned()));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = query_agent_session(&tx, session_id)?
            .ok_or_else(|| StoreError::NotFound(format!("session {session_id}")))?;
        if session.node_id != node_id {
            return Err(StoreError::Conflict(format!(
                "session {session_id} belongs to node {}",
                session.node_id
            )));
        }
        deduct_budget_on(
            &tx,
            "node",
            &node_id.to_string(),
            BudgetDimension::Requests,
            1,
            now_ms,
        )?;
        let record = TurnRecord {
            turn_id: TurnId::new(),
            session_id,
            node_id,
            attempt_no,
            turn_index,
            request_model: request_model.to_owned(),
            finish_reason: None,
            state: TurnState::Started,
            output_preview: String::new(),
            tool_call_count: 0,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        tx.execute(
            "INSERT INTO turns (turn_id, session_id, node_id, attempt_no, turn_index, request_model, finish_reason, state, output_preview, tool_call_count, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, '', 0, ?8, ?8)",
            params![
                record.turn_id.to_string(),
                session_id.to_string(),
                node_id.to_string(),
                attempt_no.get(),
                turn_index,
                request_model,
                record.state.as_str(),
                now_ms,
            ],
        )?;
        tx.commit()?;
        Ok(record)
    }

    pub fn finish_turn(
        &mut self,
        turn_id: TurnId,
        state: TurnState,
        finish_reason: Option<&str>,
        output_preview: &str,
        tool_call_count: u32,
        now_ms: i64,
    ) -> StoreResult<TurnRecord> {
        if state == TurnState::Started {
            return Err(StoreError::Conflict(
                "turn must finish with a terminal state".to_owned(),
            ));
        }
        let affected = self.conn.execute(
            "UPDATE turns SET state = ?1, finish_reason = ?2, output_preview = ?3, tool_call_count = ?4, updated_at_ms = ?5 WHERE turn_id = ?6",
            params![
                state.as_str(),
                finish_reason,
                output_preview,
                tool_call_count,
                now_ms,
                turn_id.to_string(),
            ],
        )?;
        if affected != 1 {
            return Err(StoreError::NotFound(format!("turn {turn_id}")));
        }
        query_turn(&self.conn, turn_id)?
            .ok_or_else(|| StoreError::NotFound(format!("turn {turn_id}")))
    }

    pub fn record_turn_usage(
        &mut self,
        turn_id: TurnId,
        usage: &NewTurnUsage,
        now_ms: i64,
    ) -> StoreResult<TurnUsageRecord> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let turn = query_turn(&tx, turn_id)?
            .ok_or_else(|| StoreError::NotFound(format!("turn {turn_id}")))?;
        let prompt_tokens = to_sql_integer(usage.prompt_tokens)?;
        let completion_tokens = to_sql_integer(usage.completion_tokens)?;
        let total_tokens = to_sql_integer(usage.total_tokens)?;
        tx.execute(
            "INSERT INTO turn_usage (turn_id, session_id, prompt_tokens, completion_tokens, total_tokens, source, request_id, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                turn_id.to_string(),
                turn.session_id.to_string(),
                prompt_tokens,
                completion_tokens,
                total_tokens,
                usage.source.as_str(),
                usage.request_id,
                now_ms,
            ],
        )?;
        // Gateway-internal retries reuse the same `request_id`, so the budget
        // counters must be idempotent: a repeated request_id still records the
        // usage fact but does not re-consume tokens/cost.
        let already_counted = match usage.request_id.as_deref() {
            Some(request_id) if !request_id.is_empty() => tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM turn_usage WHERE request_id = ?1 AND turn_id != ?2)",
                params![request_id, turn_id.to_string()],
                |row| row.get::<_, bool>(0),
            )?,
            _ => false,
        };
        if !already_counted {
            deduct_budget_on(
                &tx,
                "node",
                &turn.node_id.to_string(),
                BudgetDimension::Tokens,
                total_tokens,
                now_ms,
            )?;
            // Minimal placeholder: no per-model unit price is configured yet,
            // so cost is settled as 1 currency unit per token when a cost
            // budget row exists. Documented in the H09a handoff.
            deduct_budget_on(
                &tx,
                "node",
                &turn.node_id.to_string(),
                BudgetDimension::Cost,
                total_tokens,
                now_ms,
            )?;
        }
        tx.commit()?;
        Ok(TurnUsageRecord {
            turn_id,
            session_id: turn.session_id,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            source: usage.source,
            request_id: usage.request_id.clone(),
            created_at_ms: now_ms,
        })
    }

    pub fn get_turn(&self, turn_id: TurnId) -> StoreResult<Option<TurnRecord>> {
        query_turn(&self.conn, turn_id)
    }

    pub fn get_turn_usage(&self, turn_id: TurnId) -> StoreResult<Option<TurnUsageRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT turn_id, session_id, prompt_tokens, completion_tokens, total_tokens, source, request_id, created_at_ms FROM turn_usage WHERE turn_id = ?1",
        )?;
        let row = stmt
            .query_row(params![turn_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })
            .optional()?;
        match row {
            None => Ok(None),
            Some((
                turn_id,
                session_id,
                prompt_tokens,
                completion_tokens,
                total_tokens,
                source,
                request_id,
                created_at_ms,
            )) => Ok(Some(TurnUsageRecord {
                turn_id: TurnId::parse(&turn_id)?,
                session_id: AgentSessionId::parse(&session_id)?,
                prompt_tokens: from_sql_integer(prompt_tokens)?,
                completion_tokens: from_sql_integer(completion_tokens)?,
                total_tokens: from_sql_integer(total_tokens)?,
                source: UsageSource::parse(&source).ok_or_else(|| {
                    StoreError::Conflict(format!("unknown usage source {source}"))
                })?,
                request_id,
                created_at_ms,
            })),
        }
    }

    pub fn list_turns(&self, session_id: AgentSessionId) -> StoreResult<Vec<TurnRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TURN_COLUMNS} FROM turns WHERE session_id = ?1 ORDER BY turn_index ASC"
        ))?;
        let rows = stmt.query_map(params![session_id.to_string()], raw_turn_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(parse_turn(row?)?);
        }
        Ok(out)
    }

    /// Create or update a budget allocation row without touching `consumed`.
    pub fn set_budget(
        &mut self,
        scope_kind: &str,
        scope_id: &str,
        dimension: BudgetDimension,
        currency: Option<&str>,
        limits: BudgetLimits,
        now_ms: i64,
    ) -> StoreResult<BudgetRecord> {
        if limits.hard_limit.is_some_and(|limit| limit < 0)
            || limits.soft_limit.is_some_and(|limit| limit < 0)
        {
            return Err(StoreError::Conflict(
                "budget limits must be non-negative".to_owned(),
            ));
        }
        self.conn.execute(
            "INSERT INTO budgets (scope_kind, scope_id, dimension, currency, hard_limit, soft_limit, consumed, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)
             ON CONFLICT(scope_kind, scope_id, dimension) DO UPDATE SET currency = excluded.currency, hard_limit = excluded.hard_limit, soft_limit = excluded.soft_limit, updated_at_ms = excluded.updated_at_ms",
            params![
                scope_kind,
                scope_id,
                dimension.as_str(),
                currency,
                limits.hard_limit,
                limits.soft_limit,
                now_ms
            ],
        )?;
        self.get_budget(scope_kind, scope_id, dimension)?
            .ok_or_else(|| StoreError::Conflict("budget insert failed".to_owned()))
    }

    pub fn get_budget(
        &self,
        scope_kind: &str,
        scope_id: &str,
        dimension: BudgetDimension,
    ) -> StoreResult<Option<BudgetRecord>> {
        let row = self
            .conn
            .query_row(
                "SELECT scope_kind, scope_id, dimension, currency, hard_limit, soft_limit, consumed, updated_at_ms FROM budgets WHERE scope_kind = ?1 AND scope_id = ?2 AND dimension = ?3",
                params![scope_kind, scope_id, dimension.as_str()],
                |row| {
                    Ok(BudgetRecord {
                        scope_kind: row.get(0)?,
                        scope_id: row.get(1)?,
                        dimension: row.get(2)?,
                        currency: row.get(3)?,
                        hard_limit: row.get(4)?,
                        soft_limit: row.get(5)?,
                        consumed: row.get(6)?,
                        updated_at_ms: row.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Remaining hard budget for a node dimension. `None` means unlimited
    /// (either no row or no hard limit); `Some(0)` means exhausted.
    pub fn budget_remaining(
        &self,
        node_id: NodeId,
        dimension: BudgetDimension,
    ) -> StoreResult<Option<i64>> {
        let row: Option<(Option<i64>, i64)> = self
            .conn
            .query_row(
                "SELECT hard_limit, consumed FROM budgets WHERE scope_kind = 'node' AND scope_id = ?1 AND dimension = ?2",
                params![node_id.to_string(), dimension.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(limit, consumed)| limit.map(|limit| limit - consumed)))
    }

    /// Append a tool lifecycle event and, in the same transaction, maintain
    /// the `tool_calls` fact row and the `tool_calls` budget counter.
    ///
    /// The first `node.tool_call` (intent) deducts one `tool_calls` unit and
    /// inserts the fact row; `node.tool_started` and `node.tool_result` only
    /// advance the fact row state. A budget overrun rolls the whole
    /// transaction back and returns [`StoreError::BudgetExceeded`].
    pub fn append_tool_event(
        &mut self,
        node_id: NodeId,
        guard: &LeaseGuard,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<EventEnvelope> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = query_node(&tx, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if current.attempt_no != guard.attempt_no
            || current.lease_token.as_ref() != Some(&guard.lease_token)
        {
            return Err(StoreError::StaleExecution);
        }
        if current.state.is_terminal() {
            return Err(StoreError::StaleExecution);
        }
        if let Some(event_node) = event.node_id {
            if event_node != node_id {
                return Err(StoreError::Conflict(format!(
                    "event node {event_node} does not match {node_id}"
                )));
            }
        }
        if let Some(event_attempt) = event.attempt_no {
            if event_attempt != guard.attempt_no {
                return Err(StoreError::Conflict(
                    "event attempt_no does not match lease guard".to_owned(),
                ));
            }
        }

        match event.kind {
            EventKind::NodeToolCall => {
                let (tool_call_id, tool_name, tool_version, request_hash) =
                    tool_event_fields(event)?;
                deduct_budget_on(
                    &tx,
                    "node",
                    &node_id.to_string(),
                    BudgetDimension::ToolCalls,
                    1,
                    now_ms,
                )?;
                tx.execute(
                    "INSERT INTO tool_calls (tool_call_id, mission_id, run_id, node_id, session_id, tool_name, tool_version, request_hash, attempt_no, lease_token, state, summary, truncated, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'intent', '', 0, ?11, ?11)",
                    params![
                        tool_call_id,
                        event.mission_id.to_string(),
                        event.run_id.map(|id| id.to_string()),
                        node_id.to_string(),
                        event.session_id.map(|id| id.to_string()),
                        tool_name,
                        tool_version,
                        request_hash,
                        guard.attempt_no.get(),
                        guard.lease_token.as_str(),
                        now_ms,
                    ],
                )?;
            }
            EventKind::NodeToolStarted => {
                let tool_call_id = tool_call_id_from(event)?;
                tx.execute(
                    "UPDATE tool_calls SET state = 'started', updated_at_ms = ?1 WHERE tool_call_id = ?2 AND node_id = ?3",
                    params![now_ms, tool_call_id, node_id.to_string()],
                )?;
            }
            EventKind::NodeToolResult => {
                let tool_call_id = tool_call_id_from(event)?;
                let status = event
                    .payload
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("ok");
                let state = if status == "ok" {
                    "completed"
                } else {
                    "failed"
                };
                let summary = event
                    .payload
                    .get("summary")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let truncated = event
                    .payload
                    .get("truncated")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                tx.execute(
                    "UPDATE tool_calls SET state = ?1, summary = ?2, truncated = ?3, updated_at_ms = ?4 WHERE tool_call_id = ?5 AND node_id = ?6",
                    params![state, summary, truncated as i64, now_ms, tool_call_id, node_id.to_string()],
                )?;
            }
            _ => {}
        }

        let envelope = insert_event(&tx, event)?;
        tx.commit()?;
        Ok(envelope)
    }

    /// Side-effect states of one attempt, read from the `tool_calls` facts.
    pub fn side_effect_states(
        &self,
        node_id: NodeId,
        attempt_no: AttemptNo,
    ) -> StoreResult<Vec<SideEffectState>> {
        let mut stmt = self
            .conn
            .prepare("SELECT state FROM tool_calls WHERE node_id = ?1 AND attempt_no = ?2")?;
        let rows = stmt.query_map(params![node_id.to_string(), attempt_no.get()], |row| {
            row.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for row in rows {
            match row?.as_str() {
                "intent" => out.push(SideEffectState::Intent),
                "started" => out.push(SideEffectState::Started),
                "completed" => out.push(SideEffectState::Completed),
                _ => {}
            }
        }
        Ok(out)
    }

    fn find_active_leases(&self) -> StoreResult<Vec<NodeRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE state IN ('leased', 'running') ORDER BY node_id"
        ))?;
        let rows = stmt.query_map([], raw_node_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(parse_node(row?)?);
        }
        Ok(out)
    }

    /// Unconditionally revoke every surviving lease on startup: `leased ->
    /// ready` (attempt unchanged, emits `node.ready`) and `running ->
    /// needs_recovery` (emits `node.needs_recovery`). Idempotent: re-running
    /// finds no active leases.
    pub fn revoke_all_active_leases(&mut self, now_ms: i64) -> StoreResult<Vec<NodeRecord>> {
        let active = self.find_active_leases()?;
        let mut out = Vec::with_capacity(active.len());
        for node in active {
            let (next_state, kind, reason) = match node.state {
                NodeState::Leased => (NodeState::Ready, EventKind::NodeReady, None),
                NodeState::Running => (
                    NodeState::NeedsRecovery,
                    EventKind::NodeNeedsRecovery,
                    Some("lease_revoked_on_startup"),
                ),
                _ => continue,
            };
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let validated = node.state.transition(next_state)?;
            tx.execute(
                "UPDATE nodes SET state = ?1, lease_token = NULL, lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?2 WHERE node_id = ?3",
                params![validated.as_str(), now_ms, node.node_id.to_string()],
            )?;
            let payload = match reason {
                Some(reason) => serde_json::json!({
                    "reason": reason,
                    "last_state": node.state.as_str(),
                }),
                None => serde_json::json!({}),
            };
            let event = EventInput {
                event_id: EventId::new(),
                kind,
                mission_id: node.mission_id,
                run_id: Some(node.run_id),
                node_id: Some(node.node_id),
                session_id: None,
                attempt_no: Some(node.attempt_no),
                trace_id: None,
                payload,
            };
            insert_event(&tx, &event)?;
            tx.commit()?;
            out.push(query_node(&self.conn, node.node_id)?.expect("node exists"));
        }
        Ok(out)
    }

    pub fn find_waiting_approval_nodes(&self) -> StoreResult<Vec<NodeRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE state = 'waiting_approval' ORDER BY node_id"
        ))?;
        let rows = stmt.query_map([], raw_node_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(parse_node(row?)?);
        }
        Ok(out)
    }

    /// Fail-closed orphan approval handling: pending or approved-but-unconsumed
    /// approvals for a node are expired (`decided_by=recovery`,
    /// `decision_note=owner_lost`) and each emits `node.approval_decided`.
    pub fn expire_node_approvals(
        &mut self,
        node_id: NodeId,
        now_ms: i64,
    ) -> StoreResult<Vec<ApprovalRecord>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidates: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT approval_id FROM approvals WHERE node_id = ?1 AND (state = 'pending' OR (state = 'approved' AND consumed_at_ms IS NULL)) ORDER BY created_at_ms ASC",
            )?;
            let rows =
                stmt.query_map(params![node_id.to_string()], |row| row.get::<_, String>(0))?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row?);
            }
            ids
        };
        let mut out = Vec::new();
        for id in candidates {
            let affected = tx.execute(
                "UPDATE approvals SET state = 'expired', decided_at_ms = ?1, decided_by = 'recovery', decision_note = 'owner_lost' WHERE approval_id = ?2 AND (state = 'pending' OR (state = 'approved' AND consumed_at_ms IS NULL))",
                params![now_ms, &id],
            )?;
            if affected == 1 {
                let approval =
                    query_approval(&tx, ApprovalId::parse(&id)?)?.expect("approval exists");
                let event = EventInput {
                    event_id: EventId::new(),
                    kind: EventKind::NodeApprovalDecided,
                    mission_id: approval.mission_id,
                    run_id: approval.run_id,
                    node_id: Some(approval.node_id),
                    session_id: approval.session_id,
                    attempt_no: Some(approval.attempt_no),
                    trace_id: None,
                    payload: serde_json::json!({
                        "decision": "expired",
                        "reason": "owner_lost",
                        "approval_id": approval.approval_id.to_string(),
                        "tool_name": approval.tool_name,
                    }),
                };
                insert_event(&tx, &event)?;
                out.push(approval);
            }
        }
        tx.commit()?;
        Ok(out)
    }

    /// `waiting_approval -> failed` with reason `approval_orphaned`, emitting
    /// `node.failed` in the same transaction.
    pub fn fail_waiting_approval_orphaned(
        &mut self,
        node_id: NodeId,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let node = query_node(&tx, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if node.state != NodeState::WaitingApproval {
            return Err(StoreError::Conflict(format!(
                "node {node_id} is {}, expected waiting_approval",
                node.state
            )));
        }
        let next = node.state.transition(NodeState::Failed)?;
        tx.execute(
            "UPDATE nodes SET state = ?1, lease_token = NULL, lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?2 WHERE node_id = ?3",
            params![next.as_str(), now_ms, node_id.to_string()],
        )?;
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::NodeFailed,
            mission_id: node.mission_id,
            run_id: Some(node.run_id),
            node_id: Some(node.node_id),
            session_id: None,
            attempt_no: Some(node.attempt_no),
            trace_id: None,
            payload: serde_json::json!({ "reason": "approval_orphaned" }),
        };
        let envelope = insert_event(&tx, &event)?;
        tx.commit()?;
        let node = query_node(&self.conn, node_id)?.expect("node was just updated");
        Ok((node, envelope))
    }

    /// `waiting_approval -> needs_recovery`, emitting `node.needs_recovery`.
    pub fn transition_waiting_approval_to_needs_recovery(
        &mut self,
        node_id: NodeId,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let node = query_node(&tx, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if node.state != NodeState::WaitingApproval {
            return Err(StoreError::Conflict(format!(
                "node {node_id} is {}, expected waiting_approval",
                node.state
            )));
        }
        let next = node.state.transition(NodeState::NeedsRecovery)?;
        tx.execute(
            "UPDATE nodes SET state = ?1, lease_token = NULL, lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?2 WHERE node_id = ?3",
            params![next.as_str(), now_ms, node_id.to_string()],
        )?;
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::NodeNeedsRecovery,
            mission_id: node.mission_id,
            run_id: Some(node.run_id),
            node_id: Some(node.node_id),
            session_id: None,
            attempt_no: Some(node.attempt_no),
            trace_id: None,
            payload: serde_json::json!({
                "reason": "approval_orphaned",
                "last_state": "waiting_approval",
            }),
        };
        let envelope = insert_event(&tx, &event)?;
        tx.commit()?;
        let node = query_node(&self.conn, node_id)?.expect("node was just updated");
        Ok((node, envelope))
    }

    /// `needs_recovery -> ready` (attempt + 1) with a `node.ready` event, for
    /// the automatic safe-retry arm of startup recovery.
    pub fn retry_needs_recovery_and_emit(
        &mut self,
        node_id: NodeId,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = query_node(&tx, node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
        if current.state != NodeState::NeedsRecovery {
            return Err(StoreError::Conflict(format!(
                "node {node_id} is {}, expected needs_recovery",
                current.state
            )));
        }
        let next_state = NodeState::NeedsRecovery.transition(NodeState::Ready)?;
        let next_attempt = current
            .attempt_no
            .checked_next()
            .ok_or_else(|| StoreError::Conflict("attempt number exhausted".to_owned()))?;
        tx.execute(
            "UPDATE nodes SET state = ?1, attempt_no = ?2, lease_token = NULL, lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?3 WHERE node_id = ?4",
            params![next_state.as_str(), next_attempt.get(), now_ms, node_id.to_string()],
        )?;
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::NodeReady,
            mission_id: current.mission_id,
            run_id: Some(current.run_id),
            node_id: Some(current.node_id),
            session_id: None,
            attempt_no: Some(next_attempt),
            trace_id: None,
            payload: serde_json::json!({}),
        };
        let envelope = insert_event(&tx, &event)?;
        tx.commit()?;
        let node = query_node(&self.conn, node_id)?.expect("node was just updated");
        Ok((node, envelope))
    }

    /// Defensive `retry_ready -> ready` normalization. `retry_ready` is a
    /// transient mid-transaction state and should never persist; if it does,
    /// re-emit `node.ready` and return the node to the runnable pool.
    pub fn normalize_retry_ready(&mut self, now_ms: i64) -> StoreResult<Vec<NodeRecord>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidates: Vec<String> = {
            let mut stmt = tx.prepare("SELECT node_id FROM nodes WHERE state = 'retry_ready'")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row?);
            }
            ids
        };
        let mut out = Vec::new();
        for id in candidates {
            let node_id = NodeId::parse(&id)?;
            let node = query_node(&tx, node_id)?.expect("node exists");
            let next = node.state.transition(NodeState::Ready)?;
            tx.execute(
                "UPDATE nodes SET state = ?1, lease_token = NULL, lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?2 WHERE node_id = ?3",
                params![next.as_str(), now_ms, id],
            )?;
            let event = EventInput {
                event_id: EventId::new(),
                kind: EventKind::NodeReady,
                mission_id: node.mission_id,
                run_id: Some(node.run_id),
                node_id: Some(node.node_id),
                session_id: None,
                attempt_no: Some(node.attempt_no),
                trace_id: None,
                payload: serde_json::json!({}),
            };
            insert_event(&tx, &event)?;
            out.push(query_node(&tx, node_id)?.expect("node exists"));
        }
        tx.commit()?;
        Ok(out)
    }

    /// Rebuild every node-level `consumed` counter from the fact tables.
    /// Idempotent and never touches node states, so a historical
    /// `budget_exceeded` node keeps its terminal state.
    pub fn rebuild_budget_counters(&mut self, now_ms: i64) -> StoreResult<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT scope_id, dimension FROM budgets WHERE scope_kind = 'node' ORDER BY scope_id, dimension",
            )?;
            let mapped = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut out = Vec::new();
            for row in mapped {
                out.push(row?);
            }
            out
        };
        for (scope_id, dimension) in rows {
            let consumed: i64 = match dimension.as_str() {
                "tokens" | "cost" => tx.query_row(
                    "SELECT COALESCE(SUM(tu.total_tokens), 0) FROM turn_usage tu JOIN turns t ON tu.turn_id = t.turn_id WHERE t.node_id = ?1",
                    params![&scope_id],
                    |row| row.get(0),
                )?,
                "requests" => tx.query_row(
                    "SELECT COUNT(*) FROM turns WHERE node_id = ?1",
                    params![&scope_id],
                    |row| row.get(0),
                )?,
                "tool_calls" => tx.query_row(
                    "SELECT COUNT(*) FROM tool_calls WHERE node_id = ?1",
                    params![&scope_id],
                    |row| row.get(0),
                )?,
                _ => continue,
            };
            tx.execute(
                "UPDATE budgets SET consumed = ?1, updated_at_ms = ?2 WHERE scope_kind = 'node' AND scope_id = ?3 AND dimension = ?4",
                params![consumed, now_ms, &scope_id, &dimension],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read-only budget summaries for one node (no keys, base URLs or prices).
    pub fn node_budget_summaries(&self, node_id: NodeId) -> StoreResult<Vec<BudgetSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT dimension, consumed, hard_limit FROM budgets WHERE scope_kind = 'node' AND scope_id = ?1 ORDER BY dimension",
        )?;
        let rows = stmt.query_map(params![node_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (dimension, consumed, hard_limit) = row?;
            out.push(BudgetSummary {
                node_id,
                dimension,
                consumed,
                hard_limit,
                remaining: hard_limit.map(|limit| limit - consumed),
            });
        }
        Ok(out)
    }

    /// Validate and persist a Handoff, emitting `handoff.created` in the same
    /// transaction as the row insert so state and event never diverge.
    pub fn insert_handoff(&mut self, handoff: &Handoff) -> StoreResult<HandoffRecord> {
        handoff.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = insert_handoff_on(&tx, handoff, now_ms())?;
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::HandoffCreated,
            mission_id: handoff.mission_id,
            run_id: Some(handoff.run_id),
            node_id: Some(handoff.node_id),
            session_id: None,
            attempt_no: Some(handoff.provenance.attempt_no),
            trace_id: None,
            payload: serde_json::json!({
                "handoff_id": record.handoff_id.to_string(),
                "summary": handoff.summary,
                "schema_version": handoff.schema_version,
            }),
        };
        insert_event(&tx, &event)?;
        tx.commit()?;
        Ok(record)
    }

    pub fn get_handoff(
        &self,
        node_id: NodeId,
        attempt_no: AttemptNo,
    ) -> StoreResult<Option<HandoffRecord>> {
        query_handoff(&self.conn, node_id, attempt_no)
    }

    pub fn list_handoffs(&self, mission_id: MissionId) -> StoreResult<Vec<HandoffRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {HANDOFF_COLUMNS} FROM handoffs WHERE mission_id = ?1 ORDER BY created_at_ms ASC"
        ))?;
        let rows = stmt.query_map(params![mission_id.to_string()], raw_handoff_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(parse_handoff(row?)?);
        }
        Ok(out)
    }

    pub fn list_missions(&self, limit: u64, offset: u64) -> StoreResult<Vec<MissionListRecord>> {
        let limit =
            i64::try_from(limit).map_err(|_| StoreError::Conflict("limit too large".to_owned()))?;
        let offset = i64::try_from(offset)
            .map_err(|_| StoreError::Conflict("offset too large".to_owned()))?;
        let mut stmt = self.conn.prepare(
            "SELECT m.mission_id, m.state, m.title, m.goal, m.created_at_ms, m.updated_at_ms, \
             (SELECT MAX(seq) FROM events e WHERE e.mission_id = m.mission_id), \
             (SELECT COUNT(*) FROM approvals a WHERE a.mission_id = m.mission_id AND a.state = 'pending') \
             FROM missions m \
             ORDER BY m.updated_at_ms DESC, m.created_at_ms DESC, m.mission_id \
             LIMIT ?1 OFFSET ?2",
        )?;
        let rows = stmt.query_map(params![limit, offset], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, state, title, goal, created_at_ms, updated_at_ms, last_seq, pending) = row?;
            out.push(MissionListRecord {
                mission: MissionRecord {
                    mission_id: MissionId::parse(&id)?,
                    state: MissionState::parse(&state)?,
                    title,
                    goal,
                    created_at_ms,
                    updated_at_ms,
                },
                last_seq: last_seq.map(|seq| seq as u64),
                pending_approvals: pending as u64,
            });
        }
        Ok(out)
    }

    pub fn state_snapshot(&self) -> StoreResult<StateSnapshotRecord> {
        let tx = self.conn.unchecked_transaction()?;
        let missions = list_missions_on(&tx, 60, 0)?;
        let approvals = list_pending_approvals_on(&tx, now_ms())?;
        let last_seq = latest_seq_on(&tx)?;
        let (mission_count, run_count, node_count, event_count) = counts_on(&tx)?;
        let pending_approval_count = approvals.len() as u64;
        tx.commit()?;
        Ok(StateSnapshotRecord {
            missions,
            approvals,
            last_seq,
            mission_count,
            run_count,
            node_count,
            event_count,
            pending_approval_count,
        })
    }

    pub fn mission_snapshot(
        &self,
        mission_id: MissionId,
        after_seq: u64,
        event_limit: u64,
        runs_limit: u64,
        nodes_limit: u64,
    ) -> StoreResult<Option<MissionSnapshotRecord>> {
        let after_seq = http_cursor(after_seq)?;
        let event_limit = i64::try_from(event_limit)
            .map_err(|_| StoreError::Conflict("event_limit too large".to_owned()))?;
        let runs_limit = i64::try_from(runs_limit)
            .map_err(|_| StoreError::Conflict("runs_limit too large".to_owned()))?;
        let nodes_limit = i64::try_from(nodes_limit)
            .map_err(|_| StoreError::Conflict("nodes_limit too large".to_owned()))?;
        let tx = self.conn.unchecked_transaction()?;
        let Some(mission) = query_mission(&tx, mission_id)? else {
            return Ok(None);
        };
        let runs = query_runs_limited(&tx, mission_id, runs_limit)?;
        let nodes = query_nodes_limited(&tx, mission_id, nodes_limit)?;
        let approvals = query_approvals_by_mission(&tx, mission_id)?;
        let artifacts = query_artifacts(&tx, mission_id)?;
        let messages = query_messages(&tx, mission_id)?;
        let events = query_events_page(&tx, Some(mission_id), after_seq, event_limit)?;
        let last_seq = mission_latest_seq_on(&tx, mission_id)?;
        tx.commit()?;
        Ok(Some(MissionSnapshotRecord {
            mission,
            runs,
            nodes,
            approvals,
            artifacts,
            messages,
            events,
            last_seq,
        }))
    }

    pub fn read_events_for_sse(
        &self,
        mission_id: Option<MissionId>,
        after_seq: u64,
    ) -> StoreResult<(Vec<EventEnvelope>, Option<u64>)> {
        let after_seq = http_cursor(after_seq)?;
        let tx = self.conn.unchecked_transaction()?;
        let events = query_events_page(&tx, mission_id, after_seq, i64::MAX)?;
        let last_seq = latest_seq_on(&tx)?;
        tx.commit()?;
        Ok((events, last_seq))
    }

    pub fn get_artifact(
        &self,
        artifact_id: hive_types::ArtifactId,
    ) -> StoreResult<Option<ArtifactRecord>> {
        query_artifact(&self.conn, artifact_id)
    }

    pub fn list_artifacts(&self, mission_id: MissionId) -> StoreResult<Vec<ArtifactRecord>> {
        query_artifacts(&self.conn, mission_id)
    }

    pub fn create_routine(
        &mut self,
        name: &str,
        goal: &str,
        schedule: &str,
        timezone: &str,
        now_ms: i64,
    ) -> StoreResult<RoutineRecord> {
        if !routine::validate_timezone(timezone) {
            return Err(StoreError::InvalidTimezone(timezone.to_owned()));
        }
        let schedule_parsed = routine::parse_schedule(schedule)
            .map_err(|error| StoreError::InvalidSchedule(error.to_string()))?;
        if self.count_routines()? >= MAX_ROUTINES {
            return Err(StoreError::RoutineLimit);
        }
        let record = RoutineRecord {
            routine_id: RoutineId::new(),
            name: name.to_owned(),
            goal: goal.to_owned(),
            schedule: schedule.trim().to_owned(),
            enabled: true,
            timezone: timezone.to_owned(),
            last_run_ms: None,
            next_run_ms: routine::first_next_run_ms(schedule_parsed, now_ms),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        self.conn.execute(
            "INSERT INTO routines (routine_id, name, goal, schedule, enabled, timezone, last_run_ms, next_run_ms, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.routine_id.to_string(),
                record.name,
                record.goal,
                record.schedule,
                record.enabled as i64,
                record.timezone,
                Option::<i64>::None,
                record.next_run_ms,
                record.created_at_ms,
                record.updated_at_ms,
            ],
        )?;
        Ok(record)
    }

    pub fn get_routine(&self, routine_id: RoutineId) -> StoreResult<Option<RoutineRecord>> {
        query_routine(&self.conn, routine_id)
    }

    pub fn list_routines(&self) -> StoreResult<Vec<RoutineRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT routine_id, name, goal, schedule, enabled, timezone, last_run_ms, next_run_ms, created_at_ms, updated_at_ms FROM routines ORDER BY created_at_ms ASC, routine_id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(parse_routine(row?)?);
        }
        Ok(out)
    }

    pub fn count_routines(&self) -> StoreResult<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM routines", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    /// Update `enabled` and/or `schedule`. When a schedule changes or the
    /// routine is re-enabled, `next_run_ms` is recomputed from `now_ms` so
    /// missed windows are skipped rather than replayed.
    pub fn update_routine(
        &mut self,
        routine_id: RoutineId,
        enabled: Option<bool>,
        schedule: Option<&str>,
        now_ms: i64,
    ) -> StoreResult<(RoutineRecord, EventEnvelope)> {
        let current = query_routine(&self.conn, routine_id)?
            .ok_or_else(|| StoreError::NotFound(format!("routine {routine_id}")))?;
        let new_enabled = enabled.unwrap_or(current.enabled);
        let new_schedule = match schedule {
            Some(schedule) => {
                routine::parse_schedule(schedule)
                    .map_err(|error| StoreError::InvalidSchedule(error.to_string()))?;
                schedule.trim().to_owned()
            }
            None => current.schedule.clone(),
        };
        let mut changed = Vec::new();
        if new_schedule != current.schedule {
            changed.push("schedule");
        }
        if new_enabled != current.enabled {
            changed.push("enabled");
        }

        let recompute =
            changed.contains(&"schedule") || (enabled.is_some() && new_enabled && !current.enabled);
        let next_run_ms = if recompute {
            let parsed = routine::parse_schedule(&new_schedule)
                .map_err(|error| StoreError::InvalidSchedule(error.to_string()))?;
            routine::first_next_run_ms(parsed, now_ms)
        } else {
            current.next_run_ms
        };

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE routines SET enabled = ?1, schedule = ?2, next_run_ms = ?3, updated_at_ms = ?4 WHERE routine_id = ?5",
            params![
                new_enabled as i64,
                new_schedule,
                next_run_ms,
                now_ms,
                routine_id.to_string(),
            ],
        )?;
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::RoutineUpdated,
            mission_id: system_mission_id(),
            run_id: None,
            node_id: None,
            session_id: None,
            attempt_no: None,
            trace_id: None,
            payload: json!({
                "routine_id": routine_id.to_string(),
                "changed": changed,
            }),
        };
        let envelope = insert_event(&tx, &event)?;
        tx.commit()?;
        let record = query_routine(&self.conn, routine_id)?.expect("routine was just updated");
        Ok((record, envelope))
    }

    pub fn delete_routine(
        &mut self,
        routine_id: RoutineId,
        _now_ms: i64,
    ) -> StoreResult<EventEnvelope> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = tx.execute(
            "DELETE FROM routines WHERE routine_id = ?1",
            params![routine_id.to_string()],
        )?;
        if deleted == 0 {
            return Err(StoreError::NotFound(format!("routine {routine_id}")));
        }
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::RoutineDeleted,
            mission_id: system_mission_id(),
            run_id: None,
            node_id: None,
            session_id: None,
            attempt_no: None,
            trace_id: None,
            payload: json!({ "routine_id": routine_id.to_string() }),
        };
        let envelope = insert_event(&tx, &event)?;
        tx.commit()?;
        Ok(envelope)
    }

    pub fn list_due_routines(&self, now_ms: i64) -> StoreResult<Vec<RoutineId>> {
        let mut stmt = self.conn.prepare(
            "SELECT routine_id FROM routines WHERE enabled = 1 AND next_run_ms <= ?1 ORDER BY next_run_ms ASC",
        )?;
        let rows = stmt.query_map(params![now_ms], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(RoutineId::parse(&row?)?);
        }
        Ok(out)
    }

    /// Fire one routine window if it is due. A single invalid row is disabled
    /// and reported without failing the caller's broader tick loop.
    pub fn fire_routine(
        &mut self,
        routine_id: RoutineId,
        now_ms: i64,
    ) -> StoreResult<RoutineFireOutcome> {
        let row = match query_routine(&self.conn, routine_id)? {
            Some(row) => row,
            None => return Ok(RoutineFireOutcome::NotFound),
        };
        if !row.enabled {
            return Ok(RoutineFireOutcome::NotDue);
        }
        let schedule = match routine::parse_schedule(&row.schedule) {
            Ok(schedule) => schedule,
            Err(error) => {
                return self
                    .disable_routine_and_emit(
                        routine_id,
                        "invalid_schedule",
                        error.to_string(),
                        now_ms,
                    )
                    .map(|envelope| RoutineFireOutcome::Disabled { envelope });
            }
        };
        let fire_at_ms = row.next_run_ms;
        if fire_at_ms > now_ms {
            return Ok(RoutineFireOutcome::NotDue);
        }
        let new_next = routine::next_run_after(schedule, fire_at_ms, now_ms);

        match self.fire_once(routine_id, &row, fire_at_ms, new_next, now_ms) {
            Ok(outcome) => Ok(outcome),
            Err(_) => {
                self.record_fire_failure(routine_id, fire_at_ms, new_next, now_ms)?;
                Ok(RoutineFireOutcome::Failed { fire_at_ms })
            }
        }
    }

    fn fire_once(
        &mut self,
        routine_id: RoutineId,
        row: &RoutineRecord,
        fire_at_ms: i64,
        new_next: i64,
        now_ms: i64,
    ) -> StoreResult<RoutineFireOutcome> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let claimed = tx.execute(
            "UPDATE routines SET next_run_ms = ?1, last_run_ms = ?2, updated_at_ms = ?2 WHERE routine_id = ?3 AND next_run_ms = ?4 AND enabled = 1",
            params![new_next, now_ms, routine_id.to_string(), fire_at_ms],
        )?;
        if claimed == 0 {
            return Ok(RoutineFireOutcome::AlreadyFired);
        }
        let mission = MissionRecord {
            mission_id: MissionId::new(),
            state: MissionState::Queued,
            title: row.name.clone(),
            goal: row.goal.clone(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        tx.execute(
            "INSERT INTO missions (mission_id, state, title, goal, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                mission.mission_id.to_string(),
                mission.state.as_str(),
                mission.title,
                mission.goal,
                mission.created_at_ms,
                mission.updated_at_ms,
            ],
        )?;
        tx.execute(
            "INSERT INTO routine_fires (routine_id, fire_at_ms, mission_id, state, created_at_ms) VALUES (?1, ?2, ?3, 'created', ?4)",
            params![
                routine_id.to_string(),
                fire_at_ms,
                mission.mission_id.to_string(),
                now_ms,
            ],
        )?;
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::RoutineTriggered,
            mission_id: mission.mission_id,
            run_id: None,
            node_id: None,
            session_id: None,
            attempt_no: None,
            trace_id: None,
            payload: json!({
                "routine_id": routine_id.to_string(),
                "fire_at_ms": fire_at_ms,
                "mission_id": mission.mission_id.to_string(),
            }),
        };
        let envelope = insert_event(&tx, &event)?;
        tx.commit()?;
        Ok(RoutineFireOutcome::Fired {
            mission,
            fire_at_ms,
            envelope,
        })
    }

    fn disable_routine_and_emit(
        &mut self,
        routine_id: RoutineId,
        code: &str,
        message: String,
        now_ms: i64,
    ) -> StoreResult<EventEnvelope> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE routines SET enabled = 0, updated_at_ms = ?1 WHERE routine_id = ?2",
            params![now_ms, routine_id.to_string()],
        )?;
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::RoutineError,
            mission_id: system_mission_id(),
            run_id: None,
            node_id: None,
            session_id: None,
            attempt_no: None,
            trace_id: None,
            payload: json!({
                "routine_id": routine_id.to_string(),
                "code": code,
                "message": message,
            }),
        };
        let envelope = insert_event(&tx, &event)?;
        tx.commit()?;
        Ok(envelope)
    }

    fn record_fire_failure(
        &mut self,
        routine_id: RoutineId,
        fire_at_ms: i64,
        new_next: i64,
        now_ms: i64,
    ) -> StoreResult<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let _ = tx.execute(
            "INSERT INTO routine_fires (routine_id, fire_at_ms, mission_id, state, created_at_ms) VALUES (?1, ?2, NULL, 'failed', ?3)",
            params![routine_id.to_string(), fire_at_ms, now_ms],
        );
        tx.execute(
            "UPDATE routines SET next_run_ms = ?1, last_run_ms = ?2, updated_at_ms = ?2 WHERE routine_id = ?3",
            params![new_next, now_ms, routine_id.to_string()],
        )?;
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::RoutineError,
            mission_id: system_mission_id(),
            run_id: None,
            node_id: None,
            session_id: None,
            attempt_no: None,
            trace_id: None,
            payload: json!({
                "routine_id": routine_id.to_string(),
                "code": "routine_fire_failed",
                "message": "failed to create mission for routine window",
            }),
        };
        insert_event(&tx, &event)?;
        tx.commit()?;
        Ok(())
    }
}

fn http_cursor(seq: u64) -> StoreResult<i64> {
    if seq > MAX_SEQ {
        return Err(StoreError::SequenceExhausted);
    }
    i64::try_from(seq).map_err(|_| StoreError::SequenceExhausted)
}

fn list_missions_on(
    conn: &Connection,
    limit: i64,
    offset: i64,
) -> StoreResult<Vec<MissionListRecord>> {
    let mut stmt = conn.prepare(
        "SELECT m.mission_id, m.state, m.title, m.goal, m.created_at_ms, m.updated_at_ms, \
         (SELECT MAX(seq) FROM events e WHERE e.mission_id = m.mission_id), \
         (SELECT COUNT(*) FROM approvals a WHERE a.mission_id = m.mission_id AND a.state = 'pending') \
         FROM missions m \
         ORDER BY m.updated_at_ms DESC, m.created_at_ms DESC, m.mission_id \
         LIMIT ?1 OFFSET ?2",
    )?;
    let rows = stmt.query_map(params![limit, offset], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, i64>(7)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, state, title, goal, created_at_ms, updated_at_ms, last_seq, pending) = row?;
        out.push(MissionListRecord {
            mission: MissionRecord {
                mission_id: MissionId::parse(&id)?,
                state: MissionState::parse(&state)?,
                title,
                goal,
                created_at_ms,
                updated_at_ms,
            },
            last_seq: last_seq.map(|seq| seq as u64),
            pending_approvals: pending as u64,
        });
    }
    Ok(out)
}

fn latest_seq_on(conn: &Connection) -> StoreResult<Option<u64>> {
    // `seq` is AUTOINCREMENT, so sqlite_sequence keeps the high-water mark
    // even after archived rows are deleted. Retention must not move
    // `latest_seq` backwards for clients that still hold a cursor.
    let value: i64 = conn.query_row(
        "SELECT MAX(\
            COALESCE((SELECT MAX(seq) FROM events), 0),\
            COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'events'), 0)\
        )",
        [],
        |row| row.get(0),
    )?;
    Ok((value > 0).then_some(value as u64))
}

fn mission_latest_seq_on(conn: &Connection, mission_id: MissionId) -> StoreResult<Option<u64>> {
    let value: Option<i64> = conn.query_row(
        "SELECT MAX(seq) FROM events WHERE mission_id = ?1",
        params![mission_id.to_string()],
        |row| row.get(0),
    )?;
    Ok(value.map(|seq| seq as u64))
}

fn counts_on(conn: &Connection) -> StoreResult<(u64, u64, u64, u64)> {
    let missions: i64 = conn.query_row("SELECT COUNT(*) FROM missions", [], |row| row.get(0))?;
    let runs: i64 = conn.query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))?;
    let nodes: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))?;
    let events: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
    Ok((missions as u64, runs as u64, nodes as u64, events as u64))
}

fn list_pending_approvals_on(conn: &Connection, now_ms: i64) -> StoreResult<Vec<ApprovalRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {APPROVAL_COLUMNS} FROM approvals WHERE state = 'pending' AND expires_at_ms > ?1 ORDER BY expires_at_ms ASC"
    ))?;
    let rows = stmt.query_map(params![now_ms], raw_approval_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(parse_approval(row?)?);
    }
    Ok(out)
}

fn query_runs_limited(
    conn: &Connection,
    mission_id: MissionId,
    limit: i64,
) -> StoreResult<Vec<RunRecord>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, mission_id, state, created_at_ms, updated_at_ms FROM runs WHERE mission_id = ?1 ORDER BY run_id LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![mission_id.to_string(), limit], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, mission_id, state, created_at_ms, updated_at_ms) = row?;
        out.push(RunRecord {
            run_id: RunId::parse(&id)?,
            mission_id: MissionId::parse(&mission_id)?,
            state: RunState::parse(&state)?,
            created_at_ms,
            updated_at_ms,
        });
    }
    Ok(out)
}

fn query_nodes_limited(
    conn: &Connection,
    mission_id: MissionId,
    limit: i64,
) -> StoreResult<Vec<NodeRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {NODE_COLUMNS} FROM nodes WHERE mission_id = ?1 ORDER BY node_id LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![mission_id.to_string(), limit], raw_node_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(parse_node(row?)?);
    }
    Ok(out)
}

fn query_approvals_by_mission(
    conn: &Connection,
    mission_id: MissionId,
) -> StoreResult<Vec<ApprovalRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {APPROVAL_COLUMNS} FROM approvals WHERE mission_id = ?1 ORDER BY created_at_ms ASC"
    ))?;
    let rows = stmt.query_map(params![mission_id.to_string()], raw_approval_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(parse_approval(row?)?);
    }
    Ok(out)
}

fn query_artifact(
    conn: &Connection,
    artifact_id: hive_types::ArtifactId,
) -> StoreResult<Option<ArtifactRecord>> {
    let row = conn
        .query_row(
            "SELECT artifact_id, mission_id, node_id, tool_call_id, path, sha256, size_bytes, created_at_ms FROM artifacts WHERE artifact_id = ?1",
            params![artifact_id.to_string()],
            raw_artifact_row,
        )
        .optional()?;
    row.map(parse_artifact).transpose()
}

fn query_artifacts(conn: &Connection, mission_id: MissionId) -> StoreResult<Vec<ArtifactRecord>> {
    let mut stmt = conn.prepare(
        "SELECT artifact_id, mission_id, node_id, tool_call_id, path, sha256, size_bytes, created_at_ms FROM artifacts WHERE mission_id = ?1 ORDER BY created_at_ms ASC",
    )?;
    let rows = stmt.query_map(params![mission_id.to_string()], raw_artifact_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(parse_artifact(row?)?);
    }
    Ok(out)
}

type ArtifactRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    i64,
    i64,
);

fn raw_artifact_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn parse_artifact(raw: ArtifactRow) -> StoreResult<ArtifactRecord> {
    let (artifact_id, mission_id, node_id, tool_call_id, path, sha256, size_bytes, created_at_ms) =
        raw;
    Ok(ArtifactRecord {
        artifact_id: hive_types::ArtifactId::parse(&artifact_id)?,
        mission_id: MissionId::parse(&mission_id)?,
        node_id: node_id.map(|id| NodeId::parse(&id)).transpose()?,
        tool_call_id: tool_call_id.map(|id| ToolCallId::parse(&id)).transpose()?,
        path,
        sha256,
        size_bytes,
        created_at_ms,
    })
}

fn query_messages(conn: &Connection, mission_id: MissionId) -> StoreResult<Vec<EventEnvelope>> {
    let mut stmt = conn.prepare(
        "SELECT envelope_json FROM events WHERE mission_id = ?1 AND kind = 'message.user' ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map(params![mission_id.to_string()], |row| {
        row.get::<_, String>(0)
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(serde_json::from_str(&row?)?);
    }
    Ok(out)
}

fn query_events_page(
    conn: &Connection,
    mission_id: Option<MissionId>,
    after_seq: i64,
    limit: i64,
) -> StoreResult<Vec<EventEnvelope>> {
    let mut stmt = conn.prepare(
        "SELECT envelope_json FROM events WHERE (?1 IS NULL OR mission_id = ?1) AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![mission_id.map(|id| id.to_string()), after_seq, limit],
        |row| row.get::<_, String>(0),
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(serde_json::from_str(&row?)?);
    }
    Ok(out)
}

const NODE_COLUMNS: &str = "node_id, mission_id, run_id, state, attempt_no, retry_budget, lease_token, lease_owner, lease_expires_at_ms, created_at_ms, updated_at_ms";

const AGENT_SESSION_COLUMNS: &str = "session_id, mission_id, run_id, node_id, role, model, system_prompt, state, created_at_ms, updated_at_ms, closed_at_ms";

const TURN_COLUMNS: &str = "turn_id, session_id, node_id, attempt_no, turn_index, request_model, finish_reason, state, output_preview, tool_call_count, created_at_ms, updated_at_ms";

const HANDOFF_COLUMNS: &str = "handoff_id, mission_id, run_id, node_id, attempt_no, agent, schema_version, objective, summary, base_revision, created_at_ms, inputs_json, artifacts_json, decisions_json, assumptions_json, risks_json, open_questions_json, acceptance_json, recommended_next_json";

type RawHandoff = (
    String,
    String,
    String,
    String,
    i64,
    String,
    u32,
    String,
    String,
    String,
    i64,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

fn raw_handoff_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawHandoff> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
        row.get(17)?,
        row.get(18)?,
    ))
}

fn parse_handoff(raw: RawHandoff) -> StoreResult<HandoffRecord> {
    let (
        handoff_id,
        mission_id,
        run_id,
        node_id,
        attempt_no,
        agent,
        schema_version,
        objective,
        summary,
        base_revision,
        created_at_ms,
        inputs_json,
        artifacts_json,
        decisions_json,
        assumptions_json,
        risks_json,
        open_questions_json,
        acceptance_json,
        recommended_next_json,
    ) = raw;
    Ok(HandoffRecord {
        handoff_id: HandoffId::parse(&handoff_id)?,
        mission_id: MissionId::parse(&mission_id)?,
        run_id: RunId::parse(&run_id)?,
        node_id: NodeId::parse(&node_id)?,
        attempt_no: AttemptNo::new(attempt_no as u32)?,
        agent,
        schema_version,
        objective,
        summary,
        base_revision,
        created_at_ms,
        inputs: serde_json::from_str(&inputs_json)?,
        artifacts: serde_json::from_str(&artifacts_json)?,
        decisions: serde_json::from_str(&decisions_json)?,
        assumptions: serde_json::from_str(&assumptions_json)?,
        risks: serde_json::from_str(&risks_json)?,
        open_questions: serde_json::from_str(&open_questions_json)?,
        acceptance: serde_json::from_str(&acceptance_json)?,
        recommended_next: serde_json::from_str(&recommended_next_json)?,
    })
}

fn query_handoff(
    conn: &Connection,
    node_id: NodeId,
    attempt_no: AttemptNo,
) -> StoreResult<Option<HandoffRecord>> {
    let row = conn
        .query_row(
            &format!(
                "SELECT {HANDOFF_COLUMNS} FROM handoffs WHERE node_id = ?1 AND attempt_no = ?2"
            ),
            params![node_id.to_string(), attempt_no.get()],
            raw_handoff_row,
        )
        .optional()?;
    row.map(parse_handoff).transpose()
}

fn insert_handoff_on(
    conn: &Connection,
    handoff: &Handoff,
    created_at_ms: i64,
) -> StoreResult<HandoffRecord> {
    let record = HandoffRecord {
        handoff_id: HandoffId::new(),
        mission_id: handoff.mission_id,
        run_id: handoff.run_id,
        node_id: handoff.node_id,
        attempt_no: handoff.provenance.attempt_no,
        agent: handoff.agent.clone(),
        schema_version: handoff.schema_version,
        objective: handoff.objective.clone(),
        summary: handoff.summary.clone(),
        base_revision: handoff.provenance.base_revision.clone(),
        created_at_ms,
        inputs: handoff.inputs.clone(),
        artifacts: handoff.artifacts.clone(),
        decisions: handoff.decisions.clone(),
        assumptions: handoff.assumptions.clone(),
        risks: handoff.risks.clone(),
        open_questions: handoff.open_questions.clone(),
        acceptance: handoff.acceptance.clone(),
        recommended_next: handoff.recommended_next.clone(),
    };
    let inputs_json = serde_json::to_string(&record.inputs)?;
    let artifacts_json = serde_json::to_string(&record.artifacts)?;
    let decisions_json = serde_json::to_string(&record.decisions)?;
    let assumptions_json = serde_json::to_string(&record.assumptions)?;
    let risks_json = serde_json::to_string(&record.risks)?;
    let open_questions_json = serde_json::to_string(&record.open_questions)?;
    let acceptance_json = serde_json::to_string(&record.acceptance)?;
    let recommended_next_json = serde_json::to_string(&record.recommended_next)?;
    let result = conn.execute(
        "INSERT INTO handoffs (handoff_id, mission_id, run_id, node_id, attempt_no, agent, schema_version, objective, summary, base_revision, created_at_ms, inputs_json, artifacts_json, decisions_json, assumptions_json, risks_json, open_questions_json, acceptance_json, recommended_next_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            record.handoff_id.to_string(),
            record.mission_id.to_string(),
            record.run_id.to_string(),
            record.node_id.to_string(),
            record.attempt_no.get(),
            record.agent,
            record.schema_version,
            record.objective,
            record.summary,
            record.base_revision,
            record.created_at_ms,
            inputs_json,
            artifacts_json,
            decisions_json,
            assumptions_json,
            risks_json,
            open_questions_json,
            acceptance_json,
            recommended_next_json,
        ],
    );
    match result {
        Ok(_) => Ok(record),
        Err(err) if is_unique_violation(&err) => Err(StoreError::Conflict(format!(
            "handoff already exists for node {} attempt {}",
            handoff.node_id,
            handoff.provenance.attempt_no.get()
        ))),
        Err(err) => Err(StoreError::from(err)),
    }
}

type RawAgentSession = (
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
    Option<i64>,
);

fn raw_agent_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawAgentSession> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn parse_agent_session(raw: RawAgentSession) -> StoreResult<AgentSessionRecord> {
    let (
        session_id,
        mission_id,
        run_id,
        node_id,
        role,
        model,
        system_prompt,
        state,
        created_at_ms,
        updated_at_ms,
        closed_at_ms,
    ) = raw;
    Ok(AgentSessionRecord {
        session_id: AgentSessionId::parse(&session_id)?,
        mission_id: MissionId::parse(&mission_id)?,
        run_id: match run_id {
            Some(value) => Some(RunId::parse(&value)?),
            None => None,
        },
        node_id: NodeId::parse(&node_id)?,
        role,
        model,
        system_prompt,
        state: AgentSessionState::parse(&state)
            .ok_or_else(|| StoreError::Conflict(format!("unknown session state {state}")))?,
        created_at_ms,
        updated_at_ms,
        closed_at_ms,
    })
}

fn query_agent_session(
    conn: &Connection,
    session_id: AgentSessionId,
) -> StoreResult<Option<AgentSessionRecord>> {
    let row = conn
        .query_row(
            &format!("SELECT {AGENT_SESSION_COLUMNS} FROM agent_sessions WHERE session_id = ?1"),
            params![session_id.to_string()],
            raw_agent_session_row,
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some(raw) => Ok(Some(parse_agent_session(raw)?)),
    }
}

type RawTurn = (
    String,
    String,
    String,
    u32,
    u32,
    String,
    Option<String>,
    String,
    String,
    u32,
    i64,
    i64,
);

fn raw_turn_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawTurn> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
    ))
}

fn parse_turn(raw: RawTurn) -> StoreResult<TurnRecord> {
    let (
        turn_id,
        session_id,
        node_id,
        attempt_no,
        turn_index,
        request_model,
        finish_reason,
        state,
        output_preview,
        tool_call_count,
        created_at_ms,
        updated_at_ms,
    ) = raw;
    Ok(TurnRecord {
        turn_id: TurnId::parse(&turn_id)?,
        session_id: AgentSessionId::parse(&session_id)?,
        node_id: NodeId::parse(&node_id)?,
        attempt_no: AttemptNo::new(attempt_no)?,
        turn_index,
        request_model,
        finish_reason,
        state: TurnState::parse(&state)
            .ok_or_else(|| StoreError::Conflict(format!("unknown turn state {state}")))?,
        output_preview,
        tool_call_count,
        created_at_ms,
        updated_at_ms,
    })
}

fn query_turn(conn: &Connection, turn_id: TurnId) -> StoreResult<Option<TurnRecord>> {
    let row = conn
        .query_row(
            &format!("SELECT {TURN_COLUMNS} FROM turns WHERE turn_id = ?1"),
            params![turn_id.to_string()],
            raw_turn_row,
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some(raw) => Ok(Some(parse_turn(raw)?)),
    }
}

fn to_sql_integer(value: u64) -> StoreResult<i64> {
    i64::try_from(value)
        .map_err(|_| StoreError::Conflict("token count exceeds SQLite INTEGER".to_owned()))
}

fn from_sql_integer(value: i64) -> StoreResult<u64> {
    u64::try_from(value)
        .map_err(|_| StoreError::Conflict("negative token count in database".to_owned()))
}

type RawNode = (
    String,
    String,
    String,
    String,
    u32,
    u32,
    Option<String>,
    Option<String>,
    Option<i64>,
    i64,
    i64,
);

fn raw_node_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawNode> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn parse_node(raw: RawNode) -> StoreResult<NodeRecord> {
    let (
        node_id,
        mission_id,
        run_id,
        state,
        attempt_no,
        retry_budget,
        lease_token,
        lease_owner,
        lease_expires_at_ms,
        created_at_ms,
        updated_at_ms,
    ) = raw;
    let lease_token = match lease_token {
        Some(token) => Some(LeaseToken::parse(&token)?),
        None => None,
    };
    Ok(NodeRecord {
        node_id: NodeId::parse(&node_id)?,
        mission_id: MissionId::parse(&mission_id)?,
        run_id: RunId::parse(&run_id)?,
        state: NodeState::parse(&state)?,
        attempt_no: AttemptNo::new(attempt_no)?,
        retry_budget,
        lease_token,
        lease_owner,
        lease_expires_at_ms,
        created_at_ms,
        updated_at_ms,
    })
}

const APPROVAL_COLUMNS: &str = "approval_id, mission_id, run_id, node_id, session_id, tool_call_id, tool_name, tool_version, request_hash, arguments_preview, policy_version, attempt_no, lease_token, state, expires_at_ms, created_at_ms, decided_at_ms, decided_by, decision_note, consumed_at_ms";

type RawApproval = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
    String,
    u32,
    String,
    String,
    String,
    u32,
    String,
    String,
    i64,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<i64>,
);

fn raw_approval_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawApproval> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
        row.get(17)?,
        row.get(18)?,
        row.get(19)?,
    ))
}

fn parse_approval(raw: RawApproval) -> StoreResult<ApprovalRecord> {
    let (
        approval_id,
        mission_id,
        run_id,
        node_id,
        session_id,
        tool_call_id,
        tool_name,
        tool_version,
        request_hash,
        arguments_preview,
        policy_version,
        attempt_no,
        lease_token,
        state,
        expires_at_ms,
        created_at_ms,
        decided_at_ms,
        decided_by,
        decision_note,
        consumed_at_ms,
    ) = raw;
    let run_id = match run_id {
        Some(value) => Some(RunId::parse(&value)?),
        None => None,
    };
    let session_id = match session_id {
        Some(value) => Some(AgentSessionId::parse(&value)?),
        None => None,
    };
    Ok(ApprovalRecord {
        approval_id: ApprovalId::parse(&approval_id)?,
        mission_id: MissionId::parse(&mission_id)?,
        run_id,
        node_id: NodeId::parse(&node_id)?,
        session_id,
        tool_call_id: ToolCallId::parse(&tool_call_id)?,
        tool_name,
        tool_version,
        request_hash,
        arguments_preview,
        policy_version,
        attempt_no: AttemptNo::new(attempt_no)?,
        lease_token: LeaseToken::parse(&lease_token)?,
        state: ApprovalState::parse(&state)?,
        expires_at_ms,
        created_at_ms,
        decided_at_ms,
        decided_by,
        decision_note,
        consumed_at_ms,
    })
}

fn query_mission(conn: &Connection, mission_id: MissionId) -> StoreResult<Option<MissionRecord>> {
    let row = conn
        .query_row(
            "SELECT mission_id, state, title, goal, created_at_ms, updated_at_ms FROM missions WHERE mission_id = ?1",
            params![mission_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some((id, state, title, goal, created_at_ms, updated_at_ms)) => Ok(Some(MissionRecord {
            mission_id: MissionId::parse(&id)?,
            state: MissionState::parse(&state)?,
            title,
            goal,
            created_at_ms,
            updated_at_ms,
        })),
    }
}

fn query_run(conn: &Connection, run_id: RunId) -> StoreResult<Option<RunRecord>> {
    let row = conn
        .query_row(
            "SELECT run_id, mission_id, state, created_at_ms, updated_at_ms FROM runs WHERE run_id = ?1",
            params![run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some((id, mission_id, state, created_at_ms, updated_at_ms)) => Ok(Some(RunRecord {
            run_id: RunId::parse(&id)?,
            mission_id: MissionId::parse(&mission_id)?,
            state: RunState::parse(&state)?,
            created_at_ms,
            updated_at_ms,
        })),
    }
}

fn query_runs(conn: &Connection, mission_id: MissionId) -> StoreResult<Vec<RunRecord>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, mission_id, state, created_at_ms, updated_at_ms FROM runs WHERE mission_id = ?1 ORDER BY run_id",
    )?;
    let rows = stmt.query_map(params![mission_id.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, mission_id, state, created_at_ms, updated_at_ms) = row?;
        out.push(RunRecord {
            run_id: RunId::parse(&id)?,
            mission_id: MissionId::parse(&mission_id)?,
            state: RunState::parse(&state)?,
            created_at_ms,
            updated_at_ms,
        });
    }
    Ok(out)
}

fn query_node(conn: &Connection, node_id: NodeId) -> StoreResult<Option<NodeRecord>> {
    let row = conn
        .query_row(
            &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE node_id = ?1"),
            params![node_id.to_string()],
            raw_node_row,
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some(raw) => parse_node(raw).map(Some),
    }
}

fn query_nodes_by_mission(
    conn: &Connection,
    mission_id: MissionId,
) -> StoreResult<Vec<NodeRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {NODE_COLUMNS} FROM nodes WHERE mission_id = ?1 ORDER BY node_id"
    ))?;
    let rows = stmt.query_map(params![mission_id.to_string()], raw_node_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(parse_node(row?)?);
    }
    Ok(out)
}

fn query_approval(
    conn: &Connection,
    approval_id: ApprovalId,
) -> StoreResult<Option<ApprovalRecord>> {
    let row = conn
        .query_row(
            &format!("SELECT {APPROVAL_COLUMNS} FROM approvals WHERE approval_id = ?1"),
            params![approval_id.to_string()],
            raw_approval_row,
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some(raw) => parse_approval(raw).map(Some),
    }
}

fn update_node_state_on(
    conn: &Connection,
    node_id: NodeId,
    new_state: NodeState,
    now_ms: i64,
) -> StoreResult<NodeRecord> {
    let current = query_node(conn, node_id)?
        .ok_or_else(|| StoreError::NotFound(format!("node {node_id}")))?;
    let next = current.state.transition(new_state)?;
    conn.execute(
        "UPDATE nodes SET state = ?1, updated_at_ms = ?2 WHERE node_id = ?3",
        params![next.as_str(), now_ms, node_id.to_string()],
    )?;
    query_node(conn, node_id).map(|record| record.expect("node was just updated"))
}

/// Atomic budget deduction. A missing row means "no allocation" (unlimited, no
/// tracking) and succeeds; an existing row applies the CAS guard
/// `consumed + delta <= hard_limit` and returns [`StoreError::BudgetExceeded`]
/// when the hard limit would be crossed.
fn deduct_budget_on(
    conn: &Connection,
    scope_kind: &str,
    scope_id: &str,
    dimension: BudgetDimension,
    delta: i64,
    now_ms: i64,
) -> StoreResult<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM budgets WHERE scope_kind = ?1 AND scope_id = ?2 AND dimension = ?3)",
        params![scope_kind, scope_id, dimension.as_str()],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let affected = conn.execute(
        "UPDATE budgets SET consumed = consumed + ?1, updated_at_ms = ?2 WHERE scope_kind = ?3 AND scope_id = ?4 AND dimension = ?5 AND (hard_limit IS NULL OR consumed + ?1 <= hard_limit)",
        params![delta, now_ms, scope_kind, scope_id, dimension.as_str()],
    )?;
    if affected == 1 {
        Ok(())
    } else {
        Err(StoreError::BudgetExceeded {
            dimension: dimension.as_str().to_owned(),
        })
    }
}

fn tool_event_fields(event: &EventInput) -> StoreResult<(String, String, u32, String)> {
    let tool_call_id = tool_call_id_from(event)?;
    let tool_name = event
        .payload
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| StoreError::Conflict("tool event missing tool_name".to_owned()))?
        .to_owned();
    let tool_version = event
        .payload
        .get("tool_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1) as u32;
    let request_hash = event
        .payload
        .get("request_hash")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    Ok((tool_call_id, tool_name, tool_version, request_hash))
}

fn tool_call_id_from(event: &EventInput) -> StoreResult<String> {
    event
        .payload
        .get("tool_call_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| StoreError::Conflict("tool event missing tool_call_id".to_owned()))
}

/// Fixed, well-formed v4 Mission identity for process-level events (the
/// `routine.*` family other than `routine.triggered`) that have no single
/// Mission owner. The `events` table requires a non-null `mission_id`, but
/// these events must not be attributed to any real Mission.
fn system_mission_id() -> MissionId {
    MissionId::parse(SYSTEM_SCOPE_MISSION_ID).expect("system scope mission id is a valid v4 uuid")
}

fn query_routine(conn: &Connection, routine_id: RoutineId) -> StoreResult<Option<RoutineRecord>> {
    let row = conn
        .query_row(
            "SELECT routine_id, name, goal, schedule, enabled, timezone, last_run_ms, next_run_ms, created_at_ms, updated_at_ms FROM routines WHERE routine_id = ?1",
            params![routine_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                ))
            },
        )
        .optional()?;
    row.map(parse_routine).transpose()
}

fn parse_routine(
    raw: (
        String,
        String,
        String,
        String,
        i64,
        String,
        Option<i64>,
        i64,
        i64,
        i64,
    ),
) -> StoreResult<RoutineRecord> {
    let (
        routine_id,
        name,
        goal,
        schedule,
        enabled,
        timezone,
        last_run_ms,
        next_run_ms,
        created_at_ms,
        updated_at_ms,
    ) = raw;
    Ok(RoutineRecord {
        routine_id: RoutineId::parse(&routine_id)?,
        name,
        goal,
        schedule,
        enabled: enabled != 0,
        timezone,
        last_run_ms,
        next_run_ms,
        created_at_ms,
        updated_at_ms,
    })
}

fn insert_event(conn: &Connection, input: &EventInput) -> StoreResult<EventEnvelope> {
    let created_at = now_rfc3339();
    let created_at_ms = now_ms();
    let schema_version = EVENT_SCHEMA_VERSION;

    let current_max: Option<i64> =
        conn.query_row("SELECT MAX(seq) FROM events", [], |row| row.get(0))?;
    if current_max.unwrap_or(0) as u64 >= MAX_SEQ {
        return Err(StoreError::SequenceExhausted);
    }

    let result = conn.execute(
        "INSERT INTO events (event_id, mission_id, kind, schema_version, envelope_json, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![input.event_id.to_string(), input.mission_id.to_string(), input.kind.as_str(), schema_version, "", created_at_ms],
    );

    let seq = match result {
        Ok(_) => conn.last_insert_rowid(),
        Err(err) if is_unique_violation(&err) => {
            return Err(StoreError::DuplicateEvent(input.event_id.to_string()));
        }
        Err(err) => return Err(StoreError::from(err)),
    };
    let seq = u64::try_from(seq).map_err(|_| StoreError::SequenceExhausted)?;
    if seq > MAX_SEQ {
        return Err(StoreError::SequenceExhausted);
    }
    let seq_i64 = i64::try_from(seq).map_err(|_| StoreError::SequenceExhausted)?;

    let envelope = EventEnvelope::new(input.clone(), seq, created_at);
    let envelope_json = serde_json::to_string(&envelope)?;
    conn.execute(
        "UPDATE events SET envelope_json = ?1 WHERE seq = ?2",
        params![envelope_json, seq_i64],
    )?;

    Ok(envelope)
}

fn validate_cursor(seq: u64) -> StoreResult<i64> {
    if seq == 0 || seq > MAX_SEQ {
        return Err(StoreError::SequenceExhausted);
    }
    i64::try_from(seq).map_err(|_| StoreError::SequenceExhausted)
}

fn io_archive_error(error: std::io::Error) -> StoreError {
    StoreError::Archive(error.to_string())
}

fn create_approval_on(
    conn: &Connection,
    input: &ApprovalInput,
    now_ms: i64,
) -> StoreResult<ApprovalRecord> {
    let record = ApprovalRecord {
        approval_id: ApprovalId::new(),
        mission_id: input.mission_id,
        run_id: input.run_id,
        node_id: input.node_id,
        session_id: input.session_id,
        tool_call_id: input.tool_call_id,
        tool_name: input.tool_name.clone(),
        tool_version: input.tool_version,
        request_hash: input.request_hash.clone(),
        arguments_preview: input.arguments_preview.clone(),
        policy_version: input.policy_version.clone(),
        attempt_no: input.attempt_no,
        lease_token: input.lease_token.clone(),
        state: ApprovalState::Pending,
        expires_at_ms: input.expires_at_ms,
        created_at_ms: now_ms,
        decided_at_ms: None,
        decided_by: None,
        decision_note: None,
        consumed_at_ms: None,
    };

    let result = conn.execute(
        "INSERT INTO approvals (approval_id, mission_id, run_id, node_id, session_id, tool_call_id, tool_name, tool_version, request_hash, arguments_preview, policy_version, attempt_no, lease_token, state, expires_at_ms, created_at_ms, decided_at_ms, decided_by, decision_note, consumed_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            record.approval_id.to_string(),
            record.mission_id.to_string(),
            record.run_id.map(|id| id.to_string()),
            record.node_id.to_string(),
            record.session_id.map(|id| id.to_string()),
            record.tool_call_id.to_string(),
            record.tool_name,
            record.tool_version,
            record.request_hash,
            record.arguments_preview,
            record.policy_version,
            record.attempt_no.get(),
            record.lease_token.as_str(),
            record.state.as_str(),
            record.expires_at_ms,
            record.created_at_ms,
            Option::<i64>::None,
            Option::<String>::None,
            Option::<String>::None,
            Option::<i64>::None,
        ],
    );

    match result {
        Ok(_) => Ok(record),
        Err(err) if is_unique_violation(&err) => Err(StoreError::Conflict(format!(
            "tool call {} already has an approval",
            input.tool_call_id
        ))),
        Err(err) => Err(StoreError::from(err)),
    }
}

fn is_unique_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(error, _) if error.extended_code == 2067
    )
}

fn load_retention_floor(path: &Path) -> StoreResult<u64> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(StoreError::Retention(format!(
                "failed to read {}: {error}",
                path.display()
            )))
        }
    };
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        StoreError::Retention(format!("invalid JSON in {}: {error}", path.display()))
    })?;
    let floor = value
        .get("retained_since_seq")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            StoreError::Retention(format!(
                "{} must contain an unsigned integer `retained_since_seq`",
                path.display()
            ))
        })?;
    if floor > MAX_SEQ {
        return Err(StoreError::Retention(format!(
            "{} retained_since_seq {floor} exceeds maximum {MAX_SEQ}",
            path.display()
        )));
    }
    Ok(floor)
}

fn audit_entry_hash(prev_hash: &str, record_json: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(b"\n");
    hasher.update(record_json.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use hive_types::{
        AgentSessionId, ApprovalState, ArtifactId, AttemptNo, EventId, EventInput, EventKind,
        Handoff, HandoffArtifact, HandoffProvenance, LeaseToken, MissionId, MissionState, NodeId,
        NodeState, RunId, ToolCallId, HANDOFF_SCHEMA_VERSION,
    };

    use super::{RoutineFireOutcome, Store, DEFAULT_RETENTION_DAYS};
    use crate::error::StoreError;
    use crate::records::{
        can_retry, AgentSessionState, ApprovalDecision, ApprovalInput, BudgetDimension,
        BudgetLimits, LeaseGuard, MissionRecord, NewAgentSession, NewTurnUsage, NodeRecord,
        RunRecord, SnapshotInput, TurnState, UsageSource,
    };
    use crate::schema::{BUSY_TIMEOUT_MS, SCHEMA_VERSION, V1_SCHEMA};
    use crate::{MAX_SEQ, SNAPSHOT_SCHEMA_VERSION};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "hive-store-{name}-{}-{nanos}-{counter}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn event(
        mission_id: MissionId,
        node_id: Option<NodeId>,
        kind: EventKind,
        attempt: Option<AttemptNo>,
    ) -> EventInput {
        EventInput {
            event_id: EventId::new(),
            kind,
            mission_id,
            run_id: None,
            node_id,
            session_id: None,
            attempt_no: attempt,
            trace_id: None,
            payload: serde_json::json!({}),
        }
    }

    fn setup() -> (Store, MissionRecord, RunRecord, NodeRecord) {
        let mut store = Store::open_in_memory().expect("store");
        let mission = store.create_mission("m", "g", 0).expect("mission");
        let run = store.create_run(mission.mission_id, 0).expect("run");
        let node = store
            .add_node(mission.mission_id, run.run_id, 3, 0)
            .expect("node");
        (store, mission, run, node)
    }

    fn drive_to_needs_recovery(store: &mut Store, node_id: NodeId) {
        store
            .update_node_state(node_id, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .lease_node(node_id, &token, "exec", 1_000, 0)
            .expect("lease");
        store
            .update_node_state(node_id, NodeState::Running, 0)
            .expect("running");
        store.reclaim_expired_leases(2_000).expect("reclaim");
    }

    fn approval_input(mission_id: MissionId, node_id: NodeId, expires_at_ms: i64) -> ApprovalInput {
        ApprovalInput {
            mission_id,
            run_id: None,
            node_id,
            session_id: None,
            tool_call_id: ToolCallId::new(),
            tool_name: "shell".to_owned(),
            tool_version: 1,
            request_hash: "deadbeef".to_owned(),
            arguments_preview: "echo hi".to_owned(),
            policy_version: "v1".to_owned(),
            attempt_no: AttemptNo::FIRST,
            lease_token: LeaseToken::generate(),
            expires_at_ms,
        }
    }

    fn handoff_input(
        mission_id: MissionId,
        run_id: RunId,
        node_id: NodeId,
        attempt_no: AttemptNo,
    ) -> Handoff {
        Handoff {
            schema_version: HANDOFF_SCHEMA_VERSION,
            mission_id,
            run_id,
            node_id,
            agent: "worker".to_owned(),
            objective: "read the note".to_owned(),
            summary: "the note says hello".to_owned(),
            inputs: Vec::new(),
            artifacts: vec![HandoffArtifact {
                id: ArtifactId::new(),
                path: "artifacts/report.md".to_owned(),
                sha256: "a".repeat(64),
            }],
            decisions: Vec::new(),
            assumptions: Vec::new(),
            risks: Vec::new(),
            open_questions: Vec::new(),
            acceptance: Vec::new(),
            recommended_next: Vec::new(),
            provenance: HandoffProvenance {
                attempt_no,
                base_revision: "abc123".to_owned(),
                created_at: "2026-11-23T00:00:00Z".to_owned(),
            },
        }
    }

    #[test]
    fn fresh_database_migrates_to_latest() {
        let store = Store::open_in_memory().expect("store");
        assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);

        let tables: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('missions','runs','nodes','events','snapshots','approvals')",
                [],
                |row| row.get(0),
            )
            .expect("table count");
        assert_eq!(tables, 6);
    }

    #[test]
    fn v1_database_reopens_and_preserves_data() {
        let dir = TempDir::new("reopen");
        let path = dir.path().join("hive.db");
        let mission_id;

        {
            let mut store = Store::open(&path).expect("open");
            let mission = store.create_mission("t", "g", 0).expect("mission");
            mission_id = mission.mission_id;
            store
                .append_event(&event(
                    mission.mission_id,
                    None,
                    EventKind::MissionCreated,
                    None,
                ))
                .expect("event");
        }

        {
            let store = Store::open(&path).expect("reopen");
            assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
            let aggregate = store
                .get_mission_aggregate(mission_id)
                .expect("aggregate")
                .expect("present");
            assert_eq!(aggregate.mission.mission_id, mission_id);
            assert_eq!(aggregate.last_seq, Some(1));
        }
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let dir = TempDir::new("newer");
        let path = dir.path().join("hive.db");
        {
            let conn = rusqlite::Connection::open(&path).expect("raw conn");
            conn.pragma_update(None, "user_version", 99)
                .expect("set version");
        }

        let error = Store::open(&path).expect_err("must reject newer schema");
        assert!(matches!(error, StoreError::Conflict(_)));
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let mut store = Store::open_in_memory().expect("store");
        let error = store
            .create_run(MissionId::new(), 0)
            .expect_err("missing mission must fail");
        assert!(matches!(error, StoreError::Database(_)));
    }

    #[test]
    fn journal_mode_is_wal_and_busy_timeout_is_set() {
        let dir = TempDir::new("wal");
        let path = dir.path().join("hive.db");
        let store = Store::open(&path).expect("open");
        assert_eq!(store.journal_mode().expect("mode"), "wal");

        let busy: i64 = store
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("busy timeout");
        assert_eq!(busy, BUSY_TIMEOUT_MS as i64);
    }

    #[test]
    fn duplicate_event_id_is_rejected_without_second_row() {
        let (mut store, mission, _run, _node) = setup();
        let input = event(mission.mission_id, None, EventKind::MissionCreated, None);

        store.append_event(&input).expect("first append");
        let error = store.append_event(&input).expect_err("duplicate");
        assert!(matches!(error, StoreError::DuplicateEvent(_)));
        assert_eq!(store.latest_seq().expect("latest"), Some(1));
        assert_eq!(store.read_all_events().expect("all").len(), 1);
    }

    #[test]
    fn seq_is_a_monotonic_cursor() {
        let (mut store, mission, _run, _node) = setup();
        for _ in 0..3 {
            store
                .append_event(&event(
                    mission.mission_id,
                    None,
                    EventKind::NodeThought,
                    None,
                ))
                .expect("append");
        }

        let after = store.read_events_after(1).expect("after 1");
        assert_eq!(
            after.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![2, 3]
        );
        let all = store.read_all_events().expect("all");
        assert_eq!(
            all.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn cursor_rejects_zero_and_overflow() {
        let (store, mission, _run, _node) = setup();
        assert!(matches!(
            store.read_events_after(0),
            Err(StoreError::SequenceExhausted)
        ));
        assert!(matches!(
            store.read_events_after(MAX_SEQ + 1),
            Err(StoreError::SequenceExhausted)
        ));
        assert!(store
            .read_events_after(MAX_SEQ)
            .expect("max cursor")
            .is_empty());
        assert!(matches!(
            store.read_mission_events(mission.mission_id, 0),
            Err(StoreError::SequenceExhausted)
        ));
    }

    #[test]
    fn state_update_and_event_are_atomic() {
        let (mut store, mission, _run, node) = setup();
        let ready = event(
            mission.mission_id,
            Some(node.node_id),
            EventKind::NodeReady,
            Some(AttemptNo::FIRST),
        );
        let (updated, _) = store
            .transition_node_and_emit(node.node_id, NodeState::Ready, &ready, 1)
            .expect("ready");
        assert_eq!(updated.state, NodeState::Ready);

        let duplicate = EventInput {
            event_id: ready.event_id,
            kind: EventKind::NodeStarted,
            ..ready.clone()
        };
        let error = store
            .transition_node_and_emit(node.node_id, NodeState::Leased, &duplicate, 2)
            .expect_err("duplicate event must roll back the state change");
        assert!(matches!(error, StoreError::DuplicateEvent(_)));

        let after = store
            .get_node(node.node_id)
            .expect("node")
            .expect("present");
        assert_eq!(after.state, NodeState::Ready);
    }

    #[test]
    fn mission_aggregate_contains_runs_nodes_and_cursor() {
        let (mut store, mission, run, node) = setup();
        store
            .add_node(mission.mission_id, run.run_id, 3, 0)
            .expect("second node");
        store
            .append_event(&event(
                mission.mission_id,
                Some(node.node_id),
                EventKind::NodeReady,
                Some(AttemptNo::FIRST),
            ))
            .expect("event");

        let aggregate = store
            .get_mission_aggregate(mission.mission_id)
            .expect("aggregate")
            .expect("present");
        assert_eq!(aggregate.mission.mission_id, mission.mission_id);
        assert_eq!(aggregate.runs.len(), 1);
        assert_eq!(aggregate.nodes.len(), 2);
        assert_eq!(aggregate.last_seq, Some(1));
    }

    #[test]
    fn mission_events_are_filtered() {
        let (mut store, mission, _run, _node) = setup();
        let other = store.create_mission("other", "g", 0).expect("other");
        store
            .append_event(&event(
                mission.mission_id,
                None,
                EventKind::MissionCreated,
                None,
            ))
            .expect("m1");
        store
            .append_event(&event(
                other.mission_id,
                None,
                EventKind::MissionCreated,
                None,
            ))
            .expect("m2");
        store
            .append_event(&event(
                mission.mission_id,
                None,
                EventKind::MissionStarted,
                None,
            ))
            .expect("m3");

        let events = store
            .read_mission_events(mission.mission_id, 1)
            .expect("mission events");
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![3]
        );
    }

    #[test]
    fn snapshot_round_trip_uses_last_seq_and_schema_version() {
        let (mut store, mission, run, node) = setup();
        let saved = store
            .save_snapshot(&SnapshotInput {
                mission_id: mission.mission_id,
                kind: "node.side_effects",
                last_seq: 5,
                payload: serde_json::json!({ "state": "started" }),
                run_id: Some(run.run_id),
                node_id: Some(node.node_id),
                now_ms: 10,
            })
            .expect("save");
        assert_eq!(saved.last_seq, 5);
        assert_eq!(saved.schema_version, SNAPSHOT_SCHEMA_VERSION);

        let loaded = store
            .load_snapshot(mission.mission_id, "node.side_effects", Some(node.node_id))
            .expect("load")
            .expect("present");
        assert_eq!(loaded.last_seq, 5);
        assert_eq!(loaded.schema_version, SNAPSHOT_SCHEMA_VERSION);
        assert_eq!(loaded.payload, serde_json::json!({ "state": "started" }));
    }

    #[test]
    fn snapshot_rejects_seq_above_max() {
        let (mut store, mission, _run, node) = setup();
        let error = store
            .save_snapshot(&SnapshotInput {
                mission_id: mission.mission_id,
                kind: "k",
                last_seq: MAX_SEQ + 1,
                payload: serde_json::json!({}),
                run_id: None,
                node_id: Some(node.node_id),
                now_ms: 0,
            })
            .expect_err("overflow");
        assert!(matches!(error, StoreError::SequenceExhausted));
    }

    #[test]
    fn reclaim_leased_expired_returns_to_ready() {
        let (mut store, _mission, _run, node) = setup();
        store
            .update_node_state(node.node_id, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .lease_node(node.node_id, &token, "exec", 1_000, 0)
            .expect("lease");

        let reclaimed = store.reclaim_expired_leases(2_000).expect("reclaim");
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].state, NodeState::Ready);
        assert_eq!(reclaimed[0].attempt_no.get(), 1);
        assert!(reclaimed[0].lease_token.is_none());
    }

    #[test]
    fn reclaim_running_expired_needs_recovery_and_is_idempotent() {
        let (mut store, _mission, _run, node) = setup();
        drive_to_needs_recovery(&mut store, node.node_id);

        let again = store.reclaim_expired_leases(2_000).expect("reclaim again");
        assert!(again.is_empty());

        let node = store
            .get_node(node.node_id)
            .expect("node")
            .expect("present");
        assert_eq!(node.state, NodeState::NeedsRecovery);
        assert!(node.lease_token.is_none());
    }

    #[test]
    fn lease_reclaim_writes_ready_event_in_same_transaction() {
        let (mut store, _mission, _run, node) = setup();
        store
            .update_node_state(node.node_id, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .lease_node(node.node_id, &token, "exec", 1_000, 0)
            .expect("lease");

        let reclaimed = store.reclaim_expired_leases(2_000).expect("reclaim");
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].state, NodeState::Ready);
        assert_eq!(reclaimed[0].attempt_no.get(), 1);
        assert!(reclaimed[0].lease_token.is_none());

        let events = store.read_all_events().expect("events");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == EventKind::NodeReady)
                .count(),
            1
        );

        // A second reclaim finds nothing and emits no duplicate event.
        assert!(store
            .reclaim_expired_leases(2_000)
            .expect("again")
            .is_empty());
        let events = store.read_all_events().expect("events");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == EventKind::NodeReady)
                .count(),
            1
        );
    }

    #[test]
    fn lease_reclaim_writes_needs_recovery_event() {
        let (mut store, _mission, _run, node) = setup();
        store
            .update_node_state(node.node_id, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .lease_node(node.node_id, &token, "exec", 1_000, 0)
            .expect("lease");
        store
            .update_node_state(node.node_id, NodeState::Running, 0)
            .expect("running");

        let reclaimed = store.reclaim_expired_leases(2_000).expect("reclaim");
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].state, NodeState::NeedsRecovery);
        assert!(reclaimed[0].lease_token.is_none());

        let events = store.read_all_events().expect("events");
        let recovery: Vec<_> = events
            .iter()
            .filter(|event| event.kind == EventKind::NodeNeedsRecovery)
            .collect();
        assert_eq!(recovery.len(), 1);
        assert_eq!(recovery[0].payload["reason"], "lease_expired");
        assert_eq!(recovery[0].payload["last_state"], "running");
    }

    #[test]
    fn retry_from_needs_recovery_advances_attempt() {
        let (mut store, _mission, _run, node) = setup();
        drive_to_needs_recovery(&mut store, node.node_id);

        let retried = store.retry_node(node.node_id, 3_000).expect("retry");
        assert_eq!(retried.state, NodeState::Ready);
        assert_eq!(retried.attempt_no.get(), 2);
        assert!(retried.lease_token.is_none());
    }

    #[test]
    fn abort_from_needs_recovery_finalizes() {
        let (mut store, _mission, _run, node) = setup();
        drive_to_needs_recovery(&mut store, node.node_id);

        let aborted = store.abort_node(node.node_id, 3_000).expect("abort");
        assert_eq!(aborted.state, NodeState::FailedFinal);
    }

    #[test]
    fn retry_failed_node_preserves_attempt_until_claim() {
        let (mut store, _mission, _run, node) = setup();
        store
            .update_node_state(node.node_id, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .lease_node(node.node_id, &token, "exec", 10_000, 0)
            .expect("lease");
        store
            .update_node_state(node.node_id, NodeState::Running, 0)
            .expect("running");
        store
            .update_node_state(node.node_id, NodeState::Failed, 1)
            .expect("failed");

        let retried = store.retry_failed_node(node.node_id, 2).expect("retry");
        assert_eq!(retried.state, NodeState::Ready);
        assert_eq!(
            retried.attempt_no.get(),
            1,
            "attempt must not advance when re-entering ready"
        );

        let claimed = store
            .claim_node(
                node.node_id,
                &LeaseToken::generate(),
                "exec",
                20_000,
                AttemptNo::new(2).expect("attempt 2"),
                3,
            )
            .expect("claim");
        assert_eq!(claimed.state, NodeState::Leased);
        assert_eq!(claimed.attempt_no.get(), 2, "attempt advances on claim");
    }

    #[test]
    fn renew_lease_rejects_stale_token() {
        let (mut store, _mission, _run, node) = setup();
        store
            .update_node_state(node.node_id, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .lease_node(node.node_id, &token, "exec", 1_000, 0)
            .expect("lease");

        let stale = LeaseToken::generate();
        let error = store
            .renew_lease(node.node_id, &stale, "exec", 2_000, 1)
            .expect_err("stale token");
        assert!(matches!(error, StoreError::LeaseTokenMismatch));

        let renewed = store
            .renew_lease(node.node_id, &token, "exec", 2_000, 1)
            .expect("renew");
        assert_eq!(renewed.lease_expires_at_ms, Some(2_000));
    }

    #[test]
    fn cas_accepts_current_guard_and_rejects_stale() {
        let (mut store, mission, _run, node) = setup();
        store
            .update_node_state(node.node_id, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .lease_node(node.node_id, &token, "exec", 10_000, 0)
            .expect("lease");

        let guard = LeaseGuard {
            attempt_no: node.attempt_no,
            lease_token: token.clone(),
        };
        let started = event(
            mission.mission_id,
            Some(node.node_id),
            EventKind::NodeStarted,
            Some(AttemptNo::FIRST),
        );
        let (running, _) = store
            .transition_node_cas(node.node_id, &guard, NodeState::Running, &started, 1)
            .expect("cas running");
        assert_eq!(running.state, NodeState::Running);

        let stale = LeaseGuard {
            attempt_no: node.attempt_no,
            lease_token: LeaseToken::generate(),
        };
        let succeeded = event(
            mission.mission_id,
            Some(node.node_id),
            EventKind::NodeSucceeded,
            Some(AttemptNo::FIRST),
        );
        let error = store
            .transition_node_cas(node.node_id, &stale, NodeState::Succeeded, &succeeded, 2)
            .expect_err("stale guard");
        assert!(matches!(error, StoreError::StaleExecution));

        let after = store
            .get_node(node.node_id)
            .expect("node")
            .expect("present");
        assert_eq!(after.state, NodeState::Running);
    }

    #[test]
    fn retry_budget_allows_four_attempts() {
        let budget = 3;
        for attempt in 1..=3 {
            assert!(can_retry(AttemptNo::new(attempt).expect("valid"), budget));
        }
        assert!(!can_retry(AttemptNo::new(4).expect("valid"), budget));
    }

    #[test]
    fn approval_can_be_decided_once() {
        let (mut store, mission, _run, node) = setup();
        let approval = store
            .create_approval(&approval_input(mission.mission_id, node.node_id, 10_000))
            .expect("create");
        assert_eq!(approval.state, ApprovalState::Pending);

        let decided = store
            .decide_approval(
                approval.approval_id,
                ApprovalDecision::Approved,
                "user",
                1_000,
            )
            .expect("approve");
        assert_eq!(decided.state, ApprovalState::Approved);
        assert_eq!(decided.decided_by.as_deref(), Some("user"));

        let error = store
            .decide_approval(
                approval.approval_id,
                ApprovalDecision::Rejected,
                "user",
                1_001,
            )
            .expect_err("second decision");
        assert!(matches!(error, StoreError::ApprovalConflict(_)));
    }

    #[test]
    fn expired_approval_cannot_be_approved() {
        let (mut store, mission, _run, node) = setup();
        let approval = store
            .create_approval(&approval_input(mission.mission_id, node.node_id, 1_000))
            .expect("create");

        let error = store
            .decide_approval(
                approval.approval_id,
                ApprovalDecision::Approved,
                "user",
                2_000,
            )
            .expect_err("expired");
        assert!(matches!(error, StoreError::ApprovalConflict(_)));

        let after = store
            .get_approval(approval.approval_id)
            .expect("get")
            .expect("present");
        assert_eq!(after.state, ApprovalState::Expired);
    }

    #[test]
    fn expire_overdue_approvals_is_idempotent() {
        let (mut store, mission, _run, node) = setup();
        store
            .create_approval(&approval_input(mission.mission_id, node.node_id, 1_000))
            .expect("create");

        let first = store.expire_overdue_approvals(2_000).expect("expire");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].state, ApprovalState::Expired);

        let second = store.expire_overdue_approvals(2_000).expect("expire again");
        assert!(second.is_empty());
    }

    #[test]
    fn list_pending_approvals_excludes_overdue() {
        let (mut store, mission, _run, node) = setup();
        let active = store
            .create_approval(&approval_input(mission.mission_id, node.node_id, 10_000))
            .expect("active");
        store
            .create_approval(&approval_input(mission.mission_id, node.node_id, 500))
            .expect("overdue");

        let pending = store.list_pending_approvals(1_000).expect("list");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].approval_id, active.approval_id);
    }

    #[test]
    fn request_approval_transitions_node_atomically() {
        let (mut store, mission, _run, node) = setup();
        store
            .update_node_state(node.node_id, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .lease_node(node.node_id, &token, "exec", 10_000, 0)
            .expect("lease");
        store
            .update_node_state(node.node_id, NodeState::Running, 0)
            .expect("running");

        let approval = approval_input(mission.mission_id, node.node_id, 10_000);
        let request_event = event(
            mission.mission_id,
            Some(node.node_id),
            EventKind::NodeApprovalRequested,
            Some(AttemptNo::FIRST),
        );
        let (updated, stored, envelope) = store
            .transition_node_and_request_approval(node.node_id, &approval, &request_event, 1)
            .expect("request");
        assert_eq!(updated.state, NodeState::WaitingApproval);
        assert_eq!(stored.state, ApprovalState::Pending);
        assert_eq!(envelope.kind, EventKind::NodeApprovalRequested);
    }

    #[test]
    fn fresh_database_has_v6_tables() {
        let store = Store::open_in_memory().expect("store");
        assert_eq!(store.schema_version().expect("version"), 6);
        let tables: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('tool_calls','artifacts','tool_audit','agent_sessions','turns','turn_usage','handoffs','budgets','routines','routine_fires')",
                [],
                |row| row.get(0),
            )
            .expect("table count");
        assert_eq!(tables, 10);
    }

    #[test]
    fn v1_database_migrates_to_v6_and_preserves_rows() {
        let dir = TempDir::new("v1-to-v6");
        let path = dir.path().join("hive.db");
        let mission_id = MissionId::new();

        {
            let conn = rusqlite::Connection::open(&path).expect("raw conn");
            conn.execute_batch(V1_SCHEMA).expect("v1 schema");
            conn.execute(
                "INSERT INTO missions (mission_id, state, title, goal, created_at_ms, updated_at_ms) VALUES (?1, 'queued', 'old', 'goal', 1, 1)",
                rusqlite::params![mission_id.to_string()],
            )
            .expect("old mission");
            conn.pragma_update(None, "user_version", 1).expect("set v1");
        }

        let store = Store::open(&path).expect("migrate");
        assert_eq!(store.schema_version().expect("version"), 6);
        let mission = store
            .get_mission(mission_id)
            .expect("mission")
            .expect("preserved");
        assert_eq!(mission.title, "old");
        let tables: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('tool_calls','artifacts','tool_audit','agent_sessions','turns','turn_usage','handoffs','budgets','routines','routine_fires')",
                [],
                |row| row.get(0),
            )
            .expect("table count");
        assert_eq!(tables, 10);
    }

    #[test]
    fn v2_database_migrates_to_v6_and_preserves_rows() {
        use crate::schema::V2_SCHEMA;

        let dir = TempDir::new("v2-to-v6");
        let path = dir.path().join("hive.db");
        let mission_id = MissionId::new();
        let run_id = RunId::new();
        let node_id = NodeId::new();
        let tool_call_id = ToolCallId::new();

        {
            let conn = rusqlite::Connection::open(&path).expect("raw conn");
            conn.execute_batch(V1_SCHEMA).expect("v1 schema");
            conn.execute_batch(V2_SCHEMA).expect("v2 schema");
            conn.execute(
                "INSERT INTO missions (mission_id, state, title, goal, created_at_ms, updated_at_ms) VALUES (?1, 'running', 'kept', 'goal', 1, 1)",
                rusqlite::params![mission_id.to_string()],
            )
            .expect("mission");
            conn.execute(
                "INSERT INTO runs (run_id, mission_id, state, created_at_ms, updated_at_ms) VALUES (?1, ?2, 'running', 1, 1)",
                rusqlite::params![run_id.to_string(), mission_id.to_string()],
            )
            .expect("run");
            conn.execute(
                "INSERT INTO nodes (node_id, mission_id, run_id, state, attempt_no, retry_budget, lease_token, lease_owner, lease_expires_at_ms, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, 'ready', 1, 3, NULL, NULL, NULL, 1, 1)",
                rusqlite::params![node_id.to_string(), mission_id.to_string(), run_id.to_string()],
            )
            .expect("node");
            conn.execute(
                "INSERT INTO tool_calls (tool_call_id, mission_id, run_id, node_id, session_id, tool_name, tool_version, request_hash, attempt_no, lease_token, state, summary, truncated, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, NULL, 'fs.read', 1, 'hash', 1, 'lease', 'completed', 'kept tool call', 0, 1, 1)",
                rusqlite::params![
                    tool_call_id.to_string(),
                    mission_id.to_string(),
                    run_id.to_string(),
                    node_id.to_string()
                ],
            )
            .expect("tool call");
            conn.pragma_update(None, "user_version", 2).expect("set v2");
        }

        let store = Store::open(&path).expect("migrate");
        assert_eq!(store.schema_version().expect("version"), 6);
        assert_eq!(
            store
                .get_mission(mission_id)
                .expect("mission")
                .expect("preserved")
                .title,
            "kept"
        );
        let tool_calls: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM tool_calls", [], |row| row.get(0))
            .expect("tool calls");
        assert_eq!(tool_calls, 1);
        let new_tables: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('agent_sessions','turns','turn_usage')",
                [],
                |row| row.get(0),
            )
            .expect("new tables");
        assert_eq!(new_tables, 3);
    }

    #[test]
    fn v3_database_migrates_to_v6_and_preserves_rows() {
        use crate::schema::{V2_SCHEMA, V3_SCHEMA};

        let dir = TempDir::new("v3-to-v6");
        let path = dir.path().join("hive.db");
        let mission_id = MissionId::new();
        let run_id = RunId::new();
        let node_id = NodeId::new();
        let session_id = AgentSessionId::new().to_string();

        {
            let conn = rusqlite::Connection::open(&path).expect("raw conn");
            conn.execute_batch(V1_SCHEMA).expect("v1");
            conn.execute_batch(V2_SCHEMA).expect("v2");
            conn.execute_batch(V3_SCHEMA).expect("v3");
            conn.execute(
                "INSERT INTO missions (mission_id, state, title, goal, created_at_ms, updated_at_ms) VALUES (?1, 'running', 'kept', 'goal', 1, 1)",
                rusqlite::params![mission_id.to_string()],
            )
            .expect("mission");
            conn.execute(
                "INSERT INTO runs (run_id, mission_id, state, created_at_ms, updated_at_ms) VALUES (?1, ?2, 'running', 1, 1)",
                rusqlite::params![run_id.to_string(), mission_id.to_string()],
            )
            .expect("run");
            conn.execute(
                "INSERT INTO nodes (node_id, mission_id, run_id, state, attempt_no, retry_budget, lease_token, lease_owner, lease_expires_at_ms, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, 'running', 1, 3, NULL, NULL, NULL, 1, 1)",
                rusqlite::params![node_id.to_string(), mission_id.to_string(), run_id.to_string()],
            )
            .expect("node");
            conn.execute(
                "INSERT INTO agent_sessions (session_id, mission_id, run_id, node_id, role, model, system_prompt, state, created_at_ms, updated_at_ms, closed_at_ms) VALUES (?1, ?2, ?3, ?4, 'worker', 'auto/auto', '', 'active', 1, 1, NULL)",
                rusqlite::params![
                    session_id,
                    mission_id.to_string(),
                    run_id.to_string(),
                    node_id.to_string()
                ],
            )
            .expect("session");
            conn.pragma_update(None, "user_version", 3).expect("set v3");
        }

        let store = Store::open(&path).expect("migrate");
        assert_eq!(store.schema_version().expect("version"), 6);
        assert_eq!(
            store
                .get_mission(mission_id)
                .expect("mission")
                .expect("preserved")
                .title,
            "kept"
        );
        let sessions: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM agent_sessions", [], |row| row.get(0))
            .expect("sessions");
        assert_eq!(sessions, 1);
        let handoffs: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM handoffs", [], |row| row.get(0))
            .expect("handoffs");
        assert_eq!(handoffs, 0);
    }

    #[test]
    fn v4_database_migrates_to_v6_and_preserves_rows() {
        use crate::schema::{V2_SCHEMA, V3_SCHEMA, V4_SCHEMA};

        let dir = TempDir::new("v4-to-v6");
        let path = dir.path().join("hive.db");
        let mission_id = MissionId::new();
        let run_id = RunId::new();
        let node_id = NodeId::new();

        {
            let conn = rusqlite::Connection::open(&path).expect("raw conn");
            conn.execute_batch(V1_SCHEMA).expect("v1");
            conn.execute_batch(V2_SCHEMA).expect("v2");
            conn.execute_batch(V3_SCHEMA).expect("v3");
            conn.execute_batch(V4_SCHEMA).expect("v4");
            conn.execute(
                "INSERT INTO missions (mission_id, state, title, goal, created_at_ms, updated_at_ms) VALUES (?1, 'running', 'kept', 'goal', 1, 1)",
                rusqlite::params![mission_id.to_string()],
            )
            .expect("mission");
            conn.execute(
                "INSERT INTO runs (run_id, mission_id, state, created_at_ms, updated_at_ms) VALUES (?1, ?2, 'running', 1, 1)",
                rusqlite::params![run_id.to_string(), mission_id.to_string()],
            )
            .expect("run");
            conn.execute(
                "INSERT INTO nodes (node_id, mission_id, run_id, state, attempt_no, retry_budget, lease_token, lease_owner, lease_expires_at_ms, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, 'running', 1, 3, NULL, NULL, NULL, 1, 1)",
                rusqlite::params![node_id.to_string(), mission_id.to_string(), run_id.to_string()],
            )
            .expect("node");
            conn.pragma_update(None, "user_version", 4).expect("set v4");
        }

        let store = Store::open(&path).expect("migrate");
        assert_eq!(store.schema_version().expect("version"), 6);
        assert_eq!(
            store
                .get_mission(mission_id)
                .expect("mission")
                .expect("preserved")
                .title,
            "kept"
        );
        let budgets: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM budgets", [], |row| row.get(0))
            .expect("budgets");
        assert_eq!(budgets, 0);
    }

    #[test]
    fn handoff_round_trips_and_emits_event() {
        let (mut store, mission, run, node) = setup();
        let handoff = handoff_input(
            mission.mission_id,
            run.run_id,
            node.node_id,
            AttemptNo::FIRST,
        );
        let record = store.insert_handoff(&handoff).expect("insert");
        assert_eq!(record.node_id, node.node_id);
        assert_eq!(record.attempt_no, AttemptNo::FIRST);
        assert_eq!(record.summary, "the note says hello");

        let fetched = store
            .get_handoff(node.node_id, AttemptNo::FIRST)
            .expect("get")
            .expect("present");
        assert_eq!(fetched.handoff_id, record.handoff_id);
        assert_eq!(fetched.mission_id, mission.mission_id);
        assert_eq!(fetched.run_id, run.run_id);
        assert_eq!(fetched.agent, "worker");
        assert_eq!(fetched.schema_version, HANDOFF_SCHEMA_VERSION);
        assert_eq!(fetched.objective, "read the note");
        assert_eq!(fetched.base_revision, "abc123");
        assert_eq!(fetched.artifacts.len(), 1);
        assert_eq!(fetched.artifacts[0].path, "artifacts/report.md");

        let listed = store.list_handoffs(mission.mission_id).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].handoff_id, record.handoff_id);

        let events = store.read_all_events().expect("events");
        let created = events
            .iter()
            .find(|event| event.kind == EventKind::HandoffCreated)
            .expect("handoff.created");
        assert_eq!(created.payload["handoff_id"], record.handoff_id.to_string());
        assert_eq!(created.payload["schema_version"], HANDOFF_SCHEMA_VERSION);
    }

    #[test]
    fn duplicate_handoff_node_attempt_is_rejected() {
        let (mut store, mission, run, node) = setup();
        let handoff = handoff_input(
            mission.mission_id,
            run.run_id,
            node.node_id,
            AttemptNo::FIRST,
        );
        store.insert_handoff(&handoff).expect("first insert");
        let error = store.insert_handoff(&handoff).expect_err("duplicate");
        assert!(matches!(error, StoreError::Conflict(_)));
    }

    #[test]
    fn insert_handoff_rejects_invalid_handoff() {
        let (mut store, mission, run, node) = setup();
        let mut handoff = handoff_input(
            mission.mission_id,
            run.run_id,
            node.node_id,
            AttemptNo::FIRST,
        );
        handoff.summary = String::new();
        let error = store.insert_handoff(&handoff).expect_err("invalid");
        assert!(matches!(error, StoreError::State(_)));
    }

    #[test]
    fn agent_session_turn_and_usage_round_trip() {
        let (mut store, mission, run, node) = setup();
        let session = store
            .create_agent_session(&crate::records::NewAgentSession {
                mission_id: mission.mission_id,
                run_id: Some(run.run_id),
                node_id: node.node_id,
                role: "worker",
                model: "auto/auto",
                system_prompt: "be brief",
                now_ms: 10,
            })
            .expect("session");
        assert_eq!(session.state, AgentSessionState::Active);
        assert_eq!(
            store
                .get_agent_session(session.session_id)
                .expect("get")
                .expect("present")
                .role,
            "worker"
        );

        let turn = store
            .start_turn(
                session.session_id,
                node.node_id,
                AttemptNo::FIRST,
                1,
                "auto/auto",
                11,
            )
            .expect("turn");
        assert_eq!(turn.state, TurnState::Started);
        let finished = store
            .finish_turn(
                turn.turn_id,
                TurnState::Completed,
                Some("stop"),
                "done",
                0,
                12,
            )
            .expect("finish");
        assert_eq!(finished.state, TurnState::Completed);
        assert_eq!(finished.finish_reason.as_deref(), Some("stop"));

        let usage = store
            .record_turn_usage(
                turn.turn_id,
                &NewTurnUsage {
                    prompt_tokens: 7,
                    completion_tokens: 3,
                    total_tokens: 10,
                    source: UsageSource::Authoritative,
                    request_id: Some("chatcmpl-1".to_owned()),
                },
                13,
            )
            .expect("usage");
        assert_eq!(usage.total_tokens, 10);
        assert_eq!(
            store
                .get_turn_usage(turn.turn_id)
                .expect("get usage")
                .expect("present")
                .source,
            UsageSource::Authoritative
        );
        assert_eq!(
            store.list_turns(session.session_id).expect("turns").len(),
            1
        );

        let closed = store
            .set_agent_session_state(session.session_id, AgentSessionState::Completed, 14)
            .expect("close");
        assert_eq!(closed.closed_at_ms, Some(14));
    }

    #[test]
    fn approval_can_be_consumed_only_once() {
        let (mut store, mission, _run, node) = setup();
        let input = approval_input(mission.mission_id, node.node_id, 10_000);
        let tool_call_id = input.tool_call_id;
        let request_hash = input.request_hash.clone();
        let attempt_no = input.attempt_no;
        let lease_token = input.lease_token.clone();
        let approval = store.create_approval(&input).expect("create");
        store
            .decide_approval(
                approval.approval_id,
                ApprovalDecision::Approved,
                "user",
                1_000,
            )
            .expect("approve");

        let consumed = store
            .consume_approval(
                approval.approval_id,
                tool_call_id,
                &request_hash,
                attempt_no,
                &lease_token,
                2_000,
            )
            .expect("consume");
        assert_eq!(consumed.consumed_at_ms, Some(2_000));

        let error = store
            .consume_approval(
                approval.approval_id,
                tool_call_id,
                &request_hash,
                attempt_no,
                &lease_token,
                2_001,
            )
            .expect_err("replay");
        assert!(matches!(error, StoreError::ApprovalReplayed(_)));
        assert_eq!(error.code(), "approval.replayed");
    }

    #[test]
    fn approval_consumption_rejects_hash_and_lease_mismatch() {
        let (mut store, mission, _run, node) = setup();
        let input = approval_input(mission.mission_id, node.node_id, 10_000);
        let tool_call_id = input.tool_call_id;
        let attempt_no = input.attempt_no;
        let lease_token = input.lease_token.clone();
        let approval = store.create_approval(&input).expect("create");
        store
            .decide_approval(
                approval.approval_id,
                ApprovalDecision::Approved,
                "user",
                1_000,
            )
            .expect("approve");

        let error = store
            .consume_approval(
                approval.approval_id,
                tool_call_id,
                "different-hash",
                attempt_no,
                &lease_token,
                2_000,
            )
            .expect_err("hash mismatch");
        assert!(matches!(error, StoreError::StaleExecution));
        assert_eq!(error.code(), "tool.stale_execution");

        let stale_lease = LeaseToken::generate();
        let error = store
            .consume_approval(
                approval.approval_id,
                tool_call_id,
                &input.request_hash,
                attempt_no,
                &stale_lease,
                2_000,
            )
            .expect_err("lease mismatch");
        assert!(matches!(error, StoreError::StaleExecution));

        store
            .consume_approval(
                approval.approval_id,
                tool_call_id,
                &input.request_hash,
                attempt_no,
                &lease_token,
                2_000,
            )
            .expect("original request still consumable");
    }

    #[test]
    fn tool_audit_chain_detects_tampering() {
        let (mut store, mission, _run, _node) = setup();
        let tool_call_id = ToolCallId::new();
        store
            .append_tool_audit(
                mission.mission_id,
                tool_call_id,
                "node.tool_call",
                serde_json::json!({ "tool": "fs.read", "path": "a.txt" }),
                1,
            )
            .expect("first audit");
        store
            .append_tool_audit(
                mission.mission_id,
                tool_call_id,
                "node.tool_result",
                serde_json::json!({ "tool": "fs.read", "summary": "read 1 byte" }),
                2,
            )
            .expect("second audit");
        assert!(store.verify_tool_audit_chain().expect("verify"));

        store
            .conn
            .execute(
                "UPDATE tool_audit SET record_json = ?1 WHERE audit_id = 1",
                rusqlite::params![r#"{"tool":"fs.read","path":"tampered"}"#],
            )
            .expect("tamper");
        assert!(!store.verify_tool_audit_chain().expect("verify tampered"));
    }

    #[test]
    fn archive_expired_mission_preserves_latest_seq_and_active_events() {
        let (mut store, mission, _run, node) = setup();
        let active_mission = store.create_mission("active", "g", 0).expect("active");
        let active_run = store
            .create_run(active_mission.mission_id, 0)
            .expect("active run");
        let active_node = store
            .add_node(active_mission.mission_id, active_run.run_id, 3, 0)
            .expect("active node");
        let active_event = event(
            active_mission.mission_id,
            None,
            EventKind::MissionCreated,
            None,
        );
        store.append_event(&active_event).expect("active event");
        let mission_event = event(mission.mission_id, None, EventKind::MissionDone, None);
        let archived_event = store.append_event(&mission_event).expect("mission event");
        assert_eq!(archived_event.seq, 2);
        let pre_archive_latest = store.latest_seq().expect("latest");

        store
            .conn
            .execute(
                "UPDATE missions SET state = 'done', updated_at_ms = 0 WHERE mission_id = ?1",
                rusqlite::params![mission.mission_id.to_string()],
            )
            .expect("terminal");
        let dir = TempDir::new("archive");
        let reports = store
            .archive_expired_missions(dir.path(), 31 * 24 * 60 * 60 * 1000)
            .expect("archive");

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].mission_id, mission.mission_id);
        assert_eq!(reports[0].events_archived, 1);
        let post_archive_latest = store.latest_seq().expect("latest after");
        assert_eq!(post_archive_latest, Some(2));
        assert_eq!(pre_archive_latest, Some(2));
        assert_eq!(reports[0].retained_since_seq, 3);
        let active_events = store
            .read_mission_events(active_mission.mission_id, 1)
            .expect("active events");
        assert_eq!(active_events.len(), 0);
        assert!(store
            .read_mission_events(mission.mission_id, 1)
            .expect("archived events")
            .is_empty());
        assert!(store.get_node(node.node_id).expect("node").is_some());
        assert!(store
            .get_node(active_node.node_id)
            .expect("active node")
            .is_some());
    }

    #[test]
    fn archive_retention_boundary_excludes_exactly_expired() {
        let (mut store, mission, _run, _node) = setup();
        let exactly_old = 31 * 24 * 60 * 60 * 1000;
        let threshold = exactly_old - DEFAULT_RETENTION_DAYS * 24 * 60 * 60 * 1000;
        store
            .conn
            .execute(
                "UPDATE missions SET state = 'done', updated_at_ms = ?1 WHERE mission_id = ?2",
                rusqlite::params![threshold, mission.mission_id.to_string()],
            )
            .expect("terminal");
        let dir = TempDir::new("archive-boundary");
        let reports = store
            .archive_expired_missions(dir.path(), exactly_old)
            .expect("archive");
        assert!(reports.is_empty());
    }

    #[test]
    fn archived_mission_audit_chain_verifies_by_segment() {
        let mut store = Store::open_in_memory().expect("store");
        let mission = store.create_mission("m", "g", 0).expect("mission");
        let run = store.create_run(mission.mission_id, 0).expect("run");
        let _node = store
            .add_node(mission.mission_id, run.run_id, 0, 0)
            .expect("node");
        let audit = ToolCallId::new();
        store
            .append_tool_audit(
                mission.mission_id,
                audit,
                "node.tool_call",
                serde_json::json!({ "tool": "fs.read" }),
                0,
            )
            .expect("audit");

        store
            .conn
            .execute(
                "UPDATE missions SET state = 'done', updated_at_ms = 0 WHERE mission_id = ?1",
                rusqlite::params![mission.mission_id.to_string()],
            )
            .expect("terminal");
        let dir = TempDir::new("archive-audit");
        let report = store
            .archive_mission(dir.path(), mission.mission_id, 31 * 24 * 60 * 60 * 1000)
            .expect("archive");
        assert!(store
            .verify_archived_mission_audit(&report.archive_path, mission.mission_id)
            .expect("verify archive"));
    }

    #[test]
    fn retention_floor_survives_reopen() {
        let dir = TempDir::new("retention-reopen");
        let db = dir.path().join("hive.db");
        let mut store = Store::open(&db).expect("open");
        assert_eq!(store.retained_since_seq().expect("floor"), 0);

        let mission = store.create_mission("m", "g", 0).expect("mission");
        store
            .append_event(&event(
                mission.mission_id,
                None,
                EventKind::MissionCreated,
                None,
            ))
            .expect("created event");
        store
            .append_event(&event(
                mission.mission_id,
                None,
                EventKind::MissionCancelled,
                None,
            ))
            .expect("cancelled event");
        store
            .transition_mission(mission.mission_id, MissionState::Cancelled, 0)
            .expect("terminal");

        let report = store
            .archive_mission(&dir.path().join("archive"), mission.mission_id, 1)
            .expect("archive");
        assert_eq!(report.retained_since_seq, 3);
        let sidecar = std::fs::read_to_string(dir.path().join("retention.json")).expect("sidecar");
        assert!(sidecar.contains("\"retained_since_seq\":3"));
        drop(store);

        let reopened = Store::open(&db).expect("reopen");
        assert_eq!(reopened.retained_since_seq().expect("floor"), 3);
        assert!(reopened
            .read_all_events()
            .expect("events after reopen")
            .is_empty());
    }

    #[test]
    fn corrupt_retention_sidecar_refuses_to_open() {
        let dir = TempDir::new("retention-corrupt");
        let db = dir.path().join("hive.db");
        let sidecar = dir.path().join("retention.json");

        std::fs::write(&sidecar, "{not json").expect("write corrupt sidecar");
        let error = Store::open(&db).expect_err("corrupt sidecar must fail");
        let message = error.to_string();
        assert!(message.contains("retention.json"));
        assert!(message.contains("retention error"));

        std::fs::write(
            &sidecar,
            format!("{{\"retained_since_seq\":{}}}", MAX_SEQ + 1),
        )
        .expect("write out-of-range sidecar");
        let error = Store::open(&db).expect_err("out-of-range sidecar must fail");
        assert!(error.to_string().contains("exceeds maximum"));
    }

    #[test]
    fn retention_persist_failure_keeps_rows_and_floor() {
        let dir = TempDir::new("retention-blocked");
        let db = dir.path().join("hive.db");
        let mut store = Store::open(&db).expect("open");
        let mission = store.create_mission("m", "g", 0).expect("mission");
        store
            .append_event(&event(
                mission.mission_id,
                None,
                EventKind::MissionCancelled,
                None,
            ))
            .expect("event");
        store
            .transition_mission(mission.mission_id, MissionState::Cancelled, 0)
            .expect("terminal");

        let sidecar = dir.path().join("retention.json");
        std::fs::create_dir(&sidecar).expect("block sidecar path");
        let error = store
            .archive_mission(&dir.path().join("archive"), mission.mission_id, 1)
            .expect_err("persist failure");
        assert!(matches!(error, StoreError::Retention(_)));
        assert_eq!(store.retained_since_seq().expect("floor"), 0);
        assert_eq!(
            store
                .read_all_events()
                .expect("events")
                .iter()
                .filter(|envelope| envelope.mission_id == mission.mission_id)
                .count(),
            1
        );
        assert!(store.latest_seq().expect("latest").is_some());

        std::fs::remove_dir(&sidecar).expect("unblock sidecar path");
        let report = store
            .archive_mission(&dir.path().join("archive"), mission.mission_id, 1)
            .expect("retry archive");
        assert_eq!(report.retained_since_seq, 2);
        assert!(store
            .read_all_events()
            .expect("events")
            .iter()
            .all(|envelope| envelope.mission_id != mission.mission_id));
    }

    #[test]
    fn archive_expired_missions_is_idempotent_and_spares_active() {
        let (mut store, expired, _run, _node) = setup();
        store
            .append_event(&event(
                expired.mission_id,
                None,
                EventKind::MissionCancelled,
                None,
            ))
            .expect("expired event");
        store
            .transition_mission(expired.mission_id, MissionState::Cancelled, 0)
            .expect("terminal");

        let active = store.create_mission("active", "g", 0).expect("active");
        store
            .append_event(&event(
                active.mission_id,
                None,
                EventKind::MissionCreated,
                None,
            ))
            .expect("active event");

        let dir = TempDir::new("archive-idempotent");
        let now_ms = 31 * 24 * 60 * 60 * 1000;
        let first = store
            .archive_expired_missions_with_days(dir.path(), now_ms, 30)
            .expect("first archive");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].mission_id, expired.mission_id);
        assert_eq!(
            store
                .read_all_events()
                .expect("active events")
                .iter()
                .filter(|envelope| envelope.mission_id == active.mission_id)
                .count(),
            1
        );

        let second = store
            .archive_expired_missions_with_days(dir.path(), now_ms, 30)
            .expect("second archive");
        assert!(second.is_empty());
        assert_eq!(
            store
                .read_all_events()
                .expect("active events")
                .iter()
                .filter(|envelope| envelope.mission_id == active.mission_id)
                .count(),
            1
        );
    }

    #[test]
    fn budget_request_id_is_idempotent() {
        let (mut store, mission, run, node) = setup();
        store
            .update_node_state(node.node_id, NodeState::Ready, 0)
            .expect("ready");
        let session = store
            .create_agent_session(&NewAgentSession {
                mission_id: mission.mission_id,
                run_id: Some(run.run_id),
                node_id: node.node_id,
                role: "worker",
                model: "auto/auto",
                system_prompt: "",
                now_ms: 0,
            })
            .expect("session");
        store
            .set_budget(
                "node",
                &node.node_id.to_string(),
                BudgetDimension::Tokens,
                None,
                BudgetLimits {
                    hard_limit: Some(100),
                    soft_limit: None,
                },
                0,
            )
            .expect("budget");
        let usage = NewTurnUsage {
            prompt_tokens: 5,
            completion_tokens: 5,
            total_tokens: 10,
            source: UsageSource::Authoritative,
            request_id: Some("req-1".to_owned()),
        };
        let turn1 = store
            .start_turn(
                session.session_id,
                node.node_id,
                AttemptNo::FIRST,
                1,
                "auto/auto",
                0,
            )
            .expect("turn1");
        store
            .record_turn_usage(turn1.turn_id, &usage, 0)
            .expect("usage1");
        let turn2 = store
            .start_turn(
                session.session_id,
                node.node_id,
                AttemptNo::FIRST,
                2,
                "auto/auto",
                0,
            )
            .expect("turn2");
        store
            .record_turn_usage(turn2.turn_id, &usage, 0)
            .expect("usage2");
        let budget = store
            .get_budget("node", &node.node_id.to_string(), BudgetDimension::Tokens)
            .expect("get")
            .expect("present");
        assert_eq!(budget.consumed, 10);
    }

    #[test]
    fn budget_deduct_rejects_over_limit_without_modifying_row() {
        let (mut store, mission, run, node) = setup();
        store
            .update_node_state(node.node_id, NodeState::Ready, 0)
            .expect("ready");
        let session = store
            .create_agent_session(&NewAgentSession {
                mission_id: mission.mission_id,
                run_id: Some(run.run_id),
                node_id: node.node_id,
                role: "worker",
                model: "auto/auto",
                system_prompt: "",
                now_ms: 0,
            })
            .expect("session");
        store
            .set_budget(
                "node",
                &node.node_id.to_string(),
                BudgetDimension::Tokens,
                None,
                BudgetLimits {
                    hard_limit: Some(15),
                    soft_limit: None,
                },
                0,
            )
            .expect("budget");
        let first = NewTurnUsage {
            prompt_tokens: 5,
            completion_tokens: 5,
            total_tokens: 10,
            source: UsageSource::Authoritative,
            request_id: Some("req-a".to_owned()),
        };
        let second = NewTurnUsage {
            prompt_tokens: 5,
            completion_tokens: 5,
            total_tokens: 10,
            source: UsageSource::Authoritative,
            request_id: Some("req-b".to_owned()),
        };
        let turn1 = store
            .start_turn(
                session.session_id,
                node.node_id,
                AttemptNo::FIRST,
                1,
                "auto/auto",
                0,
            )
            .expect("turn1");
        store
            .record_turn_usage(turn1.turn_id, &first, 0)
            .expect("first usage");
        let turn2 = store
            .start_turn(
                session.session_id,
                node.node_id,
                AttemptNo::FIRST,
                2,
                "auto/auto",
                0,
            )
            .expect("turn2");
        let error = store
            .record_turn_usage(turn2.turn_id, &second, 0)
            .expect_err("over limit");
        assert!(matches!(
            error,
            StoreError::BudgetExceeded { ref dimension } if dimension == "tokens"
        ));
        let budget = store
            .get_budget("node", &node.node_id.to_string(), BudgetDimension::Tokens)
            .expect("get")
            .expect("present");
        assert_eq!(budget.consumed, 10);
    }

    #[test]
    fn rebuild_budget_counters_is_idempotent() {
        let (mut store, mission, run, node) = setup();
        store
            .update_node_state(node.node_id, NodeState::Ready, 0)
            .expect("ready");
        store
            .set_budget(
                "node",
                &node.node_id.to_string(),
                BudgetDimension::Tokens,
                None,
                BudgetLimits {
                    hard_limit: Some(1000),
                    soft_limit: None,
                },
                0,
            )
            .expect("tokens");
        store
            .set_budget(
                "node",
                &node.node_id.to_string(),
                BudgetDimension::Requests,
                None,
                BudgetLimits {
                    hard_limit: Some(1000),
                    soft_limit: None,
                },
                0,
            )
            .expect("requests");
        store
            .set_budget(
                "node",
                &node.node_id.to_string(),
                BudgetDimension::ToolCalls,
                None,
                BudgetLimits {
                    hard_limit: Some(1000),
                    soft_limit: None,
                },
                0,
            )
            .expect("tool calls");
        let session = store
            .create_agent_session(&NewAgentSession {
                mission_id: mission.mission_id,
                run_id: Some(run.run_id),
                node_id: node.node_id,
                role: "worker",
                model: "auto/auto",
                system_prompt: "",
                now_ms: 0,
            })
            .expect("session");
        let turn1 = store
            .start_turn(
                session.session_id,
                node.node_id,
                AttemptNo::FIRST,
                1,
                "auto/auto",
                0,
            )
            .expect("turn1");
        store
            .record_turn_usage(
                turn1.turn_id,
                &NewTurnUsage {
                    prompt_tokens: 3,
                    completion_tokens: 4,
                    total_tokens: 7,
                    source: UsageSource::Authoritative,
                    request_id: Some("r1".to_owned()),
                },
                0,
            )
            .expect("usage1");
        let turn2 = store
            .start_turn(
                session.session_id,
                node.node_id,
                AttemptNo::FIRST,
                2,
                "auto/auto",
                0,
            )
            .expect("turn2");
        store
            .record_turn_usage(
                turn2.turn_id,
                &NewTurnUsage {
                    prompt_tokens: 5,
                    completion_tokens: 6,
                    total_tokens: 11,
                    source: UsageSource::Authoritative,
                    request_id: Some("r2".to_owned()),
                },
                0,
            )
            .expect("usage2");

        let token = LeaseToken::generate();
        store
            .lease_node(node.node_id, &token, "exec", 1000, 0)
            .expect("lease");
        let leased = store.get_node(node.node_id).expect("get").expect("node");
        let guard = LeaseGuard {
            attempt_no: leased.attempt_no,
            lease_token: token,
        };
        let tool = EventInput {
            event_id: EventId::new(),
            kind: EventKind::NodeToolCall,
            mission_id: mission.mission_id,
            run_id: Some(run.run_id),
            node_id: Some(node.node_id),
            session_id: Some(session.session_id),
            attempt_no: Some(AttemptNo::FIRST),
            trace_id: None,
            payload: serde_json::json!({
                "tool_call_id": "tc_fact",
                "tool_name": "fs.read",
                "tool_version": 1,
                "request_hash": "hash",
            }),
        };
        store
            .append_tool_event(node.node_id, &guard, &tool, 0)
            .expect("tool");

        store.rebuild_budget_counters(1).expect("rebuild 1");
        store.rebuild_budget_counters(2).expect("rebuild 2");

        assert_eq!(
            store
                .get_budget("node", &node.node_id.to_string(), BudgetDimension::Tokens)
                .expect("get")
                .expect("present")
                .consumed,
            18
        );
        assert_eq!(
            store
                .get_budget("node", &node.node_id.to_string(), BudgetDimension::Requests)
                .expect("get")
                .expect("present")
                .consumed,
            2
        );
        assert_eq!(
            store
                .get_budget(
                    "node",
                    &node.node_id.to_string(),
                    BudgetDimension::ToolCalls
                )
                .expect("get")
                .expect("present")
                .consumed,
            1
        );
    }

    #[test]
    fn rebuild_keeps_budget_exceeded_node_failed_final() {
        let (mut store, mission, run, node) = setup();
        store
            .update_node_state(node.node_id, NodeState::Ready, 0)
            .expect("ready");
        let session = store
            .create_agent_session(&NewAgentSession {
                mission_id: mission.mission_id,
                run_id: Some(run.run_id),
                node_id: node.node_id,
                role: "worker",
                model: "auto/auto",
                system_prompt: "",
                now_ms: 0,
            })
            .expect("session");
        let turn = store
            .start_turn(
                session.session_id,
                node.node_id,
                AttemptNo::FIRST,
                1,
                "auto/auto",
                0,
            )
            .expect("turn");
        store
            .record_turn_usage(
                turn.turn_id,
                &NewTurnUsage {
                    prompt_tokens: 5,
                    completion_tokens: 5,
                    total_tokens: 10,
                    source: UsageSource::Authoritative,
                    request_id: Some("req-final".to_owned()),
                },
                0,
            )
            .expect("usage");
        store
            .set_budget(
                "node",
                &node.node_id.to_string(),
                BudgetDimension::Tokens,
                None,
                BudgetLimits {
                    hard_limit: Some(100),
                    soft_limit: None,
                },
                0,
            )
            .expect("budget");
        for next in [
            NodeState::Leased,
            NodeState::Running,
            NodeState::Failed,
            NodeState::FailedFinal,
        ] {
            store
                .update_node_state(node.node_id, next, 0)
                .expect("advance");
        }
        store.rebuild_budget_counters(1).expect("rebuild");
        assert_eq!(
            store
                .get_node(node.node_id)
                .expect("get")
                .expect("present")
                .state,
            NodeState::FailedFinal
        );
    }

    #[test]
    fn budget_summary_omits_sensitive_fields() {
        let (mut store, _mission, _run, node) = setup();
        store
            .set_budget(
                "node",
                &node.node_id.to_string(),
                BudgetDimension::Tokens,
                None,
                BudgetLimits {
                    hard_limit: Some(100),
                    soft_limit: None,
                },
                0,
            )
            .expect("budget");
        let summaries = store
            .node_budget_summaries(node.node_id)
            .expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].dimension, "tokens");
        assert_eq!(summaries[0].consumed, 0);
        assert_eq!(summaries[0].hard_limit, Some(100));
        assert_eq!(summaries[0].remaining, Some(100));
        let debug = format!("{:?}", summaries);
        for banned in ["api_key", "base_url", "price", "unit_price"] {
            assert!(!debug.contains(banned), "leaked {banned} in {debug}");
        }
    }

    #[test]
    fn v5_database_migrates_to_v6_and_preserves_rows() {
        use crate::schema::{V2_SCHEMA, V3_SCHEMA, V4_SCHEMA, V5_SCHEMA};

        let dir = TempDir::new("v5-to-v6");
        let path = dir.path().join("hive.db");
        let mission_id = MissionId::new();

        {
            let conn = rusqlite::Connection::open(&path).expect("raw conn");
            conn.execute_batch(V1_SCHEMA).expect("v1");
            conn.execute_batch(V2_SCHEMA).expect("v2");
            conn.execute_batch(V3_SCHEMA).expect("v3");
            conn.execute_batch(V4_SCHEMA).expect("v4");
            conn.execute_batch(V5_SCHEMA).expect("v5");
            conn.execute(
                "INSERT INTO missions (mission_id, state, title, goal, created_at_ms, updated_at_ms) VALUES (?1, 'queued', 'kept', 'goal', 1, 1)",
                rusqlite::params![mission_id.to_string()],
            )
            .expect("mission");
            conn.execute(
                "INSERT INTO budgets (scope_kind, scope_id, dimension, currency, hard_limit, soft_limit, consumed, updated_at_ms) VALUES ('node', 'n_seed', 'tokens', NULL, 100, NULL, 7, 1)",
                [],
            )
            .expect("budget");
            conn.pragma_update(None, "user_version", 5).expect("set v5");
        }

        let store = Store::open(&path).expect("migrate");
        assert_eq!(store.schema_version().expect("version"), 6);
        assert_eq!(
            store
                .get_mission(mission_id)
                .expect("mission")
                .expect("preserved")
                .title,
            "kept"
        );
        let budgets: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM budgets", [], |row| row.get(0))
            .expect("budgets");
        assert_eq!(budgets, 1);
        let consumed: i64 = store
            .conn
            .query_row(
                "SELECT consumed FROM budgets WHERE scope_id = 'n_seed'",
                [],
                |row| row.get(0),
            )
            .expect("consumed");
        assert_eq!(consumed, 7);
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM routines", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("routines"),
            0
        );
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM routine_fires", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("fires"),
            0
        );
    }

    #[test]
    fn create_routine_validates_schedule_timezone_and_limit() {
        let mut store = Store::open_in_memory().expect("store");
        assert!(matches!(
            store.create_routine("n", "g", "garbage", "Asia/Shanghai", 0),
            Err(StoreError::InvalidSchedule(_))
        ));
        assert!(matches!(
            store.create_routine("n", "g", "every:5m", "UTC", 0),
            Err(StoreError::InvalidTimezone(_))
        ));
        for _ in 0..50 {
            store
                .create_routine("n", "g", "every:5m", "Asia/Shanghai", 0)
                .expect("routine");
        }
        assert!(matches!(
            store.create_routine("n", "g", "every:5m", "Asia/Shanghai", 0),
            Err(StoreError::RoutineLimit)
        ));
    }

    #[test]
    fn routine_crud_round_trips_and_emits_update_delete() {
        let mut store = Store::open_in_memory().expect("store");
        let routine = store
            .create_routine("n", "g", "hourly", "Asia/Shanghai", 0)
            .expect("create");
        assert_eq!(routine.schedule, "hourly");
        assert!(routine.enabled);
        assert_eq!(routine.next_run_ms, 3_600_000);

        let (updated, envelope) = store
            .update_routine(routine.routine_id, Some(false), None, 10_000)
            .expect("disable");
        assert!(!updated.enabled);
        assert_eq!(envelope.kind, EventKind::RoutineUpdated);

        let (reenabled, _) = store
            .update_routine(routine.routine_id, Some(true), None, 100_000)
            .expect("re-enable");
        assert!(reenabled.enabled);
        assert_eq!(reenabled.next_run_ms, 100_000 + 3_600_000);

        let (_, _) = store
            .update_routine(routine.routine_id, None, Some("daily:09:00"), 0)
            .expect("schedule change");
        let changed = store
            .get_routine(routine.routine_id)
            .expect("get")
            .expect("present");
        assert_eq!(changed.schedule, "daily:09:00");

        let envelope = store.delete_routine(routine.routine_id, 0).expect("delete");
        assert_eq!(envelope.kind, EventKind::RoutineDeleted);
        assert!(store
            .get_routine(routine.routine_id)
            .expect("get")
            .is_none());

        let events = store.read_all_events().expect("events");
        assert!(events
            .iter()
            .any(|event| event.kind == EventKind::RoutineUpdated));
        assert!(events
            .iter()
            .any(|event| event.kind == EventKind::RoutineDeleted));
        // Routine lifecycle events are not node-scoped.
        for event in events.iter().filter(|event| {
            matches!(
                event.kind,
                EventKind::RoutineUpdated | EventKind::RoutineDeleted
            )
        }) {
            assert!(event.node_id.is_none());
            assert!(event.attempt_no.is_none());
        }
    }

    #[test]
    fn fire_routine_is_idempotent_per_window() {
        let mut store = Store::open_in_memory().expect("store");
        let routine = store
            .create_routine("n", "g", "every:5m", "Asia/Shanghai", 0)
            .expect("routine");
        let due = routine.next_run_ms;
        let outcome = store.fire_routine(routine.routine_id, due).expect("fire");
        assert!(matches!(
            outcome,
            RoutineFireOutcome::Fired { fire_at_ms, .. } if fire_at_ms == due
        ));
        let second = store
            .fire_routine(routine.routine_id, due)
            .expect("second fire");
        assert!(matches!(second, RoutineFireOutcome::NotDue));

        let missions: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM missions", [], |row| row.get(0))
            .expect("missions");
        assert_eq!(missions, 1);
        let fires: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM routine_fires", [], |row| row.get(0))
            .expect("fires");
        assert_eq!(fires, 1);
        let events = store.read_all_events().expect("events");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == EventKind::RoutineTriggered)
                .count(),
            1
        );
    }

    #[test]
    fn fire_routine_disables_corrupted_schedule_and_continues() {
        let mut store = Store::open_in_memory().expect("store");
        let good = store
            .create_routine("good", "g", "every:5m", "Asia/Shanghai", 0)
            .expect("good");
        let bad = store
            .create_routine("bad", "g", "every:5m", "Asia/Shanghai", 0)
            .expect("bad");
        store
            .conn
            .execute(
                "UPDATE routines SET schedule = 'garbage' WHERE routine_id = ?1",
                rusqlite::params![bad.routine_id.to_string()],
            )
            .expect("corrupt");
        let due = good.next_run_ms;

        let good_outcome = store.fire_routine(good.routine_id, due).expect("good fire");
        assert!(matches!(good_outcome, RoutineFireOutcome::Fired { .. }));
        let bad_outcome = store.fire_routine(bad.routine_id, due).expect("bad fire");
        assert!(matches!(bad_outcome, RoutineFireOutcome::Disabled { .. }));

        let bad_row = store
            .get_routine(bad.routine_id)
            .expect("get")
            .expect("row");
        assert!(!bad_row.enabled);
        let events = store.read_all_events().expect("events");
        assert!(events
            .iter()
            .any(|event| event.kind == EventKind::RoutineError));
        assert!(events
            .iter()
            .any(|event| event.kind == EventKind::RoutineTriggered));
    }

    #[test]
    fn daily_schedule_first_run_is_asia_shanghai_epoch() {
        let mut store = Store::open_in_memory().expect("store");
        let routine = store
            .create_routine("n", "g", "daily:09:00", "Asia/Shanghai", 1_795_392_000_000)
            .expect("routine");
        assert_eq!(routine.next_run_ms, 1_795_395_600_000);
        assert_eq!(
            crate::time::rfc3339_from_ms(routine.next_run_ms),
            "2026-11-23T01:00:00Z"
        );
    }

    #[test]
    fn system_scope_mission_is_reserved_and_not_listed() {
        let system_id = MissionId::parse(hive_types::SYSTEM_SCOPE_MISSION_ID)
            .expect("system scope mission id parses");
        let store = Store::open_in_memory().expect("store");
        assert!(store.get_mission(system_id).expect("get").is_none());
        assert!(!store
            .list_missions(100, 0)
            .expect("missions")
            .iter()
            .any(|item| item.mission.mission_id == system_id));
    }

    #[test]
    fn routine_lifecycle_events_use_system_scope() {
        let system_id = MissionId::parse(hive_types::SYSTEM_SCOPE_MISSION_ID)
            .expect("system scope mission id parses");
        let mut store = Store::open_in_memory().expect("store");

        let routine = store
            .create_routine("n", "g", "every:5m", "Asia/Shanghai", 0)
            .expect("routine");
        let (_, updated) = store
            .update_routine(routine.routine_id, Some(false), None, 1)
            .expect("update");
        assert_eq!(updated.kind, EventKind::RoutineUpdated);
        assert_eq!(updated.mission_id, system_id);

        let deleted = store.delete_routine(routine.routine_id, 2).expect("delete");
        assert_eq!(deleted.kind, EventKind::RoutineDeleted);
        assert_eq!(deleted.mission_id, system_id);

        let bad = store
            .create_routine("bad", "g", "every:5m", "Asia/Shanghai", 0)
            .expect("bad");
        store
            .conn
            .execute(
                "UPDATE routines SET schedule = 'garbage' WHERE routine_id = ?1",
                rusqlite::params![bad.routine_id.to_string()],
            )
            .expect("corrupt");
        let outcome = store
            .fire_routine(bad.routine_id, bad.next_run_ms)
            .expect("fire");
        assert!(matches!(outcome, RoutineFireOutcome::Disabled { .. }));
        let error = store
            .read_all_events()
            .expect("events")
            .into_iter()
            .find(|event| event.kind == EventKind::RoutineError)
            .expect("routine error");
        assert_eq!(error.mission_id, system_id);
    }

    #[test]
    fn routine_triggered_uses_real_mission_and_lists_it() {
        let system_id = MissionId::parse(hive_types::SYSTEM_SCOPE_MISSION_ID)
            .expect("system scope mission id parses");
        let mut store = Store::open_in_memory().expect("store");
        let routine = store
            .create_routine("n", "g", "every:5m", "Asia/Shanghai", 0)
            .expect("routine");
        let outcome = store
            .fire_routine(routine.routine_id, routine.next_run_ms)
            .expect("fire");
        let mission_id = match outcome {
            RoutineFireOutcome::Fired { mission, .. } => mission.mission_id,
            other => panic!("expected fired, got {other:?}"),
        };
        assert_ne!(mission_id, system_id);

        let triggered = store
            .read_all_events()
            .expect("events")
            .into_iter()
            .find(|event| event.kind == EventKind::RoutineTriggered)
            .expect("triggered");
        assert_eq!(triggered.mission_id, mission_id);

        assert!(store
            .list_missions(100, 0)
            .expect("missions")
            .iter()
            .any(|item| item.mission.mission_id == mission_id));
        assert!(!store
            .list_missions(100, 0)
            .expect("missions")
            .iter()
            .any(|item| item.mission.mission_id == system_id));
    }
}
