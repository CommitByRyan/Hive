//! Integration tests for real `shell.run` execution. These use the
//! `hive_shell_helper` binary so we can assert on literal argv handling,
//! bounded output and whole-process-tree cleanup on timeout.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hive_policy::{ApprovalMode, NetworkPolicy, PathPolicy, RiskLevel, ToolRegistry, ToolSpec};
use hive_tools::{
    AllowAllApprovals, RecordingAuditSink, ShellPolicy, ToolCall, ToolContext, ToolDispatcher,
};
use hive_types::{AttemptNo, EventKind, LeaseToken, MissionId, NodeId};
use serde_json::json;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "hive-tools-shell-{name}-{}-{counter}",
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

fn helper_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hive_shell_helper"))
}

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

fn enabled_dispatcher(
    root: &Path,
    helper: &Path,
    timeout: Duration,
    max_output: usize,
) -> ToolDispatcher {
    let spec = ToolSpec::new(
        "shell.run",
        1,
        RiskLevel::Execute,
        ApprovalMode::Always,
        timeout,
        max_output,
    )
    .with_arguments(
        ["program".to_owned()],
        ["program".to_owned(), "args".to_owned(), "cwd".to_owned()],
    );
    let mut registry = ToolRegistry::new();
    registry.register(spec).expect("shell spec");
    let paths = PathPolicy::new([root]).expect("paths");
    let shell = ShellPolicy::new(true, [helper.to_path_buf()]).expect("shell policy");
    ToolDispatcher::new(registry, paths, NetworkPolicy::deny_all()).with_shell(shell)
}

fn no_started_event(audit: &RecordingAuditSink) -> bool {
    audit
        .events()
        .iter()
        .all(|event| event.kind != EventKind::NodeToolStarted)
}

#[test]
fn shell_disabled_and_argv_not_interpreted() {
    let temp = TempDir::new("disabled");
    let helper = helper_path();

    let disabled = ToolDispatcher::with_default_root(temp.path()).expect("dispatcher");
    let mut audit = RecordingAuditSink::default();
    let error = disabled
        .dispatch(
            &context(),
            ToolCall::new("shell.run", json!({ "program": helper.to_string_lossy() })),
            &mut audit,
            &mut AllowAllApprovals,
        )
        .expect_err("shell disabled");
    assert_eq!(error.code(), "shell.disabled");
    assert!(no_started_event(&audit));

    let dispatcher = enabled_dispatcher(temp.path(), &helper, Duration::from_secs(10), 4096);
    let outcome = dispatcher
        .dispatch(
            &context(),
            ToolCall::new(
                "shell.run",
                json!({
                    "program": helper.to_string_lossy(),
                    "args": ["echo", "a;b", "$(evil)", "|", ">", "out.txt"],
                }),
            ),
            &mut RecordingAuditSink::default(),
            &mut AllowAllApprovals,
        )
        .expect("argv literal");
    assert_eq!(outcome.output.content, "a;b\n$(evil)\n|\n>\nout.txt\n");
    assert!(!outcome.output.truncated);
    assert!(!temp.path().join("out.txt").exists());
}

#[test]
fn shell_program_denied_before_spawn() {
    let temp = TempDir::new("denied");
    let helper = helper_path();
    let dispatcher = enabled_dispatcher(temp.path(), &helper, Duration::from_secs(10), 4096);

    for program in ["C:\\Windows\\System32\\cmd.exe", "relative.exe"] {
        let mut audit = RecordingAuditSink::default();
        let error = dispatcher
            .dispatch(
                &context(),
                ToolCall::new("shell.run", json!({ "program": program })),
                &mut audit,
                &mut AllowAllApprovals,
            )
            .expect_err("program denied");
        assert_eq!(error.code(), "shell.program_denied", "program={program}");
        assert!(no_started_event(&audit), "program={program}");
    }
}

#[test]
fn shell_timeout_kills_descendants_and_truncates() {
    let temp = TempDir::new("timeout");
    let helper = helper_path();
    let dispatcher = enabled_dispatcher(temp.path(), &helper, Duration::from_secs(2), 512);

    let outcome = dispatcher
        .dispatch(
            &context(),
            ToolCall::new(
                "shell.run",
                json!({ "program": helper.to_string_lossy(), "args": ["overflow"] }),
            ),
            &mut RecordingAuditSink::default(),
            &mut AllowAllApprovals,
        )
        .expect("bounded output");
    assert!(outcome.output.truncated);
    assert_eq!(outcome.output.content.len(), 512);
    assert_eq!(outcome.output.bytes, 512);

    let error = dispatcher
        .dispatch(
            &context(),
            ToolCall::new(
                "shell.run",
                json!({
                    "program": helper.to_string_lossy(),
                    "args": ["spawn-tree", "3000", "finished.txt"],
                }),
            ),
            &mut RecordingAuditSink::default(),
            &mut AllowAllApprovals,
        )
        .expect_err("timeout");
    assert_eq!(error.code(), "shell.timeout");

    // The descendant would write `finished.txt` after 3000ms if it survived;
    // the Job Object close must have killed it before then.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!temp.path().join("finished.txt").exists());
}
