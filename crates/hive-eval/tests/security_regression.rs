//! End-to-end security regressions through the unified
//! `validate -> policy -> approval -> execute -> audit` path.

use hive_eval::TempDir;
use hive_tools::{
    AllowAllApprovals, DenyApprovals, RecordingAuditSink, ToolCall, ToolContext, ToolDispatcher,
};
use hive_types::{AttemptNo, EventKind, LeaseToken, MissionId, NodeId};
use serde_json::json;

fn context() -> ToolContext {
    ToolContext {
        mission_id: MissionId::new(),
        run_id: None,
        node_id: NodeId::new(),
        session_id: None,
        attempt_no: AttemptNo::FIRST,
        lease_token: LeaseToken::generate(),
        policy_version: "v1".to_owned(),
    }
}

#[test]
fn shell_disabled_spawns_no_process() {
    let dir = TempDir::new("sec-shell");
    let dispatcher = ToolDispatcher::with_default_root(dir.path()).expect("dispatcher");
    let mut audit = RecordingAuditSink::default();
    let error = dispatcher
        .dispatch(
            &context(),
            ToolCall::new("shell.run", json!({ "program": "cmd.exe" })),
            &mut audit,
            &mut AllowAllApprovals,
        )
        .expect_err("shell disabled");
    assert_eq!(error.code(), "shell.disabled");
    assert!(audit
        .events()
        .iter()
        .all(|event| event.kind != EventKind::NodeToolStarted));
}

#[test]
fn path_escape_is_rejected_at_dispatcher() {
    let dir = TempDir::new("sec-path");
    let dispatcher = ToolDispatcher::with_default_root(dir.path()).expect("dispatcher");
    let mut audit = RecordingAuditSink::default();
    let error = dispatcher
        .dispatch(
            &context(),
            ToolCall::new("fs.read", json!({ "path": "../secret.txt" })),
            &mut audit,
            &mut AllowAllApprovals,
        )
        .expect_err("traversal rejected");
    assert_eq!(error.code(), "path.outside_workspace");
    assert!(audit
        .events()
        .iter()
        .all(|event| event.kind != EventKind::NodeToolStarted));
}

#[test]
fn unapproved_high_risk_tool_emits_no_tool_started() {
    let dir = TempDir::new("sec-approval");
    let dispatcher = ToolDispatcher::with_default_root(dir.path()).expect("dispatcher");
    let mut audit = RecordingAuditSink::default();
    let error = dispatcher
        .dispatch(
            &context(),
            ToolCall::new(
                "fs.write",
                json!({ "path": "out.txt", "content": "payload" }),
            ),
            &mut audit,
            &mut DenyApprovals,
        )
        .expect_err("approval required");
    assert_eq!(error.code(), "approval.required");
    assert!(!dir.path().join("out.txt").exists());
    assert!(audit
        .events()
        .iter()
        .all(|event| event.kind != EventKind::NodeToolStarted));
}
