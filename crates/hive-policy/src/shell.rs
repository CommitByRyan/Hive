//! Program-level shell policy: disabled by default, and when enabled only
//! normalized absolute program paths from the configured allow-list may run.
//! Shells and interpreters (`cmd`, `powershell`/`pwsh`, `bash`/`sh`, and
//! common script interpreters) are rejected even when configured.

use std::path::{Path, PathBuf};

use crate::PolicyError;

/// Base names that are inherently unsafe to execute, regardless of the
/// configured allow-list. Names are matched case-insensitively and without a
/// trailing `.exe`/`.com`.
const DENIED_PROGRAMS: &[&str] = &[
    "cmd",
    "command",
    "powershell",
    "pwsh",
    "powershell_ise",
    "bash",
    "sh",
    "dash",
    "zsh",
    "fish",
    "csh",
    "tcsh",
    "ksh",
    "python",
    "python3",
    "pythonw",
    "pypy",
    "pypy3",
    "node",
    "nodejs",
    "ruby",
    "perl",
    "php",
    "deno",
    "bun",
    "lua",
    "luajit",
    "wscript",
    "cscript",
    "mshta",
    "regsvr32",
    "rundll32",
];

/// Default-deny shell policy. When disabled every `shell.run` returns
/// `shell.disabled`; when enabled only canonical absolute paths present in the
/// allow-list may spawn, and shells/interpreters stay rejected.
#[derive(Clone, Debug)]
pub struct ShellPolicy {
    enabled: bool,
    allowed_programs: Vec<PathBuf>,
}

impl ShellPolicy {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            allowed_programs: Vec::new(),
        }
    }

    pub fn new(
        enabled: bool,
        allowed: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, PolicyError> {
        let mut allowed_programs = Vec::new();
        for program in allowed {
            allowed_programs.push(canonicalize_program(&program)?);
        }
        Ok(Self {
            enabled,
            allowed_programs,
        })
    }

    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn allowed_programs(&self) -> &[PathBuf] {
        &self.allowed_programs
    }

    /// Authorize a caller-supplied `program`. Returns the canonical absolute
    /// path that must be used for spawn, or a stable policy error.
    pub fn authorize_program(&self, program: &str) -> Result<PathBuf, PolicyError> {
        if !self.enabled {
            return Err(PolicyError::ShellDisabled);
        }
        let path = Path::new(program);
        if !path.is_absolute() {
            return Err(PolicyError::ShellProgramDenied(format!(
                "program must be an absolute path: {program}"
            )));
        }
        let canonical = canonicalize_program(path)?;
        if !self
            .allowed_programs
            .iter()
            .any(|allowed| allowed == &canonical)
        {
            return Err(PolicyError::ShellProgramDenied(format!(
                "program is not in the configured allow-list: {}",
                canonical.display()
            )));
        }
        Ok(canonical)
    }
}

fn canonicalize_program(program: &Path) -> Result<PathBuf, PolicyError> {
    if !program.is_absolute() {
        return Err(PolicyError::ShellProgramDenied(format!(
            "program must be an absolute path: {}",
            program.display()
        )));
    }
    if is_inherently_denied(program) {
        return Err(PolicyError::ShellProgramDenied(format!(
            "shell or interpreter programs are forbidden: {}",
            program.display()
        )));
    }
    let canonical = std::fs::canonicalize(program).map_err(|error| {
        PolicyError::ShellProgramDenied(format!(
            "cannot resolve program {}: {error}",
            program.display()
        ))
    })?;
    if is_inherently_denied(&canonical) {
        return Err(PolicyError::ShellProgramDenied(format!(
            "program resolves to a forbidden shell or interpreter: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn is_inherently_denied(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    let base = lower
        .strip_suffix(".exe")
        .or_else(|| lower.strip_suffix(".com"))
        .unwrap_or(&lower);
    DENIED_PROGRAMS.contains(&base)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "hive-policy-shell-{name}-{}-{counter}",
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

    fn fake_program(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"not a real executable").expect("write program");
        path
    }

    #[test]
    fn disabled_policy_rejects_everything() {
        let policy = ShellPolicy::disabled();
        assert!(!policy.enabled());
        assert_eq!(
            policy
                .authorize_program("C:\\Windows\\System32\\cmd.exe")
                .expect_err("disabled")
                .code(),
            "shell.disabled"
        );
    }

    #[test]
    fn enabled_policy_allows_only_canonical_allow_listed_programs() {
        let temp = TempDir::new("allowed");
        let program = fake_program(temp.path(), "tool.exe");
        let policy = ShellPolicy::new(true, [program.clone()]).expect("policy");

        let authorized = policy
            .authorize_program(program.to_str().expect("utf8"))
            .expect("allow-listed");
        assert!(authorized.is_absolute());

        let other = fake_program(temp.path(), "other.exe");
        assert_eq!(
            policy
                .authorize_program(other.to_str().expect("utf8"))
                .expect_err("not allow-listed")
                .code(),
            "shell.program_denied"
        );
    }

    #[test]
    fn relative_programs_are_denied() {
        let temp = TempDir::new("relative");
        let program = fake_program(temp.path(), "tool.exe");
        let policy = ShellPolicy::new(true, [program]).expect("policy");
        assert_eq!(
            policy
                .authorize_program("tool.exe")
                .expect_err("relative")
                .code(),
            "shell.program_denied"
        );
    }

    #[test]
    fn shells_and_interpreters_are_denied_even_when_allow_listed() {
        let temp = TempDir::new("denied");
        let program = fake_program(temp.path(), "python3.exe");
        let error = ShellPolicy::new(true, [program]).expect_err("interpreter");
        assert_eq!(error.code(), "shell.program_denied");
    }

    #[test]
    fn inherently_denied_names_cover_required_shells() {
        let probe = |name: &str| is_inherently_denied(Path::new(name));
        assert!(probe("cmd.exe"));
        assert!(probe("CMD"));
        assert!(probe("powershell.exe"));
        assert!(probe("pwsh"));
        assert!(probe("bash"));
        assert!(probe("sh"));
        assert!(probe("python.exe"));
        assert!(probe("node"));
        assert!(!probe("tool.exe"));
    }
}
