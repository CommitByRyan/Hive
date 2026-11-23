//! Real `shell.run` execution: argv-only spawn, a minimal cleared environment,
//! bounded stdout/stderr capture, a hard timeout, and whole-process-tree
//! cleanup (Windows Job Objects with `KILL_ON_JOB_CLOSE`, Unix process groups).

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hive_policy::ErrorCode;

use crate::{ToolError, ToolOutput, ToolResult};

/// Abstraction over OS process-tree isolation so tests can inject an
/// assignment failure without a production switch.
trait ProcessTree: Sized {
    fn create() -> ToolResult<Self>;
    fn assign(&mut self, child: &Child) -> ToolResult<()>;
    fn terminate(&self) -> ToolResult<()>;
}

/// Run a program with literal argv (never via a shell), capturing bounded
/// stdout/stderr and enforcing `timeout` with whole-tree cleanup.
pub(crate) fn run(
    program: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    max_output: usize,
) -> ToolResult<ToolOutput> {
    run_with::<ProcessTreeGuard>(program, args, cwd, timeout, max_output)
}

fn run_with<G: ProcessTree>(
    program: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    max_output: usize,
) -> ToolResult<ToolOutput> {
    let mut command = Command::new(program);
    command.args(args);
    command.current_dir(cwd);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    configure_environment(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut guard = G::create()?;
    let mut child = command.spawn().map_err(|error| {
        ToolError::new(
            ErrorCode::ToolExecutionFailed,
            format!("cannot spawn {}: {error}", program.display()),
        )
    })?;
    if let Err(cause) = guard.assign(&child) {
        return cleanup_orphan(&mut child, program, cause);
    }

    let stdout_thread = spawn_reader(child.stdout.take(), max_output);
    let stderr_thread = spawn_reader(child.stderr.take(), max_output);

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut status = None;
    loop {
        match child.try_wait() {
            Ok(Some(code)) => {
                status = Some(code);
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(ToolError::new(
                    ErrorCode::ToolExecutionFailed,
                    format!("cannot wait for {}: {error}", program.display()),
                ));
            }
        }
    }

    if timed_out {
        let killed = guard.terminate().is_ok();
        let reaped = child.wait().is_ok();
        let _ = stdout_thread.join();
        let _ = stderr_thread.join();
        if !killed || !reaped {
            return Err(ToolError::new(
                ErrorCode::ShellCleanupFailed,
                format!(
                    "failed to terminate the process tree of {}",
                    program.display()
                ),
            ));
        }
        return Err(ToolError::new(
            ErrorCode::ShellTimeout,
            format!("{} exceeded the {timeout:?} timeout", program.display()),
        ));
    }

    let (stdout, stdout_truncated) = stdout_thread.join().unwrap_or_else(|_| (Vec::new(), false));
    let (stderr, stderr_truncated) = stderr_thread.join().unwrap_or_else(|_| (Vec::new(), false));

    // Dropping the guard closes the Job Object (KILL_ON_JOB_CLOSE) or leaves
    // the Unix process group to the OS reaper; no orphaned descendants remain.
    drop(guard);

    let truncated = stdout_truncated || stderr_truncated;
    let bytes = stdout.len() + stderr.len();
    let content = combine_output(stdout, stderr);
    let exit_code = status.and_then(|code| code.code());
    let output = ToolOutput {
        content,
        bytes,
        truncated,
        summary: format!(
            "exit_code={} truncated={truncated}",
            exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_owned())
        ),
    };

    match exit_code {
        Some(0) => Ok(output),
        Some(code) => Err(ToolError::new(
            ErrorCode::ToolExecutionFailed,
            format!("{} exited with non-zero code {code}", program.display()),
        )
        .with_exit_code(code)
        .with_output(output)),
        None => Err(ToolError::new(
            ErrorCode::ToolExecutionFailed,
            format!("{} terminated by a signal", program.display()),
        )
        .with_output(output)),
    }
}

