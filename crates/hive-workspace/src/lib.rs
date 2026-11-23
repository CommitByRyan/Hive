//! Worktree lifecycle management for Hive build agents.
//!
//! Thin wrapper over `git worktree` invoked through `std::process::Command`
//! with argv arrays (never a shell string). Every managed worktree lives under
//! `base_dir` (default `<project_root>/.hive/worktrees`) so a node can only
//! touch its own directory and removals cannot escape the worktree root.

use std::path::{Path, PathBuf};
use std::process::Command;

use hive_types::NodeId;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("git failed: {message}")]
    Git { message: String },
    #[error("worktree already exists for `{0}`")]
    AlreadyExists(String),
    #[error("path `{path}` is outside the worktree base directory")]
    PathOutsideBaseDir { path: String },
    #[error("refusing to treat the project root `{0}` as a worktree")]
    ProjectRootAsWorktree(String),
    #[error("project root `{0}` is dirty; merge refused")]
    DirtyProjectRoot(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug)]
pub struct WorktreeManager {
    project_root: PathBuf,
    base_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorktreeHandle {
    pub branch: String,
    pub path: PathBuf,
    pub node_id: Option<NodeId>,
}

/// Result of `git status --porcelain` for a checkout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeStatus {
    pub dirty: bool,
}

/// Whether an integration merge would apply cleanly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergePreview {
    /// The merge is clean and would produce this tree object id.
    Clean { tree: String },
    /// The merge would conflict on these paths.
    Conflicts { paths: Vec<String> },
}

/// Result of attempting a real merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The merge completed; `commit` is the new HEAD sha.
    Merged { commit: String },
    /// The merge was not attempted because it would conflict.
    Conflicts { paths: Vec<String> },
}

/// Policy controlling what `recycle` does with a dirty worktree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecyclePolicy {
    pub keep_dirty: bool,
}

/// Outcome of recycling a worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecycleOutcome {
    KeptDirty,
    Removed,
}

