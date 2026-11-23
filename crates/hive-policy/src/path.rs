use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use hive_platform::PathPlatform;

use crate::PolicyError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathAccess {
    Read,
    Write,
    List,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedPath {
    root: PathBuf,
    relative: PathBuf,
    final_path: PathBuf,
}

impl AuthorizedPath {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn relative(&self) -> &Path {
        &self.relative
    }

    pub fn final_path(&self) -> &Path {
        &self.final_path
    }
}

/// A handle-level authorized path. On Windows this wraps the exact handle the
/// platform traversal opened; callers must not re-open the path by string.
#[derive(Debug)]
pub struct HandleAuthorizedPath {
    root: PathBuf,
    relative: PathBuf,
    handle: hive_platform::PathHandle,
}

impl HandleAuthorizedPath {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn relative(&self) -> &Path {
        &self.relative
    }

    pub fn final_path(&self) -> &Path {
        self.handle.final_path()
    }

    pub fn is_dir(&self) -> bool {
        self.handle.is_dir()
    }

    pub fn into_platform_handle(self) -> hive_platform::PathHandle {
        self.handle
    }
}

fn map_platform_error(error: hive_platform::PlatformError) -> PolicyError {
    use hive_platform::PlatformError;
    match error {
        PlatformError::ReparsePoint(message) => PolicyError::PathReparseEscape(message),
        PlatformError::OutsideRoot(message) => PolicyError::PathOutsideWorkspace(message),
        PlatformError::CrossVolume(message) => PolicyError::PathOutsideWorkspace(message),
        PlatformError::NotFound(message) => PolicyError::PathNotFound(message),
        PlatformError::Unsupported(message) => PolicyError::PathUnsupportedForm(message),
        PlatformError::Io { context, source } => {
            PolicyError::PathUnsupportedForm(format!("{context}: {source}"))
        }
    }
}

/// Application-layer path policy.
///
/// Authorization canonicalizes the target (or its nearest existing ancestor
/// for writes) and compares directory components with the registered root.
/// This closes lexical traversal and simple symlink/junction escapes, but a
/// concurrent swap between this check and the caller's open leaves a TOCTOU
/// window. It is not handle-level isolation.
#[derive(Clone, Debug)]
pub struct PathPolicy {
    roots: Vec<PathBuf>,
}

impl PathPolicy {
    pub fn new<I, P>(roots: I) -> Result<Self, PolicyError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut canonical_roots = Vec::new();
        for root in roots {
            let root = root.as_ref();
            if !root.is_absolute() {
                return Err(PolicyError::PathUnsupportedForm(format!(
                    "workspace root must be absolute: {}",
                    root.display()
                )));
            }
            let canonical = std::fs::canonicalize(root).map_err(|error| {
                PolicyError::PathUnsupportedForm(format!(
                    "cannot resolve workspace root {}: {error}",
                    root.display()
                ))
            })?;
            if !canonical.is_dir() {
                return Err(PolicyError::PathUnsupportedForm(format!(
                    "workspace root is not a directory: {}",
                    canonical.display()
                )));
            }
            canonical_roots.push(canonical);
        }
        if canonical_roots.is_empty() {
            return Err(PolicyError::PathUnsupportedForm(
                "at least one workspace root is required".to_owned(),
            ));
        }
        Ok(Self {
            roots: canonical_roots,
        })
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn root(&self, index: usize) -> Result<&Path, PolicyError> {
        self.roots.get(index).map(PathBuf::as_path).ok_or_else(|| {
            PolicyError::PathUnsupportedForm(format!("unknown workspace root index {index}"))
        })
    }

    pub fn authorize(
        &self,
        root_index: usize,
        input: &str,
        access: PathAccess,
    ) -> Result<AuthorizedPath, PolicyError> {
        let relative = validate_relative(input)?;
        let root = self.root(root_index)?;
        let joined = root.join(&relative);
        let final_path = match access {
            PathAccess::Read | PathAccess::List => canonicalize_existing(&joined)?,
            PathAccess::Write => canonicalize_for_write(&joined)?,
        };
        if !is_within(root, &final_path) {
            return Err(PolicyError::PathOutsideWorkspace(format!(
                "{} resolves outside {}",
                input,
                root.display()
            )));
        }
        Ok(AuthorizedPath {
            root: root.to_path_buf(),
            relative,
            final_path,
        })
    }

    /// Authorize the registered root itself. Only callers that explicitly
    /// need a root-scoped operation (for example listing the workspace top
    /// level) should use this; file tools must not treat it as a file path.
    pub fn authorize_root(&self, root_index: usize) -> Result<AuthorizedPath, PolicyError> {
        let root = self.root(root_index)?;
        Ok(AuthorizedPath {
            root: root.to_path_buf(),
            relative: PathBuf::new(),
            final_path: root.to_path_buf(),
        })
    }

    /// Report the platform's handle-level path capability without asserting
    /// guarantees the underlying OS does not provide.
    pub fn path_capabilities(&self) -> hive_platform::PathCapabilities {
        hive_platform::path_capabilities()
    }

    /// Authorize a relative path through the platform's handle traversal.
    ///
    /// On Windows this opens every component with
    /// `FILE_FLAG_OPEN_REPARSE_POINT`, rejects reparse points, verifies the
    /// final path with `GetFinalPathNameByHandleW` and rejects cross-volume or
    /// short-name aliases. On other platforms it falls back to canonicalization
    /// and reports the reduced capability.
    pub fn authorize_handle(
        &self,
        root_index: usize,
        input: &str,
        access: PathAccess,
    ) -> Result<HandleAuthorizedPath, PolicyError> {
        let relative = validate_relative(input)?;
        let root = self.root(root_index)?;
        let platform = hive_platform::default_path_platform();
        let root_handle = platform.open_root(root).map_err(map_platform_error)?;

        let handle = match access {
            PathAccess::Write => {
                match platform.open_checked(&root_handle, &relative, hive_platform::OpenMode::Write)
                {
                    Ok(handle) => handle,
                    Err(hive_platform::PlatformError::NotFound(_)) => platform
                        .create_file(&root_handle, &relative)
                        .map_err(map_platform_error)?,
                    Err(error) => return Err(map_platform_error(error)),
                }
            }
            PathAccess::Read | PathAccess::List => platform
                .open_checked(&root_handle, &relative, hive_platform::OpenMode::Read)
                .map_err(map_platform_error)?,
        };

        Ok(HandleAuthorizedPath {
            root: root.to_path_buf(),
            relative,
            handle,
        })
    }
}

fn validate_relative(input: &str) -> Result<PathBuf, PolicyError> {
    if input.is_empty() {
        return Err(PolicyError::PathUnsupportedForm(
            "empty path is not a relative file path".to_owned(),
        ));
    }
    if input.contains('\0') {
        return Err(PolicyError::PathUnsupportedForm(
            "NUL is not allowed in paths".to_owned(),
        ));
    }
    if input.starts_with("\\\\") || input.starts_with("//") {
        return Err(PolicyError::PathUnsupportedForm(format!(
            "UNC and device paths are not allowed: {input}"
        )));
    }
    if input.starts_with('\\') || input.starts_with('/') {
        return Err(PolicyError::PathUnsupportedForm(format!(
            "absolute paths are not allowed: {input}"
        )));
    }

    let bytes = input.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(PolicyError::PathUnsupportedForm(format!(
            "drive-qualified paths are not allowed: {input}"
        )));
    }
    if input.contains(':') {
        return Err(PolicyError::PathAdsForbidden(format!(
            "alternate data stream separator is not allowed: {input}"
        )));
    }

    let mut relative = PathBuf::new();
    let mut saw_segment = false;
    for segment in input.split(['/', '\\']) {
        if segment.is_empty() {
            return Err(PolicyError::PathUnsupportedForm(format!(
                "empty path segment in {input}"
            )));
        }
        if segment == ".." {
            return Err(PolicyError::PathOutsideWorkspace(format!(
                "parent traversal is not allowed: {input}"
            )));
        }
        if segment == "." {
            return Err(PolicyError::PathUnsupportedForm(format!(
                "ambiguous dot segment in {input}"
            )));
        }
        if segment.ends_with('.') || segment.ends_with(' ') {
            return Err(PolicyError::PathUnsupportedForm(format!(
                "trailing dot or space is not allowed: {segment}"
            )));
        }
        if segment
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '<' | '>' | '"' | '|' | '?' | '*'))
        {
            return Err(PolicyError::PathUnsupportedForm(format!(
                "unsupported character in path segment: {segment}"
            )));
        }
        if segment.len() > 255 {
            return Err(PolicyError::PathUnsupportedForm(
                "path segment exceeds 255 bytes".to_owned(),
            ));
        }
        if is_reserved_dos_name(segment) {
            return Err(PolicyError::PathUnsupportedForm(format!(
                "reserved DOS device name is not allowed: {segment}"
            )));
        }
        relative.push(segment);
        saw_segment = true;
    }
    if !saw_segment {
        return Err(PolicyError::PathUnsupportedForm(format!(
            "no path segment in {input}"
        )));
    }
    Ok(relative)
}