/// Fail-closed cleanup for a child that spawned but was never assigned to a
/// Job Object / process group. The child is killed and reaped directly; only
/// then is the original assignment error returned.
fn cleanup_orphan(child: &mut Child, program: &Path, cause: ToolError) -> ToolResult<ToolOutput> {
    let pid = child.id();
    let killed = child.kill().is_ok();
    let reaped = child.wait().is_ok();
    // Close any remaining pipe read ends so no handle outlives the cleanup.
    drop(child.stdin.take());
    drop(child.stdout.take());
    drop(child.stderr.take());
    if !killed || !reaped {
        return Err(ToolError::new(
            ErrorCode::ShellCleanupFailed,
            format!(
                "cannot clean up child process {pid} of {} after assignment failed: {}",
                program.display(),
                cause.message()
            ),
        ));
    }
    Err(ToolError::new(
        cause.error_code(),
        format!(
            "{}: {} (child process {pid} was terminated)",
            cause.code(),
            cause.message()
        ),
    ))
}

fn configure_environment(command: &mut Command) {
    command.env_clear();
    #[cfg(windows)]
    {
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
    }
    #[cfg(unix)]
    {
        command.env("PATH", "/usr/bin:/bin");
        command.env("LC_ALL", "C");
    }
}

fn spawn_reader(
    stream: Option<impl Read + Send + 'static>,
    max_output: usize,
) -> JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let Some(reader) = stream else {
            return (Vec::new(), false);
        };
        read_bounded(reader, max_output)
    })
}

fn read_bounded(reader: impl Read, max_output: usize) -> (Vec<u8>, bool) {
    let mut reader = reader;
    let mut buffer = Vec::with_capacity(max_output.min(64 * 1024));
    let mut truncated = false;
    let mut total = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                total = total.saturating_add(read);
                let room = max_output.saturating_sub(buffer.len());
                let keep = read.min(room);
                buffer.extend_from_slice(&chunk[..keep]);
                if total > max_output {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (buffer, truncated)
}

fn combine_output(stdout: Vec<u8>, stderr: Vec<u8>) -> String {
    let mut out = String::from_utf8_lossy(&stdout).into_owned();
    if !stderr.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&String::from_utf8_lossy(&stderr));
    }
    out
}

/// Owns the OS-level process-tree isolation handle.
struct ProcessTreeGuard {
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(unix)]
    pgid: i32,
}

#[cfg(windows)]
impl ProcessTree for ProcessTreeGuard {
    fn create() -> ToolResult<Self> {
        use std::mem::size_of;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if job.is_null() {
            return Err(ToolError::new(
                ErrorCode::ToolExecutionFailed,
                "cannot create Job Object for shell isolation",
            ));
        }
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            unsafe { CloseHandle(job) };
            return Err(ToolError::new(
                ErrorCode::ToolExecutionFailed,
                "cannot configure Job Object with KILL_ON_JOB_CLOSE",
            ));
        }
        Ok(Self { job })
    }

    fn assign(&mut self, child: &Child) -> ToolResult<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        let ok = unsafe { AssignProcessToJobObject(self.job, child.as_raw_handle() as _) };
        if ok == 0 {
            return Err(ToolError::new(
                ErrorCode::ToolExecutionFailed,
                "cannot assign child process to the Job Object",
            ));
        }
        Ok(())
    }

    fn terminate(&self) -> ToolResult<()> {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        let ok = unsafe { TerminateJobObject(self.job, 1) };
        if ok == 0 {
            return Err(ToolError::new(
                ErrorCode::ShellCleanupFailed,
                "TerminateJobObject failed",
            ));
        }
        Ok(())
    }
}

#[cfg(unix)]
impl ProcessTree for ProcessTreeGuard {
    fn create() -> ToolResult<Self> {
        Ok(Self { pgid: 0 })
    }

    fn assign(&mut self, child: &Child) -> ToolResult<()> {
        self.pgid = child.id() as i32;
        Ok(())
    }

