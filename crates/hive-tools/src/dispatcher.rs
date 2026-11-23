use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hive_policy::{
    request_hash, ApprovalMode, HashRequest, NetworkPolicy, PathAccess, PathPolicy, PolicyError,
    RiskLevel, ShellPolicy, ToolRegistry,
};
use hive_types::{EventId, EventInput, EventKind, ToolCallId};
use serde_json::{json, Value};

use crate::audit::fail_closed;
use crate::types::builtin_spec;
use crate::{
    ApprovalGate, AuditSink, NormalizedToolRequest, ToolCall, ToolCallOutcome, ToolContext,
    ToolError, ToolOutput, ToolResult,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn default_registry() -> Result<ToolRegistry, ToolError> {
    let mut registry = ToolRegistry::new();
    registry.register(builtin_spec(
        "fs.read",
        RiskLevel::ReadOnly,
        ApprovalMode::Never,
        64 * 1024,
        &["path"],
        &["path"],
        64 * 1024,
    ))?;
    registry.register(builtin_spec(
        "fs.write",
        RiskLevel::Write,
        ApprovalMode::Policy,
        64 * 1024,
        &["path", "content"],
        &["path", "content"],
        256 * 1024,
    ))?;
    registry.register(builtin_spec(
        "fs.list",
        RiskLevel::ReadOnly,
        ApprovalMode::Never,
        64 * 1024,
        &["path"],
        &["path"],
        64 * 1024,
    ))?;
    registry.register(
        builtin_spec(
            "shell.run",
            RiskLevel::Execute,
            ApprovalMode::Always,
            64 * 1024,
            &["program"],
            &["program", "args", "cwd"],
            64 * 1024,
        )
        .with_timeout(std::time::Duration::from_secs(30)),
    )?;
    Ok(registry)
}

pub struct ToolDispatcher {
    registry: ToolRegistry,
    paths: PathPolicy,
    network: NetworkPolicy,
    shell: ShellPolicy,
}

enum PreparedAction {
    Read(PathBuf),
    Write {
        path: PathBuf,
        content: Vec<u8>,
    },
    List(PathBuf),
    Shell {
        program: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
    },
}

impl ToolDispatcher {
    pub fn new(registry: ToolRegistry, paths: PathPolicy, network: NetworkPolicy) -> Self {
        Self {
            registry,
            paths,
            network,
            shell: ShellPolicy::disabled(),
        }
    }

    pub fn with_default_root(root: impl AsRef<Path>) -> Result<Self, ToolError> {
        let registry = default_registry()?;
        let paths = PathPolicy::new([root])?;
        Ok(Self::new(registry, paths, NetworkPolicy::deny_all()))
    }

    /// Attach a shell policy. Production callers should build the policy from
    /// the configured allow-list; `with_default_root` keeps shell disabled.
    pub fn with_shell(mut self, shell: ShellPolicy) -> Self {
        self.shell = shell;
        self
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn paths(&self) -> &PathPolicy {
        &self.paths
    }

    pub fn network(&self) -> &NetworkPolicy {
        &self.network
    }

    pub fn shell(&self) -> &ShellPolicy {
        &self.shell
    }

    pub fn dispatch(
        &self,
        context: &ToolContext,
        call: ToolCall,
        audit: &mut dyn AuditSink,
        approvals: &mut dyn ApprovalGate,
    ) -> ToolResult<ToolCallOutcome> {
        let spec = self
            .registry
            .get(&call.tool_name)
            .ok_or_else(|| ToolError::from(PolicyError::ToolNotFound(call.tool_name.clone())))?;
        spec.validate_arguments(&call.arguments)?;

        let mission_id = context.mission_id.to_string();
        let run_id = context.run_id.map(|id| id.to_string());
        let node_id = context.node_id.to_string();
        let session_id = context.session_id.map(|id| id.to_string());
        let hash = request_hash(&HashRequest {
            tool_name: &spec.name,
            tool_version: spec.version,
            arguments: &call.arguments,
            mission_id: &mission_id,
            run_id: run_id.as_deref(),
            node_id: &node_id,
            session_id: session_id.as_deref(),
            attempt_no: context.attempt_no.get(),
            lease_token: context.lease_token.as_str(),
            policy_version: &context.policy_version,
        });

        let request = NormalizedToolRequest {
            tool_call_id: ToolCallId::new(),
            tool_name: spec.name.clone(),
            tool_version: spec.version,
            mission_id: context.mission_id,
            run_id: context.run_id,
            node_id: context.node_id,
            session_id: context.session_id,
            attempt_no: context.attempt_no,
            lease_token: context.lease_token.clone(),
            arguments: call.arguments,
            request_hash: hash,
            policy_version: context.policy_version.clone(),
        };

        self.audit_event(
            context,
            &request,
            EventKind::NodeToolCall,
            json!({ "phase": "intent" }),
            audit,
        )?;

        let prepared = match self.prepare(&request, spec) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.audit_error(context, &request, &error, audit)?;
                return Err(error);
            }
        };

        if let Err(error) = approvals.check(&request, spec) {
            self.audit_error(context, &request, &error, audit)?;
            return Err(error);
        }

        self.audit_event(
            context,
            &request,
            EventKind::NodeToolStarted,
            json!({ "phase": "started" }),
            audit,
        )?;

        match self.execute(&request, spec, prepared) {
            Ok(output) => {
                self.audit_success(context, &request, &output, audit)?;
                Ok(ToolCallOutcome {
                    tool_call_id: request.tool_call_id,
                    request_hash: request.request_hash.clone(),
                    output,
                })
            }
            Err(error) => {
                self.audit_error(context, &request, &error, audit)?;
                Err(error)
            }
        }
    }

    fn prepare(
        &self,
        request: &NormalizedToolRequest,
        spec: &hive_policy::ToolSpec,
    ) -> ToolResult<PreparedAction> {
        match request.tool_name.as_str() {
            "fs.read" => {
                let path = string_arg(&request.arguments, "path")?;
                let authorized = self.paths.authorize(0, path, PathAccess::Read)?;
                Ok(PreparedAction::Read(authorized.final_path().to_path_buf()))
            }
            "fs.list" => {
                let path = string_arg(&request.arguments, "path")?;
                let authorized = if path.is_empty() || path == "." {
                    self.paths.authorize_root(0)?
                } else {
                    self.paths.authorize(0, path, PathAccess::List)?
                };
                Ok(PreparedAction::List(authorized.final_path().to_path_buf()))
            }
            "fs.write" => {
                let path = string_arg(&request.arguments, "path")?;
                let content = string_arg(&request.arguments, "content")?;
                if content.len() > spec.max_argument_bytes {
                    return Err(PolicyError::ToolArgumentsDenied(
                        "content exceeds the write limit".to_owned(),
                    )
                    .into());
                }
                let authorized = self.paths.authorize(0, path, PathAccess::Write)?;
                Ok(PreparedAction::Write {
                    path: authorized.final_path().to_path_buf(),
                    content: content.as_bytes().to_vec(),
                })
            }
            "shell.run" => {
                let program = string_arg(&request.arguments, "program")?;
                let authorized_program = self.shell.authorize_program(program)?;
                let args = string_list_arg(&request.arguments, "args")?;
                let cwd = match request.arguments.get("cwd") {
                    Some(Value::String(cwd)) => {
                        let authorized = self.paths.authorize(0, cwd, PathAccess::List)?;
                        authorized.final_path().to_path_buf()
                    }
                    None => self.paths.authorize_root(0)?.final_path().to_path_buf(),
                    Some(_) => {
                        return Err(PolicyError::ToolArgumentsDenied(
                            "argument `cwd` must be a string".to_owned(),
                        )
                        .into());
                    }
                };
                Ok(PreparedAction::Shell {
                    program: authorized_program,
                    args,
                    cwd,
                })
            }
            other => Err(PolicyError::ToolNotFound(other.to_owned()).into()),
        }
    }

    fn execute(
        &self,
        request: &NormalizedToolRequest,
        spec: &hive_policy::ToolSpec,
        prepared: PreparedAction,
    ) -> ToolResult<ToolOutput> {
        match prepared {
            PreparedAction::Read(path) => read_bounded(&path, spec.max_output),
            PreparedAction::List(path) => list_bounded(&path, spec.max_output),
            PreparedAction::Write { path, content } => write_atomic(&path, &content),
            PreparedAction::Shell { program, args, cwd } => {
                crate::shell::run(&program, &args, &cwd, spec.timeout, spec.max_output)
            }
        }
        .map_err(|error| {
            let mut wrapped = ToolError::new(
                error.error_code(),
                format!("{}: {}", request.tool_name, error.message()),
            );
            if let Some(exit_code) = error.exit_code() {
                wrapped = wrapped.with_exit_code(exit_code);
            }
            if let Some(output) = error.output() {
                wrapped = wrapped.with_output(output.clone());
            }
            wrapped
        })
    }

    fn audit_event(
        &self,
        context: &ToolContext,
        request: &NormalizedToolRequest,
        kind: EventKind,
        extra: Value,
        audit: &mut dyn AuditSink,
    ) -> ToolResult<()> {
        let mut payload = json!({
            "tool_call_id": request.tool_call_id.to_string(),
            "tool_name": request.tool_name,
            "tool_version": request.tool_version,
            "request_hash": request.request_hash,
        });
        if let (Some(payload), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                payload.insert(key.clone(), value.clone());
            }
        }
        let event = EventInput {
            event_id: EventId::new(),
            kind,
            mission_id: context.mission_id,
            run_id: context.run_id,
            node_id: Some(context.node_id),
            session_id: context.session_id,
            attempt_no: Some(context.attempt_no),
            trace_id: None,
            payload,
        };
        audit.append(&event).map_err(|error| {
            fail_closed(format!(
                "cannot persist {} audit event: {error}",
                kind.as_str()
            ))
        })
    }

    fn audit_success(
        &self,
        context: &ToolContext,
        request: &NormalizedToolRequest,
        output: &ToolOutput,
        audit: &mut dyn AuditSink,
    ) -> ToolResult<()> {
        self.audit_event(
            context,
            request,
            EventKind::NodeToolResult,
            json!({
                "status": "ok",
                "summary": output.summary,
                "truncated": output.truncated,
            }),
            audit,
        )
    }

    fn audit_error(
        &self,
        context: &ToolContext,
        request: &NormalizedToolRequest,
        error: &ToolError,
        audit: &mut dyn AuditSink,
    ) -> ToolResult<()> {
        let truncated = error
            .output()
            .map(|output| output.truncated)
            .unwrap_or(false);
        let summary = error
            .output()
            .map(|output| output.summary.clone())
            .unwrap_or_default();
        let mut payload = json!({
            "status": "error",
            "code": error.code(),
            "message": error.message(),
            "truncated": truncated,
            "summary": summary,
        });
        if let Some(exit_code) = error.exit_code() {
            payload["exit_code"] = json!(exit_code);
        }
        self.audit_event(context, request, EventKind::NodeToolResult, payload, audit)
    }
}

