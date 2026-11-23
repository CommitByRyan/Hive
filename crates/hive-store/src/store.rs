use std::path::Path;

use hive_types::{
    AgentSessionId, ApprovalId, ApprovalState, AttemptNo, EventEnvelope, EventInput, LeaseToken,
    MissionId, MissionState, NodeId, NodeState, RunId, RunState, ToolCallId, TurnId,
    EVENT_SCHEMA_VERSION,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::records::{
    AgentSessionRecord, AgentSessionState, ApprovalDecision, ApprovalInput, ApprovalRecord,
    LeaseGuard, MissionAggregate, MissionRecord, NewAgentSession, NewTurnUsage, NodeRecord,
    RunRecord, SnapshotInput, SnapshotRecord, ToolAuditRecord, TurnRecord, TurnState,
    TurnUsageRecord, UsageSource,
};
use crate::schema;
use crate::time::{now_ms, now_rfc3339};
use crate::{MAX_SEQ, SNAPSHOT_SCHEMA_VERSION};

#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> StoreResult<Self> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> StoreResult<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> StoreResult<Self> {
        schema::apply_pragmas(&conn)?;
        let mut conn = conn;
        schema::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    pub fn journal_mode(&self) -> StoreResult<String> {
        let mode = self
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        Ok(mode)
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
        let value: Option<i64> = self
            .conn
            .query_row("SELECT MAX(seq) FROM events", [], |row| row.get(0))?;
        Ok(value.map(|seq| seq as u64))
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

    pub fn reclaim_expired_leases(&mut self, now_ms: i64) -> StoreResult<Vec<NodeRecord>> {
        let expired = self.find_expired_leases(now_ms)?;
        let mut reclaimed = Vec::with_capacity(expired.len());
        for node in expired {
            let next_state = match node.state {
                NodeState::Leased => NodeState::Ready,
                NodeState::Running => NodeState::NeedsRecovery,
                _ => continue,
            };
            let validated = node.state.transition(next_state)?;
            self.conn.execute(
                "UPDATE nodes SET state = ?1, lease_token = NULL, lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?2 WHERE node_id = ?3",
                params![validated.as_str(), now_ms, node.node_id.to_string()],
            )?;
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
        let session = query_agent_session(&self.conn, session_id)?
            .ok_or_else(|| StoreError::NotFound(format!("session {session_id}")))?;
        if session.node_id != node_id {
            return Err(StoreError::Conflict(format!(
                "session {session_id} belongs to node {}",
                session.node_id
            )));
        }
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
        self.conn.execute(
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
        let turn = query_turn(&self.conn, turn_id)?
            .ok_or_else(|| StoreError::NotFound(format!("turn {turn_id}")))?;
        let prompt_tokens = to_sql_integer(usage.prompt_tokens)?;
        let completion_tokens = to_sql_integer(usage.completion_tokens)?;
        let total_tokens = to_sql_integer(usage.total_tokens)?;
        self.conn.execute(
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
}

const NODE_COLUMNS: &str = "node_id, mission_id, run_id, state, attempt_no, retry_budget, lease_token, lease_owner, lease_expires_at_ms, created_at_ms, updated_at_ms";

const AGENT_SESSION_COLUMNS: &str = "session_id, mission_id, run_id, node_id, role, model, system_prompt, state, created_at_ms, updated_at_ms, closed_at_ms";

const TURN_COLUMNS: &str = "turn_id, session_id, node_id, attempt_no, turn_index, request_model, finish_reason, state, output_preview, tool_call_count, created_at_ms, updated_at_ms";

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
        ApprovalState, AttemptNo, EventId, EventInput, EventKind, LeaseToken, MissionId, NodeId,
        NodeState, RunId, ToolCallId,
    };

    use super::Store;
    use crate::error::StoreError;
    use crate::records::{
        can_retry, AgentSessionState, ApprovalDecision, ApprovalInput, LeaseGuard, MissionRecord,
        NewTurnUsage, NodeRecord, RunRecord, SnapshotInput, TurnState, UsageSource,
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
    fn fresh_database_has_v3_tables() {
        let store = Store::open_in_memory().expect("store");
        assert_eq!(store.schema_version().expect("version"), 3);
        let tables: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('tool_calls','artifacts','tool_audit','agent_sessions','turns','turn_usage')",
                [],
                |row| row.get(0),
            )
            .expect("table count");
        assert_eq!(tables, 6);
    }

    #[test]
    fn v1_database_migrates_to_v3_and_preserves_rows() {
        let dir = TempDir::new("v1-to-v3");
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
        assert_eq!(store.schema_version().expect("version"), 3);
        let mission = store
            .get_mission(mission_id)
            .expect("mission")
            .expect("preserved");
        assert_eq!(mission.title, "old");
        let tables: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('tool_calls','artifacts','tool_audit','agent_sessions','turns','turn_usage')",
                [],
                |row| row.get(0),
            )
            .expect("table count");
        assert_eq!(tables, 6);
    }

    #[test]
    fn v2_database_migrates_to_v3_and_preserves_rows() {
        use crate::schema::V2_SCHEMA;

        let dir = TempDir::new("v2-to-v3");
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
        assert_eq!(store.schema_version().expect("version"), 3);
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
}
