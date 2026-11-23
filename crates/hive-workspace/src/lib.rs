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
        run_git(dir, &["add", "README.md"]);
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
}