fn string_arg<'a>(arguments: &'a Value, name: &str) -> ToolResult<&'a str> {
    arguments.get(name).and_then(Value::as_str).ok_or_else(|| {
        ToolError::from(PolicyError::ToolArgumentsDenied(format!(
            "argument `{name}` must be a string"
        )))
    })
}

fn string_list_arg(arguments: &Value, name: &str) -> ToolResult<Vec<String>> {
    let Some(value) = arguments.get(name) else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(ToolError::from(PolicyError::ToolArgumentsDenied(format!(
            "argument `{name}` must be an array of strings"
        ))));
    };
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Some(text) = item.as_str() else {
            return Err(ToolError::from(PolicyError::ToolArgumentsDenied(format!(
                "argument `{name}`[{index}] must be a string"
            ))));
        };
        out.push(text.to_owned());
    }
    Ok(out)
}

fn read_bounded(path: &Path, max_output: usize) -> ToolResult<ToolOutput> {
    let mut file = File::open(path)?;
    let limit = u64::try_from(max_output).unwrap_or(u64::MAX);
    let mut buffer = Vec::new();
    Read::by_ref(&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut buffer)?;
    let truncated = buffer.len() > max_output;
    if truncated {
        buffer.truncate(max_output);
    }
    let bytes = buffer.len();
    Ok(ToolOutput {
        content: String::from_utf8_lossy(&buffer).into_owned(),
        bytes,
        truncated,
        summary: format!("read {bytes} bytes"),
    })
}