    fn terminate(&self) -> ToolResult<()> {
        let status = Command::new("kill")
            .args(["-9", &format!("-{}", self.pgid)])
            .status();
        match status {
            Ok(_) => Ok(()),
            Err(error) => Err(ToolError::new(
                ErrorCode::ShellCleanupFailed,
                format!("cannot kill process group {}: {error}", self.pgid),
            )),
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        unsafe { CloseHandle(self.job) };
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::process::Child;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use hive_policy::ErrorCode;
    use hive_store::{classify_recovery, RecoveryDecision, SideEffectState, Store};
    use hive_types::{AttemptNo, EventId, EventInput, EventKind, LeaseToken, NodeState};

    use super::{run_with, ProcessTree};
    use crate::{ToolError, ToolResult};

    thread_local! {
        static ASSIGNED_PID: Cell<Option<u32>> = const { Cell::new(None) };
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct FailingAssign;

    impl ProcessTree for FailingAssign {
        fn create() -> ToolResult<Self> {
            Ok(FailingAssign)
        }

        fn assign(&mut self, child: &Child) -> ToolResult<()> {
            ASSIGNED_PID.with(|slot| slot.set(Some(child.id())));
            Err(ToolError::new(
                ErrorCode::ToolExecutionFailed,
                "injected assign failure",
            ))
        }

        fn terminate(&self) -> ToolResult<()> {
            Ok(())
        }
    }

    fn helper_binary() -> PathBuf {
        let name = if cfg!(windows) {
            "hive_shell_helper.exe"
        } else {
            "hive_shell_helper"
        };
        let mut dir = std::env::current_exe()
            .expect("current exe")
            .parent()
            .map(PathBuf::from);
        while let Some(current) = dir {
            let candidate = current.join(name);
            if candidate.exists() {
                return candidate;
            }
            dir = current.parent().map(PathBuf::from);
        }
        panic!("hive_shell_helper binary not found");
    }

    #[cfg(windows)]
    fn process_alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE as u32
        }
    }

    #[cfg(not(windows))]
    fn process_alive(pid: u32) -> bool {
        std::fs::metadata(format!("/proc/{pid}")).is_ok()
    }

    #[test]
    fn uncertain_shell_completion_requires_recovery() {
        assert_eq!(
            classify_recovery(&[SideEffectState::Intent]),
            RecoveryDecision::SafeToRetry
        );
        assert_eq!(
            classify_recovery(&[SideEffectState::Intent, SideEffectState::Started]),
            RecoveryDecision::NeedsRecovery
        );

        let mut store = Store::open_in_memory().expect("store");
        let mission = store.create_mission("m", "g", 0).expect("mission");
        let run = store.create_run(mission.mission_id, 0).expect("run");
        let node = store
            .add_node(mission.mission_id, run.run_id, 3, 0)
            .expect("node");

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

        let started = EventInput {
            event_id: EventId::new(),
            kind: EventKind::NodeToolStarted,
            mission_id: mission.mission_id,
            run_id: Some(run.run_id),
            node_id: Some(node.node_id),
            session_id: None,
            attempt_no: Some(AttemptNo::FIRST),
            trace_id: None,
            payload: serde_json::json!({ "tool_name": "shell.run" }),
        };
        store.append_event(&started).expect("started event");

        let reclaimed = store.reclaim_expired_leases(2_000).expect("reclaim");
        let reclaimed_node = reclaimed
            .into_iter()
            .find(|record| record.node_id == node.node_id)
            .expect("reclaimed node");
        assert_eq!(reclaimed_node.state, NodeState::NeedsRecovery);
    }

    #[test]
    fn assign_failure_kills_spawned_child_without_orphan() {
        let helper = helper_binary();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "hive-tools-orphan-{}-{counter}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");

        let args = vec!["sleep".to_owned(), "60000".to_owned()];
        let result = run_with::<FailingAssign>(&helper, &args, &dir, Duration::from_secs(5), 1024);
        assert!(result.is_err());
        assert_eq!(result.as_ref().unwrap_err().code(), "tool.execution_failed");

        let pid = ASSIGNED_PID.with(|slot| slot.get()).expect("pid recorded");
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !process_alive(pid),
            "child process {pid} must be killed after assign failure"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