impl WorktreeManager {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        let project_root = project_root.into();
        let base_dir = project_root.join(".hive").join("worktrees");
        Self {
            project_root,
            base_dir,
        }
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Create a worktree for `node` at `hive/<node-slug>` on top of
    /// `base_ref`, returning a handle to the new checkout.
    pub fn create(&self, node: NodeId, base_ref: &str) -> Result<WorktreeHandle, WorkspaceError> {
        let slug = node_slug(node);
        let branch = format!("hive/{slug}");
        let path = self.base_dir.join(&slug);
        std::fs::create_dir_all(&self.base_dir)?;
        self.ensure_safe_target(&path)?;

        let output = Command::new("git")
            .arg("-C")
            .arg(&self.project_root)
            .arg("worktree")
            .arg("add")
            .arg("-b")
            .arg(&branch)
            .arg(&path)
            .arg(base_ref)
            .output()?;

        if output.status.success() {
            return Ok(WorktreeHandle {
                branch,
                path,
                node_id: Some(node),
            });
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if stderr.contains("already exists") || stderr.contains("already checked out") {
            return Err(WorkspaceError::AlreadyExists(branch));
        }
        Err(WorkspaceError::Git { message: stderr })
    }

    /// List managed worktrees under `base_dir`.
    pub fn list(&self) -> Result<Vec<WorktreeHandle>, WorkspaceError> {
        if !self.base_dir.exists() {
            return Ok(Vec::new());
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.project_root)
            .arg("worktree")
            .arg("list")
            .arg("--porcelain")
            .output()?;
        if !output.status.success() {
            return Err(WorkspaceError::Git {
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        parse_worktree_list(&String::from_utf8_lossy(&output.stdout), &self.base_dir)
    }

    /// Remove a worktree. A dirty working tree is rejected unless `force` is
    /// explicitly set.
    pub fn remove(&self, handle: &WorktreeHandle, force: bool) -> Result<(), WorkspaceError> {
        self.ensure_safe_target(&handle.path)?;
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.project_root)
            .arg("worktree")
            .arg("remove");
        if force {
            command.arg("--force");
        }
        let output = command.arg(&handle.path).output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(WorkspaceError::Git {
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        }
    }

    /// Resolve the current HEAD commit of a worktree to a 40/64 hex sha.
    pub fn head_revision(&self, handle: &WorktreeHandle) -> Result<String, WorkspaceError> {
        head_revision_at(&handle.path)
    }

    /// Read the dirty state of a worktree.
    pub fn status(&self, handle: &WorktreeHandle) -> Result<WorktreeStatus, WorkspaceError> {
        status_at(&handle.path)
    }

    /// Preview a merge of `branch` into `target_ref` without touching the
    /// working tree or creating a merge commit.
    pub fn preview_merge(
        &self,
        target_ref: &str,
        branch: &str,
    ) -> Result<MergePreview, WorkspaceError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.project_root)
            .arg("merge-tree")
            .arg("--write-tree")
            .arg(target_ref)
            .arg(branch)
            .output()?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        match output.status.code() {
            Some(0) => {
                let tree = stdout.lines().next().unwrap_or_default().trim().to_owned();
                if tree.is_empty() {
                    return Err(WorkspaceError::Git {
                        message: "merge-tree returned no tree".to_owned(),
                    });
                }
                Ok(MergePreview::Clean { tree })
            }
            Some(1) => {
                let tree = stdout.lines().next().unwrap_or_default().trim().to_owned();
                if !is_hex_sha(&tree) {
                    return Err(WorkspaceError::Git {
                        message: if stderr.is_empty() {
                            "merge-tree failed".to_owned()
                        } else {
                            stderr
                        },
                    });
                }
                let paths = parse_conflict_paths(&stdout)?;
                Ok(MergePreview::Conflicts { paths })
            }
            _ => Err(WorkspaceError::Git {
                message: if stderr.is_empty() {
                    format!("merge-tree exited with {}", output.status)
                } else {
                    stderr
                },
            }),
        }
    }

    /// Merge `branch` into the checked-out branch of `project_root`.
    ///
    /// Refuses a dirty project root and returns without touching the target
    /// when the preview detects conflicts. Never uses `--force`, `reset`, or
    /// automatic conflict resolution.
    pub fn merge(
        &self,
        target_ref: &str,
        branch: &str,
        message: &str,
    ) -> Result<MergeOutcome, WorkspaceError> {
        if status_at(&self.project_root)?.dirty {
            return Err(WorkspaceError::DirtyProjectRoot(
                self.project_root.display().to_string(),
            ));
        }
        if let MergePreview::Conflicts { paths } = self.preview_merge(target_ref, branch)? {
            return Ok(MergeOutcome::Conflicts { paths });
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.project_root)
            .arg("merge")
            .arg("--no-ff")
            .arg("-m")
            .arg(message)
            .arg(branch)
            .output()?;
        if !output.status.success() {
            return Err(WorkspaceError::Git {
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        let commit = head_revision_at(&self.project_root)?;
        Ok(MergeOutcome::Merged { commit })
    }

    /// Recycle a worktree. A dirty worktree is kept when the policy requests
    /// it; otherwise it is removed (forcing removal of dirty content).
    pub fn recycle(
        &self,
        handle: &WorktreeHandle,
        policy: RecyclePolicy,
    ) -> Result<RecycleOutcome, WorkspaceError> {
        let status = self.status(handle)?;
        if status.dirty && policy.keep_dirty {
            return Ok(RecycleOutcome::KeptDirty);
        }
        self.remove(handle, status.dirty)?;
        Ok(RecycleOutcome::Removed)
    }

    fn ensure_safe_target(&self, path: &Path) -> Result<(), WorkspaceError> {
        if path == self.project_root {
            return Err(WorkspaceError::ProjectRootAsWorktree(
                self.project_root.display().to_string(),
            ));
        }
        let base = self.base_dir.canonicalize()?;
        if !canonical_target(path).starts_with(&base) {
            return Err(WorkspaceError::PathOutsideBaseDir {
                path: path.display().to_string(),
            });
        }
        Ok(())
    }
}

fn node_slug(node: NodeId) -> String {
    node.prefixed()
}

/// Resolve `path` to an absolute, separator-normalized form even when it does
/// not exist yet, by canonicalizing the deepest existing ancestor.
fn canonical_target(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let file_name = path.file_name().map(PathBuf::from).unwrap_or_default();
    match path.parent() {
        Some(parent) => parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(file_name),
        None => path.to_path_buf(),
    }
}

/// Resolve `git -C <path> rev-parse HEAD` to a 40/64 hex sha.
pub fn head_revision_at(path: &Path) -> Result<String, WorkspaceError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()?;
    if !output.status.success() {
        return Err(WorkspaceError::Git {
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if is_hex_sha(&sha) {
        Ok(sha)
    } else {
        Err(WorkspaceError::Git {
            message: format!("rev-parse HEAD returned invalid sha `{sha}`"),
        })
    }
}

/// Parse `git status --porcelain` for a checkout into its dirty state.
fn status_at(path: &Path) -> Result<WorktreeStatus, WorkspaceError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("status")
        .arg("--porcelain")
        .output()?;
    if !output.status.success() {
        return Err(WorkspaceError::Git {
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let dirty = stdout.lines().any(|line| !line.trim().is_empty());
    Ok(WorktreeStatus { dirty })
}

/// Extract the conflicted paths from `git merge-tree --write-tree` output.
///
/// The first line is the resulting tree object id; conflicted file info
/// follows as `<mode> <oid> <stage>\t<path>` lines until a blank line, after
/// which informational messages may appear.
fn parse_conflict_paths(stdout: &str) -> Result<Vec<String>, WorkspaceError> {
    let mut paths = Vec::new();
    for line in stdout.lines().skip(1) {
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((_, path)) = line.split_once('\t') {
            let path = path.trim_end();
            if !path.is_empty() && !paths.iter().any(|existing| existing == path) {
                paths.push(path.to_owned());
            }
        }
    }
    if paths.is_empty() {
        return Err(WorkspaceError::Git {
            message: "merge-tree reported conflicts but produced no paths".to_owned(),
        });
    }
    Ok(paths)
}

fn is_hex_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse_worktree_list(
    output: &str,
    base_dir: &Path,
) -> Result<Vec<WorktreeHandle>, WorkspaceError> {
    let mut handles = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;

    fn flush(
        path: &mut Option<PathBuf>,
        branch: &mut Option<String>,
        base_dir: &Path,
        handles: &mut Vec<WorktreeHandle>,
    ) {
        let Some(path) = path.take() else {
            return;
        };
        let branch = branch.take().unwrap_or_default();
        if path.starts_with(base_dir) {
            let node_id = branch
                .strip_prefix("refs/heads/hive/")
                .and_then(|slug| NodeId::parse(slug).ok());
            handles.push(WorktreeHandle {
                branch,
                path,
                node_id,
            });
        }
    }

    for line in output.lines() {
        if let Some(value) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch, base_dir, &mut handles);
            path = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("branch ") {
            branch = Some(value.to_owned());
        }
    }
    flush(&mut path, &mut branch, base_dir, &mut handles);
    Ok(handles)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let mut dir = std::env::temp_dir();
            dir.push(format!("hive-workspace-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
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

    fn run_git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("README.md"), b"base\n").expect("write");
        std::fs::write(dir.join(".gitignore"), b"/.hive/\n").expect("gitignore");
        run_git(dir, &["add", "README.md", ".gitignore"]);
        run_git(dir, &["commit", "-q", "-m", "init"]);
    }

    #[test]
    fn creates_two_independent_worktrees() {
        let dir = TempDir::new("two");
        init_repo(dir.path());
        let manager = WorktreeManager::new(dir.path());

        let a = manager.create(NodeId::new(), "HEAD").expect("a");
        let b = manager.create(NodeId::new(), "HEAD").expect("b");
        std::fs::write(a.path.join("a.txt"), b"from a").expect("write a");
        std::fs::write(b.path.join("b.txt"), b"from b").expect("write b");

        assert!(a.path.join("a.txt").exists());
        assert!(!a.path.join("b.txt").exists());
        assert!(b.path.join("b.txt").exists());
        assert!(!b.path.join("a.txt").exists());
        assert_eq!(
            a.node_id.as_ref().map(|node| node.prefixed()).as_deref(),
            a.branch.strip_prefix("hive/")
        );

        let listed = manager.list().expect("list");
        assert_eq!(listed.len(), 2);
        let listed_paths: Vec<PathBuf> = listed.iter().map(|handle| handle.path.clone()).collect();
        assert!(listed_paths.contains(&a.path));
        assert!(listed_paths.contains(&b.path));
    }

    #[test]
    fn create_same_node_twice_is_rejected() {
        let dir = TempDir::new("dup");
        init_repo(dir.path());
        let manager = WorktreeManager::new(dir.path());
        let node = NodeId::new();

        manager.create(node, "HEAD").expect("first");
        let error = manager.create(node, "HEAD").expect_err("second");
        assert!(matches!(error, WorkspaceError::AlreadyExists(_)));
    }

    #[test]
    fn remove_rejects_dirty_worktree_without_force() {
        let dir = TempDir::new("dirty");
        init_repo(dir.path());
        let manager = WorktreeManager::new(dir.path());
        let handle = manager.create(NodeId::new(), "HEAD").expect("create");
        std::fs::write(handle.path.join("dirty.txt"), b"untracked").expect("dirty");

        assert!(manager.remove(&handle, false).is_err());
        assert!(
            handle.path.exists(),
            "dirty worktree must survive a non-force remove"
        );

        manager.remove(&handle, true).expect("force remove");
        assert!(!handle.path.exists());
    }

    #[test]
    fn remove_rejects_path_outside_base_dir() {
        let dir = TempDir::new("escape");
        init_repo(dir.path());
        let manager = WorktreeManager::new(dir.path());
        manager.create(NodeId::new(), "HEAD").expect("create base");

        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("outside dir");
        let handle = WorktreeHandle {
            branch: "hive/n_outside".to_owned(),
            path: outside,
            node_id: None,
        };
        let error = manager.remove(&handle, false).expect_err("escape");
        assert!(matches!(error, WorkspaceError::PathOutsideBaseDir { .. }));
    }

    #[test]
    fn remove_rejects_project_root_as_worktree() {
        let dir = TempDir::new("root");
        init_repo(dir.path());
        let manager = WorktreeManager::new(dir.path());
        let handle = WorktreeHandle {
            branch: "hive/n_root".to_owned(),
            path: dir.path().to_path_buf(),
            node_id: None,
        };
        let error = manager.remove(&handle, false).expect_err("root");
        assert!(matches!(error, WorkspaceError::ProjectRootAsWorktree(_)));
    }

    #[test]
    fn head_revision_returns_hex_sha() {
        let dir = TempDir::new("head-sha");
        init_repo(dir.path());
        let manager = WorktreeManager::new(dir.path());
        let handle = manager.create(NodeId::new(), "HEAD").expect("create");

        let sha = manager.head_revision(&handle).expect("head");
        assert!(is_hex_sha(&sha), "expected 40/64 hex sha, got `{sha}`");
    }

    #[test]
    fn two_worktrees_merge_cleanly_when_files_differ() {
        let dir = TempDir::new("merge-clean");
        init_repo(dir.path());
        let manager = WorktreeManager::new(dir.path());

        let a = manager.create(NodeId::new(), "HEAD").expect("a");
        let b = manager.create(NodeId::new(), "HEAD").expect("b");
        std::fs::write(a.path.join("a.txt"), b"from a").expect("write a");
        run_git(&a.path, &["add", "a.txt"]);
        run_git(&a.path, &["commit", "-q", "-m", "a adds a.txt"]);
        std::fs::write(b.path.join("b.txt"), b"from b").expect("write b");
        run_git(&b.path, &["add", "b.txt"]);
        run_git(&b.path, &["commit", "-q", "-m", "b adds b.txt"]);

        let outcome_a = manager
            .merge("HEAD", &a.branch, "merge a")
            .expect("merge a");
        assert!(matches!(outcome_a, MergeOutcome::Merged { .. }));
        let outcome_b = manager
            .merge("HEAD", &b.branch, "merge b")
            .expect("merge b");
        assert!(matches!(outcome_b, MergeOutcome::Merged { .. }));

        assert!(dir.path().join("a.txt").exists());
        assert!(dir.path().join("b.txt").exists());
    }

    #[test]
    fn preview_merge_reports_conflict_and_does_not_touch_target() {
        let dir = TempDir::new("preview-conflict");
        init_repo(dir.path());
        let manager = WorktreeManager::new(dir.path());
        let handle = manager.create(NodeId::new(), "HEAD").expect("create");

        std::fs::write(dir.path().join("README.md"), b"main side\n").expect("main write");
        run_git(dir.path(), &["add", "README.md"]);
        run_git(dir.path(), &["commit", "-q", "-m", "main edits readme"]);
        std::fs::write(handle.path.join("README.md"), b"feature side\n").expect("feat write");
        run_git(&handle.path, &["add", "README.md"]);
        run_git(
            &handle.path,
            &["commit", "-q", "-m", "feature edits readme"],
        );

        let before = head_revision_at(dir.path()).expect("before");
        let preview = manager
            .preview_merge("HEAD", &handle.branch)
            .expect("preview");
        assert!(matches!(preview, MergePreview::Conflicts { .. }));
        if let MergePreview::Conflicts { paths } = preview {
            assert!(paths.iter().any(|path| path.ends_with("README.md")));
        }

        assert_eq!(head_revision_at(dir.path()).expect("after"), before);
        assert!(!status_at(dir.path()).expect("status").dirty);
    }

    #[test]
    fn merge_refuses_dirty_project_root() {
        let dir = TempDir::new("merge-dirty");
        init_repo(dir.path());
        let manager = WorktreeManager::new(dir.path());
        let handle = manager.create(NodeId::new(), "HEAD").expect("create");
        std::fs::write(handle.path.join("x.txt"), b"x").expect("x");
        run_git(&handle.path, &["add", "x.txt"]);
        run_git(&handle.path, &["commit", "-q", "-m", "x"]);

        std::fs::write(dir.path().join("dirty.txt"), b"untracked").expect("dirty");
        let error = manager
            .merge("HEAD", &handle.branch, "merge")
            .expect_err("dirty");
        assert!(matches!(error, WorkspaceError::DirtyProjectRoot(_)));
        assert!(!dir.path().join("x.txt").exists());
    }

    #[test]
    fn recycle_keeps_dirty_worktree_when_policy_says_so() {
        let dir = TempDir::new("recycle-keep");
        init_repo(dir.path());
        let manager = WorktreeManager::new(dir.path());
        let handle = manager.create(NodeId::new(), "HEAD").expect("create");
        std::fs::write(handle.path.join("dirty.txt"), b"untracked").expect("dirty");

        let outcome = manager
            .recycle(&handle, RecyclePolicy { keep_dirty: true })
            .expect("recycle keep");
        assert_eq!(outcome, RecycleOutcome::KeptDirty);
        assert!(handle.path.exists(), "dirty worktree must be kept");

        let outcome = manager
            .recycle(&handle, RecyclePolicy { keep_dirty: false })
            .expect("recycle remove");
        assert_eq!(outcome, RecycleOutcome::Removed);
        assert!(!handle.path.exists());
    }
}