fn list_bounded(path: &Path, max_output: usize) -> ToolResult<ToolOutput> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let kind = if file_type.is_symlink() {
            "link"
        } else if file_type.is_dir() {
            "dir"
        } else {
            "file"
        };
        entries.push((entry.file_name().to_string_lossy().into_owned(), kind));
    }
    entries.sort_by_key(|(name, _)| name.to_lowercase());

    let mut lines = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    for (name, kind) in entries {
        let line = format!("{kind}\t{name}");
        let additional = if lines.is_empty() {
            line.len()
        } else {
            line.len() + 1
        };
        if bytes.saturating_add(additional) > max_output {
            truncated = true;
            break;
        }
        bytes += additional;
        lines.push(line);
    }
    let content = lines.join("\n");
    let count = lines.len();
    Ok(ToolOutput {
        content,
        bytes,
        truncated,
        summary: format!("listed {count} entries"),
    })
}

fn write_atomic(path: &Path, content: &[u8]) -> ToolResult<ToolOutput> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let temp = temp_path(path);
    let write_result = (|| -> ToolResult<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result?;
    Ok(ToolOutput {
        content: String::new(),
        bytes: content.len(),
        truncated: false,
        summary: format!("wrote {} bytes", content.len()),
    })
}

fn temp_path(path: &Path) -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hive-temp".to_owned());
    path.with_file_name(format!(
        ".{file_name}.hive-tmp-{}-{counter}",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use hive_policy::{NetworkPolicy, PathPolicy};
    use hive_types::{AttemptNo, LeaseToken, MissionId, NodeId};
    use serde_json::json;

    use super::*;
    use crate::{AllowAllApprovals, DenyApprovals, RecordingAuditSink};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "hive-tools-{name}-{}-{counter}",
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

    fn dispatcher(root: &Path) -> ToolDispatcher {
        ToolDispatcher::with_default_root(root).expect("dispatcher")
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall::new(name, arguments)
    }

    #[test]
    fn fs_read_is_bounded_and_audits_three_events() {
        let temp = TempDir::new("read");
        std::fs::write(temp.path().join("file.txt"), b"hello world").expect("write");
        let dispatcher = dispatcher(temp.path());
        let mut audit = RecordingAuditSink::default();

        let outcome = dispatcher
            .dispatch(
                &context(),
                call("fs.read", json!({ "path": "file.txt" })),
                &mut audit,
                &mut AllowAllApprovals,
            )
            .expect("read");
        assert_eq!(outcome.output.content, "hello world");
        assert_eq!(outcome.output.bytes, 11);
        assert!(!outcome.output.truncated);
        assert_eq!(
            audit
                .events()
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                EventKind::NodeToolCall,
                EventKind::NodeToolStarted,
                EventKind::NodeToolResult
            ]
        );
    }

    #[test]
    fn fs_read_truncates_at_limit() {
        let temp = TempDir::new("truncate");
        std::fs::write(temp.path().join("file.txt"), b"0123456789").expect("write");
        let limited = hive_policy::ToolSpec::new(
            "fs.read",
            1,
            RiskLevel::ReadOnly,
            ApprovalMode::Never,
            std::time::Duration::from_secs(5),
            4,
        )
        .with_arguments(["path".to_owned()], ["path".to_owned()]);
        let mut fresh = ToolRegistry::new();
        fresh.register(limited).expect("limited");
        let dispatcher = ToolDispatcher::new(
            fresh,
            PathPolicy::new([temp.path()]).expect("policy"),
            NetworkPolicy::deny_all(),
        );
        let outcome = dispatcher
            .dispatch(
                &context(),
                call("fs.read", json!({ "path": "file.txt" })),
                &mut RecordingAuditSink::default(),
                &mut AllowAllApprovals,
            )
            .expect("read");
        assert_eq!(outcome.output.content, "0123");
        assert!(outcome.output.truncated);
    }

    #[test]
    fn fs_read_rejects_traversal_before_execution() {
        let temp = TempDir::new("traversal");
        let dispatcher = dispatcher(temp.path());
        let mut audit = RecordingAuditSink::default();
        let error = dispatcher
            .dispatch(
                &context(),
                call("fs.read", json!({ "path": "../secret.txt" })),
                &mut audit,
                &mut AllowAllApprovals,
            )
            .expect_err("traversal");
        assert_eq!(error.code(), "path.outside_workspace");
        assert!(audit
            .events()
            .iter()
            .all(|event| event.kind != EventKind::NodeToolStarted));
        let result = audit
            .events()
            .iter()
            .find(|event| event.kind == EventKind::NodeToolResult)
            .expect("result");
        assert_eq!(result.payload["code"], "path.outside_workspace");
    }

    #[test]
    fn fs_write_uses_temp_file_and_atomic_rename() {
        let temp = TempDir::new("write");
        let dispatcher = dispatcher(temp.path());
        let mut audit = RecordingAuditSink::default();
        let outcome = dispatcher
            .dispatch(
                &context(),
                call(
                    "fs.write",
                    json!({ "path": "nested/out.txt", "content": "payload" }),
                ),
                &mut audit,
                &mut AllowAllApprovals,
            )
            .expect("write");
        assert_eq!(outcome.output.bytes, 7);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("nested/out.txt")).expect("read"),
            "payload"
        );
        let leftovers = std::fs::read_dir(temp.path().join("nested"))
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("hive-tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn fs_write_requires_approval_and_does_not_create_file() {
        let temp = TempDir::new("approval");
        let dispatcher = dispatcher(temp.path());
        let error = dispatcher
            .dispatch(
                &context(),
                call(
                    "fs.write",
                    json!({ "path": "out.txt", "content": "payload" }),
                ),
                &mut RecordingAuditSink::default(),
                &mut DenyApprovals,
            )
            .expect_err("approval");
        assert_eq!(error.code(), "approval.required");
        assert!(!temp.path().join("out.txt").exists());
    }

    #[test]
    fn fs_list_returns_sorted_bounded_entries() {
        let temp = TempDir::new("list");
        std::fs::create_dir(temp.path().join("sub")).expect("sub");
        std::fs::write(temp.path().join("sub/b.txt"), b"b").expect("b");
        std::fs::write(temp.path().join("sub/a.txt"), b"a").expect("a");
        let dispatcher = dispatcher(temp.path());
        let outcome = dispatcher
            .dispatch(
                &context(),
                call("fs.list", json!({ "path": "sub" })),
                &mut RecordingAuditSink::default(),
                &mut AllowAllApprovals,
            )
            .expect("list");
        assert_eq!(
            outcome.output.content,
            "file\ta.txt\nfile\tb.txt".to_owned()
        );
        assert_eq!(outcome.output.bytes, 21);
        assert!(!outcome.output.truncated);
    }

    #[test]
    fn fs_list_can_list_workspace_root_with_dot() {
        let temp = TempDir::new("list-root");
        std::fs::write(temp.path().join("root.txt"), b"r").expect("file");
        let dispatcher = dispatcher(temp.path());
        let outcome = dispatcher
            .dispatch(
                &context(),
                call("fs.list", json!({ "path": "." })),
                &mut RecordingAuditSink::default(),
                &mut AllowAllApprovals,
            )
            .expect("list root");
        assert_eq!(outcome.output.content, "file\troot.txt");
    }

    #[test]
    fn shell_run_is_disabled_without_started_event() {
        let temp = TempDir::new("shell");
        let dispatcher = dispatcher(temp.path());
        let mut audit = RecordingAuditSink::default();
        let error = dispatcher
            .dispatch(
                &context(),
                call("shell.run", json!({ "program": "cmd.exe" })),
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
    fn extra_arguments_are_rejected_before_any_audit_or_execution() {
        let temp = TempDir::new("extra");
        let dispatcher = dispatcher(temp.path());
        let mut audit = RecordingAuditSink::default();
        let error = dispatcher
            .dispatch(
                &context(),
                call("fs.read", json!({ "path": "a.txt", "extra": 1 })),
                &mut audit,
                &mut AllowAllApprovals,
            )
            .expect_err("extra");
        assert_eq!(error.code(), "tool.arguments_denied");
        assert!(audit.events().is_empty());
    }
}
