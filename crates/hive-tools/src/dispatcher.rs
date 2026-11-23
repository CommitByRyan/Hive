use std::io::Read;
use std::path::{Path, PathBuf};

use hive_platform::{
    default_path_platform, DirectoryEntry, DirectoryEntryKind, PathPlatform, PlatformError,
};
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
    Read(hive_platform::PathHandle),
    Write {
        relative: PathBuf,
        content: Vec<u8>,
    },
    List {
        handle: hive_platform::PathHandle,
    },
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

    #[cfg(test)]
    fn prepare_for_test(
        &self,
        request: &NormalizedToolRequest,
        spec: &hive_policy::ToolSpec,
    ) -> ToolResult<PreparedAction> {
        self.prepare(request, spec)
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
                let authorized = self.paths.authorize_handle(0, path, PathAccess::Read)?;
                Ok(PreparedAction::Read(authorized.into_platform_handle()))
            }
            "fs.list" => {
                let path = string_arg(&request.arguments, "path")?;
                let authorized = self.paths.authorize_handle_listing(0, path)?;
                Ok(PreparedAction::List {
                    handle: authorized.into_platform_handle(),
                })
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
                let root = self.paths.root(0)?;
                let authorized = hive_policy::validate_relative_path(path)?;
                if let Some(parent) = authorized.parent() {
                    let parent = if parent.as_os_str().is_empty() {
                        root
                    } else {
                        &root.join(parent)
                    };
                    if parent.exists() && !parent.is_dir() {
                        return Err(ToolError::new(
                            hive_policy::ErrorCode::PathUnsupportedForm,
                            format!("write parent is not a directory: {path}"),
                        ));
                    }
                }
                Ok(PreparedAction::Write {
                    relative: authorized,
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
            PreparedAction::Read(handle) => read_bounded(handle, spec.max_output),
            PreparedAction::List { handle } => list_bounded(&handle, spec.max_output),
            PreparedAction::Write { relative, content } => {
                let platform = default_path_platform();
                let root = platform
                    .open_root(self.paths.root(0)?)
                    .map_err(map_platform_error)?;
                platform
                    .atomic_write(&root, &relative, &content)
                    .map_err(map_platform_error)?;
                Ok(ToolOutput {
                    content: String::new(),
                    bytes: content.len(),
                    truncated: false,
                    summary: format!("wrote {} bytes", content.len()),
                })
            }
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

fn map_platform_error(error: PlatformError) -> ToolError {
    ToolError::from(match error {
        PlatformError::ReparsePoint(message) => {
            hive_policy::PolicyError::PathReparseEscape(message)
        }
        PlatformError::OutsideRoot(message) | PlatformError::CrossVolume(message) => {
            hive_policy::PolicyError::PathOutsideWorkspace(message)
        }
        PlatformError::NotFound(message) => hive_policy::PolicyError::PathNotFound(message),
        PlatformError::Unsupported(message) => {
            hive_policy::PolicyError::PathUnsupportedForm(message)
        }
        PlatformError::Io { context, source } => {
            hive_policy::PolicyError::ToolExecutionFailed(format!("{context}: {source}"))
        }
    })
}

fn read_bounded(handle: hive_platform::PathHandle, max_output: usize) -> ToolResult<ToolOutput> {
    let mut file = handle.into_file().map_err(map_platform_error)?;
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

fn list_bounded(handle: &hive_platform::PathHandle, max_output: usize) -> ToolResult<ToolOutput> {
    let platform = default_path_platform();
    let mut entries = platform
        .list_directory(handle)
        .map_err(map_platform_error)?;
    entries.sort_by_key(|entry| entry.name.to_lowercase());

    let mut lines = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    for DirectoryEntry { name, kind } in entries {
        let kind = match kind {
            DirectoryEntryKind::Link => "link",
            DirectoryEntryKind::Directory => "dir",
            DirectoryEntryKind::File => "file",
        };
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

    fn request(name: &str, arguments: serde_json::Value) -> NormalizedToolRequest {
        NormalizedToolRequest {
            tool_call_id: ToolCallId::new(),
            tool_name: name.to_owned(),
            tool_version: 1,
            mission_id: MissionId::new(),
            run_id: None,
            node_id: NodeId::new(),
            session_id: None,
            attempt_no: AttemptNo::FIRST,
            lease_token: LeaseToken::generate(),
            arguments,
            request_hash: "test".to_owned(),
            policy_version: "v1".to_owned(),
        }
    }

    #[test]
    #[cfg(windows)]
    fn fs_read_reads_from_authorized_handle_after_path_swap() {
        let temp = TempDir::new("read-swap");
        std::fs::write(temp.path().join("file.txt"), b"authorized").expect("original");
        let dispatcher = dispatcher(temp.path());
        let spec = dispatcher.registry().get("fs.read").expect("spec");
        let request = request("fs.read", json!({ "path": "file.txt" }));
        let prepared = dispatcher
            .prepare_for_test(&request, spec)
            .expect("prepare");
        std::fs::rename(temp.path().join("file.txt"), temp.path().join("old.txt"))
            .expect("move original");
        std::fs::write(temp.path().join("file.txt"), b"replacement").expect("replacement");
        let output = dispatcher
            .execute(&request, spec, prepared)
            .expect("execute");
        assert_eq!(output.content, "authorized");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("file.txt")).expect("new path"),
            "replacement"
        );
    }

    #[test]
    fn fs_write_replaces_atomically_and_keeps_temp_clean() {
        let temp = TempDir::new("atomic-replace");
        std::fs::write(temp.path().join("out.txt"), b"old").expect("old");
        let dispatcher = dispatcher(temp.path());
        dispatcher
            .dispatch(
                &context(),
                call("fs.write", json!({ "path": "out.txt", "content": "new" })),
                &mut RecordingAuditSink::default(),
                &mut AllowAllApprovals,
            )
            .expect("replace");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("out.txt")).expect("new"),
            "new"
        );
        let leftovers = std::fs::read_dir(temp.path())
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("hive-tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn fs_write_cleans_temp_when_replace_fails() {
        let temp = TempDir::new("replace-fails");
        std::fs::create_dir(temp.path().join("target.txt")).expect("target directory");
        std::fs::write(temp.path().join("keep.txt"), b"unchanged").expect("keep");
        let dispatcher = dispatcher(temp.path());
        let error = dispatcher
            .dispatch(
                &context(),
                call(
                    "fs.write",
                    json!({ "path": "target.txt", "content": "replacement" }),
                ),
                &mut RecordingAuditSink::default(),
                &mut AllowAllApprovals,
            )
            .expect_err("directory target");
        assert_eq!(error.code(), "path.unsupported_form");
        assert!(temp.path().join("target.txt").is_dir());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("keep.txt")).expect("keep"),
            "unchanged"
        );
        let leftovers = std::fs::read_dir(temp.path())
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("hive-tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    #[cfg(windows)]
    fn fs_write_rejects_junction_parent_before_temp_creation() {
        let outside = TempDir::new("write-junction-outside");
        let temp = TempDir::new("write-junction");
        let link = temp.path().join("link");
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !matches!(status, Ok(status) if status.success()) {
            eprintln!("SKIP: NTFS junction creation unavailable");
            return;
        }
        let dispatcher = dispatcher(temp.path());
        let error = dispatcher
            .dispatch(
                &context(),
                call(
                    "fs.write",
                    json!({ "path": "link/out.txt", "content": "denied" }),
                ),
                &mut RecordingAuditSink::default(),
                &mut AllowAllApprovals,
            )
            .expect_err("junction parent");
        assert_eq!(error.code(), "path.reparse_escape");
        assert!(!outside.path().join("out.txt").exists());
        let leftovers = std::fs::read_dir(outside.path())
            .expect("outside")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("hive-tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir(link);
    }

    #[test]
    #[cfg(windows)]
    fn fs_write_replaces_leaf_with_authorized_parent_after_target_swap() {
        let temp = TempDir::new("write-target-swap");
        std::fs::write(temp.path().join("file.txt"), b"old").expect("old");
        let dispatcher = dispatcher(temp.path());
        let spec = dispatcher.registry().get("fs.write").expect("spec");
        let request = request(
            "fs.write",
            json!({ "path": "file.txt", "content": "replacement" }),
        );
        let prepared = dispatcher
            .prepare_for_test(&request, spec)
            .expect("prepare");
        std::fs::rename(temp.path().join("file.txt"), temp.path().join("old.txt"))
            .expect("move old");
        std::fs::write(temp.path().join("file.txt"), b"other").expect("other");
        dispatcher
            .execute(&request, spec, prepared)
            .expect("replace");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("file.txt")).expect("new"),
            "replacement"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("old.txt")).expect("old"),
            "old"
        );
    }

    #[test]
    fn fs_list_uses_authorized_directory_handle() {
        let temp = TempDir::new("list-handle");
        std::fs::write(temp.path().join("file.txt"), b"file").expect("file");
        let dispatcher = dispatcher(temp.path());
        let outcome = dispatcher
            .dispatch(
                &context(),
                call("fs.list", json!({ "path": "." })),
                &mut RecordingAuditSink::default(),
                &mut AllowAllApprovals,
            )
            .expect("list");
        assert_eq!(outcome.output.content, "file\tfile.txt");
        #[cfg(windows)]
        assert!(
            dispatcher
                .paths()
                .path_capabilities()
                .listing_handle_relative
        );
        #[cfg(not(windows))]
        assert!(
            !dispatcher
                .paths()
                .path_capabilities()
                .listing_handle_relative
        );
    }
}