fn is_reserved_dos_name(segment: &str) -> bool {
    let base = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    if matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let bytes = base.as_bytes();
    bytes.len() == 4
        && (base.starts_with("COM") || base.starts_with("LPT"))
        && bytes[3].is_ascii_digit()
        && bytes[3] != b'0'
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf, PolicyError> {
    std::fs::canonicalize(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            PolicyError::PathNotFound(path.display().to_string())
        } else {
            PolicyError::PathUnsupportedForm(format!("cannot resolve {}: {error}", path.display()))
        }
    })
}

fn canonicalize_for_write(path: &Path) -> Result<PathBuf, PolicyError> {
    if path.exists() {
        return canonicalize_existing(path);
    }

    let mut ancestor = path.to_path_buf();
    let mut missing: Vec<OsString> = Vec::new();
    while !ancestor.exists() {
        let name = ancestor.file_name().ok_or_else(|| {
            PolicyError::PathUnsupportedForm(format!(
                "cannot find an existing ancestor for {}",
                path.display()
            ))
        })?;
        missing.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .ok_or_else(|| {
                PolicyError::PathUnsupportedForm(format!(
                    "cannot find an existing ancestor for {}",
                    path.display()
                ))
            })?
            .to_path_buf();
    }

    let mut resolved = canonicalize_existing(&ancestor)?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

/// Compare canonical paths by directory components. `C:\root2` is not inside
/// `C:\root`. Windows comparisons are case-insensitive.
pub fn is_within(root: &Path, candidate: &Path) -> bool {
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    let mut candidate_components = candidate.components();
    for root_component in root.components() {
        let Some(candidate_component) = candidate_components.next() else {
            return false;
        };
        if !components_equal(root_component, candidate_component) {
            return false;
        }
    }
    true
}

fn components_equal(left: Component<'_>, right: Component<'_>) -> bool {
    #[cfg(windows)]
    {
        left.as_os_str().to_string_lossy().to_lowercase()
            == right.as_os_str().to_string_lossy().to_lowercase()
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::ErrorCode;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "hive-policy-{name}-{}-{counter}",
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

    fn policy(root: &Path) -> PathPolicy {
        PathPolicy::new([root]).expect("policy")
    }

    #[test]
    fn policy_rejects_parent_traversal_and_absolute_forms() {
        let temp = TempDir::new("traversal");
        let policy = policy(temp.path());

        assert_eq!(
            policy
                .authorize(0, "../outside", PathAccess::Read)
                .expect_err("traversal")
                .code(),
            "path.outside_workspace"
        );
        assert_eq!(
            policy
                .authorize(0, "C:\\Windows\\win.ini", PathAccess::Read)
                .expect_err("absolute")
                .code(),
            "path.unsupported_form"
        );
        assert_eq!(
            policy
                .authorize(0, "\\\\server\\share\\file", PathAccess::Read)
                .expect_err("unc")
                .code(),
            "path.unsupported_form"
        );
        assert_eq!(
            policy
                .authorize(0, "C:relative", PathAccess::Read)
                .expect_err("drive relative")
                .code(),
            "path.unsupported_form"
        );
    }

    #[test]
    fn policy_rejects_ads_and_reserved_names() {
        let temp = TempDir::new("ads");
        let policy = policy(temp.path());

        assert_eq!(
            policy
                .authorize(0, "file.txt:secret", PathAccess::Write)
                .expect_err("ads")
                .code(),
            "path.ads_forbidden"
        );
        assert_eq!(
            policy
                .authorize(0, "CON.txt", PathAccess::Write)
                .expect_err("reserved")
                .code(),
            "path.unsupported_form"
        );
        assert_eq!(
            policy
                .authorize(0, "name. ", PathAccess::Write)
                .expect_err("trailing")
                .code(),
            "path.unsupported_form"
        );
    }

    #[test]
    fn segment_boundary_keeps_sibling_root_out() {
        let temp = TempDir::new("boundary");
        let root = temp.path().join("root");
        let sibling = temp.path().join("root2");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::create_dir_all(&sibling).expect("sibling");

        let root = std::fs::canonicalize(&root).expect("canonical root");
        let sibling = std::fs::canonicalize(&sibling).expect("canonical sibling");
        assert!(is_within(&root, &root));
        assert!(!is_within(&root, &sibling));
        assert!(!is_within(&root, &root.join("..").join("root2")));
    }

    #[test]
    fn canonical_read_accepts_inside_file_and_case_variant() {
        let temp = TempDir::new("inside");
        std::fs::write(temp.path().join("file.txt"), b"hello").expect("write");
        let policy = policy(temp.path());

        let exact = policy
            .authorize(0, "file.txt", PathAccess::Read)
            .expect("exact");
        assert!(exact.final_path().ends_with("file.txt"));

        #[cfg(windows)]
        {
            let variant = policy
                .authorize(0, "FILE.TXT", PathAccess::Read)
                .expect("case variant");
            assert_eq!(variant.final_path(), exact.final_path());
        }
    }

    #[test]
    fn write_authorizes_missing_leaf_under_existing_parent() {
        let temp = TempDir::new("write");
        let policy = policy(temp.path());
        let authorized = policy
            .authorize(0, "nested/new.txt", PathAccess::Write)
            .expect("missing leaf");
        assert!(authorized.final_path().ends_with("new.txt"));
        assert!(authorized.final_path().to_string_lossy().contains("nested"));
        assert!(is_within(authorized.root(), authorized.final_path()));
    }

    #[cfg(windows)]
    fn try_create_junction(link: &Path, target: &Path) -> bool {
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        matches!(status, Ok(status) if status.success())
    }

    #[test]
    #[cfg(windows)]
    fn policy_rejects_symlink_escape() {
        let outside = TempDir::new("symlink-outside");
        std::fs::write(outside.path().join("secret.txt"), b"secret").expect("secret");
        let temp = TempDir::new("symlink");
        let link = temp.path().join("link");
        if !try_create_junction(&link, outside.path()) {
            eprintln!(
                "SKIP: junction creation unavailable (needs NTFS); platform handle defense not exercised"
            );
            return;
        }

        let policy = policy(temp.path());
        let error = policy
            .authorize_handle(0, "link/secret.txt", PathAccess::Read)
            .expect_err("escape");
        assert_eq!(error.code(), "path.reparse_escape");

        let _ = std::fs::remove_dir(&link);
    }

    #[test]
    #[cfg(windows)]
    fn open_handle_detects_toctou_swap() {
        let outside = TempDir::new("toctou-outside");
        std::fs::write(outside.path().join("secret.txt"), b"secret").expect("secret");
        let temp = TempDir::new("toctou");
        std::fs::create_dir(temp.path().join("dir")).expect("dir");
        std::fs::write(temp.path().join("dir").join("file.txt"), b"original").expect("file");
        let policy = policy(temp.path());

        let authorized = policy
            .authorize_handle(0, "dir/file.txt", PathAccess::Read)
            .expect("open");
        assert!(authorized
            .final_path()
            .to_string_lossy()
            .to_lowercase()
            .ends_with("file.txt"));

        // End the validation window, then let a concurrent actor swap the
        // directory: move the real one aside and drop a junction in its place.
        drop(authorized);
        std::fs::rename(temp.path().join("dir"), temp.path().join("dir_old")).expect("rename");
        if !try_create_junction(&temp.path().join("dir"), outside.path()) {
            eprintln!(
                "SKIP: junction creation unavailable (needs NTFS); platform handle defense not exercised"
            );
            return;
        }
        let error = policy
            .authorize_handle(0, "dir/file.txt", PathAccess::Read)
            .expect_err("swap");
        assert_eq!(error.code(), "path.reparse_escape");

        let _ = std::fs::remove_dir(temp.path().join("dir"));
    }

    #[test]
    fn error_codes_for_path_families_are_stable() {
        assert_eq!(ErrorCode::PathNotFound.code(), "path.not_found");
        assert_eq!(
            ErrorCode::PathOutsideWorkspace.code(),
            "path.outside_workspace"
        );
    }
}
