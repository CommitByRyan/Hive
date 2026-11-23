//! Handle-level path authorization.
//!
//! On Windows the platform walks each path component with
//! `FILE_FLAG_OPEN_REPARSE_POINT`, rejects any reparse point before following
//! it, verifies the final path with `GetFinalPathNameByHandleW` against the
//! opened root, and compares the volume serial to reject cross-volume escapes.
//! On other platforms the platform falls back to `canonicalize` and reports the
//! reduced capability explicitly: it is an application-layer limit with a
//! TOCTOU window, not handle-level isolation.

use std::path::{Path, PathBuf};

use crate::error::PlatformError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenMode {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PathCapabilities {
    pub handle_level: bool,
    pub reparse_detection: bool,
    pub final_path_verification: bool,
    pub note: &'static str,
}

pub fn path_capabilities() -> PathCapabilities {
    #[cfg(windows)]
    {
        PathCapabilities {
            handle_level: true,
            reparse_detection: true,
            final_path_verification: true,
            note: "Win32 handle traversal: FILE_FLAG_OPEN_REPARSE_POINT per component + GetFinalPathNameByHandleW",
        }
    }
    #[cfg(not(windows))]
    {
        PathCapabilities {
            handle_level: false,
            reparse_detection: false,
            final_path_verification: false,
            note:
                "canonicalize + prefix check only; TOCTOU window remains (application-layer limit)",
        }
    }
}

/// An opened workspace-root directory. On Windows this owns a directory handle
/// whose final path and volume serial number are the comparison baseline.
pub struct RootHandle {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    final_path: PathBuf,
    #[cfg(windows)]
    volume: Option<u32>,
}

impl RootHandle {
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }
}

#[cfg(windows)]
impl Drop for RootHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// An authorized path. On Windows the returned handle is the exact object the
/// traversal opened; callers must not re-open the path by string afterwards.
pub struct PathHandle {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    final_path: PathBuf,
    is_dir: bool,
}

impl std::fmt::Debug for PathHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PathHandle")
            .field("final_path", &self.final_path)
            .field("is_dir", &self.is_dir)
            .finish_non_exhaustive()
    }
}

impl PathHandle {
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }

    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    #[cfg(windows)]
    pub fn as_raw_handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.handle
    }

    #[cfg(not(windows))]
    pub fn path(&self) -> &Path {
        &self.final_path
    }
}

#[cfg(windows)]
impl Drop for PathHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

pub trait PathPlatform: Send + Sync {
    fn capabilities(&self) -> PathCapabilities;
    fn open_root(&self, root: &Path) -> Result<RootHandle, PlatformError>;
    fn open_checked(
        &self,
        root: &RootHandle,
        relative: &Path,
        mode: OpenMode,
    ) -> Result<PathHandle, PlatformError>;
    /// Create a missing leaf file (and any missing parent directories) without
    /// following reparse points, re-verifying every created segment.
    fn create_file(&self, root: &RootHandle, relative: &Path) -> Result<PathHandle, PlatformError>;
}

#[cfg(windows)]
#[derive(Default, Clone, Copy)]
pub struct WindowsPathPlatform;

#[cfg(not(windows))]
#[derive(Default, Clone, Copy)]
pub struct FallbackPathPlatform;

#[cfg(windows)]
pub type DefaultPathPlatform = WindowsPathPlatform;

#[cfg(not(windows))]
pub type DefaultPathPlatform = FallbackPathPlatform;

pub fn default_path_platform() -> DefaultPathPlatform {
    DefaultPathPlatform::default()
}

#[cfg(windows)]
impl PathPlatform for WindowsPathPlatform {
    fn capabilities(&self) -> PathCapabilities {
        path_capabilities()
    }

    fn open_root(&self, root: &Path) -> Result<RootHandle, PlatformError> {
        let handle = open_directory(root)?;
        let final_path = final_path_of(handle)?;
        let info = file_info(handle)?;
        Ok(RootHandle {
            handle,
            final_path,
            volume: Some(info.dwVolumeSerialNumber),
        })
    }

    fn open_checked(
        &self,
        root: &RootHandle,
        relative: &Path,
        mode: OpenMode,
    ) -> Result<PathHandle, PlatformError> {
        walk_existing(root, relative, mode)
    }

    fn create_file(&self, root: &RootHandle, relative: &Path) -> Result<PathHandle, PlatformError> {
        create_missing(root, relative)
    }
}

#[cfg(not(windows))]
impl PathPlatform for FallbackPathPlatform {
    fn capabilities(&self) -> PathCapabilities {
        path_capabilities()
    }

    fn open_root(&self, root: &Path) -> Result<RootHandle, PlatformError> {
        let canonical = std::fs::canonicalize(root).map_err(|source| PlatformError::Io {
            context: format!("cannot resolve workspace root {}", root.display()),
            source,
        })?;
        if !canonical.is_dir() {
            return Err(PlatformError::Unsupported(format!(
                "workspace root is not a directory: {}",
                canonical.display()
            )));
        }
        Ok(RootHandle {
            final_path: canonical,
        })
    }

    fn open_checked(
        &self,
        root: &RootHandle,
        relative: &Path,
        _mode: OpenMode,
    ) -> Result<PathHandle, PlatformError> {
        let full = root.final_path.join(relative);
        let canonical = std::fs::canonicalize(&full).map_err(|source| PlatformError::Io {
            context: format!("cannot resolve {}", full.display()),
            source,
        })?;
        if !within(&root.final_path, &canonical) {
            return Err(PlatformError::OutsideRoot(format!(
                "{} resolves outside {}",
                full.display(),
                root.final_path.display()
            )));
        }
        Ok(PathHandle {
            final_path: canonical,
            is_dir: canonical.is_dir(),
        })
    }

    fn create_file(&self, root: &RootHandle, relative: &Path) -> Result<PathHandle, PlatformError> {
        let full = root.final_path.join(relative);
        if let Some(parent) = full
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|source| PlatformError::Io {
                context: format!("cannot create parent {}", parent.display()),
                source,
            })?;
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&full)
            .map_err(|source| PlatformError::Io {
                context: format!("cannot create {}", full.display()),
                source,
            })?;
        let canonical = std::fs::canonicalize(&full).map_err(|source| PlatformError::Io {
            context: format!("cannot resolve {}", full.display()),
            source,
        })?;
        if !within(&root.final_path, &canonical) {
            let _ = std::fs::remove_file(&full);
            return Err(PlatformError::OutsideRoot(format!(
                "{} resolves outside {}",
                full.display(),
                root.final_path.display()
            )));
        }
        Ok(PathHandle {
            final_path: canonical,
            is_dir: false,
        })
    }
}

#[cfg(not(windows))]
fn within(root: &Path, candidate: &Path) -> bool {
    use std::path::Component;
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    let mut candidate_components = candidate.components();
    for root_component in root.components() {
        match candidate_components.next() {
            Some(candidate_component) if candidate_component == root_component => {}
            _ => return false,
        }
    }
    true
}

#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};

#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, GetFileInformationByHandle, GetFinalPathNameByHandleW,
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

#[cfg(windows)]
const DIR_RIGHTS: u32 = FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES;

#[cfg(windows)]
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

#[cfg(windows)]
fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn last_error() -> std::io::Error {
    std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

#[cfg(windows)]
fn create_file_w(
    path: &std::ffi::OsStr,
    access: u32,
    disposition: u32,
    flags: u32,
) -> Result<HANDLE, std::io::Error> {
    let wide = wide(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            SHARE_ALL,
            std::ptr::null(),
            disposition,
            flags,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(last_error())
    } else {
        Ok(handle)
    }
}

#[cfg(windows)]
fn map_open_err(path: &Path, error: std::io::Error) -> PlatformError {
    if error.kind() == std::io::ErrorKind::NotFound {
        PlatformError::NotFound(format!("{} does not exist", path.display()))
    } else {
        PlatformError::Io {
            context: format!("open {}", path.display()),
            source: error,
        }
    }
}

#[cfg(windows)]
fn open_directory(path: &Path) -> Result<HANDLE, PlatformError> {
    create_file_w(
        path.as_os_str(),
        DIR_RIGHTS,
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )
    .map_err(|error| map_open_err(path, error))
}

#[cfg(windows)]
fn open_leaf(path: &Path, mode: OpenMode) -> Result<HANDLE, PlatformError> {
    let access = match mode {
        OpenMode::Read => FILE_GENERIC_READ,
        OpenMode::Write => FILE_GENERIC_READ | FILE_GENERIC_WRITE,
    };
    match create_file_w(
        path.as_os_str(),
        access,
        OPEN_EXISTING,
        FILE_FLAG_OPEN_REPARSE_POINT,
    ) {
        Ok(handle) => Ok(handle),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Other
            ) =>
        {
            // May be a directory rather than a file. Retry with backup semantics.
            create_file_w(
                path.as_os_str(),
                FILE_GENERIC_READ,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
            .map_err(|error| map_open_err(path, error))
        }
        Err(error) => Err(map_open_err(path, error)),
    }
}

#[cfg(windows)]
fn create_leaf(path: &Path) -> Result<HANDLE, PlatformError> {
    create_file_w(
        path.as_os_str(),
        FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        CREATE_NEW,
        FILE_ATTRIBUTE_NORMAL,
    )
    .map_err(|error| map_open_err(path, error))
}

#[cfg(windows)]
fn file_info(handle: HANDLE) -> Result<BY_HANDLE_FILE_INFORMATION, PlatformError> {
    let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    if ok == 0 {
        return Err(PlatformError::Io {
            context: "GetFileInformationByHandle".to_owned(),
            source: last_error(),
        });
    }
    Ok(info)
}

#[cfg(windows)]
fn final_path_of(handle: HANDLE) -> Result<PathBuf, PlatformError> {
    let mut buffer = vec![0u16; 512];
    loop {
        let needed = unsafe {
            GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0)
        };
        if needed == 0 {
            return Err(PlatformError::Io {
                context: "GetFinalPathNameByHandleW".to_owned(),
                source: last_error(),
            });
        }
        let needed = needed as usize;
        if needed < buffer.len() {
            buffer.truncate(needed);
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer)));
        }
        buffer.resize(needed + 1, 0);
    }
}

#[cfg(windows)]
fn normalized_final(path: &Path) -> Vec<String> {
    let text = path.to_string_lossy();
    let text = text.strip_prefix("\\\\?\\").unwrap_or(&text);
    let text = text.strip_prefix("\\\\.\\").unwrap_or(text);
    text.split('\\')
        .map(|component| component.to_lowercase())
        .filter(|component| !component.is_empty())
        .collect()
}

#[cfg(windows)]
fn final_within(root: &[String], candidate: &[String]) -> bool {
    candidate.len() >= root.len()
        && root
            .iter()
            .zip(candidate.iter())
            .all(|(left, right)| left == right)
}

#[cfg(windows)]
fn names_equal(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
}

#[cfg(windows)]
fn inspect_handle(
    handle: HANDLE,
    current: &Path,
    root: &RootHandle,
    root_norm: &[String],
) -> Result<(BY_HANDLE_FILE_INFORMATION, PathBuf), PlatformError> {
    let info = file_info(handle)?;
    let attributes = info.dwFileAttributes;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(PlatformError::ReparsePoint(format!(
            "reparse point is not allowed at {}",
            current.display()
        )));
    }
    if attributes & FILE_ATTRIBUTE_DEVICE != 0 {
        return Err(PlatformError::Unsupported(format!(
            "device object is not allowed at {}",
            current.display()
        )));
    }
    let final_path = final_path_of(handle)?;
    let normalized = normalized_final(&final_path);
    if !final_within(root_norm, &normalized) {
        return Err(PlatformError::OutsideRoot(format!(
            "{} resolves outside the workspace root",
            current.display()
        )));
    }
    if root
        .volume
        .is_some_and(|volume| info.dwVolumeSerialNumber != volume)
    {
        return Err(PlatformError::CrossVolume(format!(
            "{} is on a different volume than the workspace root",
            current.display()
        )));
    }
    if let (Some(final_name), Some(requested)) = (final_path.file_name(), current.file_name()) {
        if !names_equal(final_name, requested) {
            return Err(PlatformError::Unsupported(format!(
                "{} resolves to a different name (8.3 short name or alias)",
                current.display()
            )));
        }
    }
    Ok((info, final_path))
}

#[cfg(windows)]
fn walk_existing(
    root: &RootHandle,
    relative: &Path,
    mode: OpenMode,
) -> Result<PathHandle, PlatformError> {
    let components: Vec<std::ffi::OsString> = relative
        .iter()
        .map(|component| component.to_os_string())
        .collect();
    if components.is_empty() {
        return Err(PlatformError::Unsupported(
            "empty relative path cannot be opened".to_owned(),
        ));
    }
    let root_norm = normalized_final(&root.final_path);
    let mut current = root.final_path.clone();
    let leaf_index = components.len() - 1;
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let is_leaf = index == leaf_index;
        let handle = if is_leaf {
            open_leaf(&current, mode)?
        } else {
            open_directory(&current)?
        };
        let (info, final_path) = inspect_handle(handle, &current, root, &root_norm)?;
        let is_dir = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        if is_leaf {
            if mode == OpenMode::Write && is_dir {
                unsafe {
                    CloseHandle(handle);
                }
                return Err(PlatformError::Unsupported(format!(
                    "cannot write to directory {}",
                    current.display()
                )));
            }
            return Ok(PathHandle {
                handle,
                final_path,
                is_dir,
            });
        }
        if !is_dir {
            unsafe {
                CloseHandle(handle);
            }
            return Err(PlatformError::Unsupported(format!(
                "{} is not a directory",
                current.display()
            )));
        }
        unsafe {
            CloseHandle(handle);
        }
    }
    unreachable!("leaf component is always returned")
}

#[cfg(windows)]
fn create_missing(root: &RootHandle, relative: &Path) -> Result<PathHandle, PlatformError> {
    let components: Vec<std::ffi::OsString> = relative
        .iter()
        .map(|component| component.to_os_string())
        .collect();
    if components.is_empty() {
        return Err(PlatformError::Unsupported(
            "empty relative path cannot be created".to_owned(),
        ));
    }
    let root_norm = normalized_final(&root.final_path);
    let mut current = root.final_path.clone();

    // Walk the longest existing ancestor, verifying each existing directory.
    let mut existing = 0usize;
    while existing < components.len() {
        current.push(&components[existing]);
        match open_directory(&current) {
            Ok(handle) => {
                inspect_handle(handle, &current, root, &root_norm)?;
                unsafe {
                    CloseHandle(handle);
                }
            }
            Err(PlatformError::NotFound(_)) => {
                current.pop();
                break;
            }
            Err(error) => return Err(error),
        }
        existing += 1;
    }

    if existing == components.len() {
        return Err(PlatformError::Unsupported(format!(
            "{} already exists",
            current.display()
        )));
    }

    let leaf_index = components.len() - 1;
    while existing <= leaf_index {
        let is_leaf = existing == leaf_index;
        current.push(&components[existing]);
        if is_leaf {
            let handle = create_leaf(&current)?;
            let (info, final_path) = inspect_handle(handle, &current, root, &root_norm)?;
            if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                unsafe {
                    CloseHandle(handle);
                }
                return Err(PlatformError::Unsupported(format!(
                    "created object is unexpectedly a directory: {}",
                    current.display()
                )));
            }
            return Ok(PathHandle {
                handle,
                final_path,
                is_dir: false,
            });
        }
        let ok = unsafe {
            CreateDirectoryW(
                wide(&std::ffi::OsString::from(&current)).as_ptr(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(PlatformError::Io {
                context: format!("CreateDirectoryW {}", current.display()),
                source: last_error(),
            });
        }
        let handle = open_directory(&current)?;
        inspect_handle(handle, &current, root, &root_norm)?;
        unsafe {
            CloseHandle(handle);
        }
        existing += 1;
    }
    unreachable!("leaf component is always created and returned")
}

#[cfg(all(test, windows))]
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
                "hive-platform-{name}-{}-{counter}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn open_checked_reads_file_and_reports_final_path_within_root() {
        let temp = TempDir::new("read");
        std::fs::write(temp.path().join("file.txt"), b"hello").expect("write");
        let platform = default_path_platform();
        let root = platform.open_root(temp.path()).expect("root");
        let handle = platform
            .open_checked(&root, Path::new("file.txt"), OpenMode::Read)
            .expect("open");
        assert!(!handle.is_dir());
        assert!(handle
            .final_path()
            .to_string_lossy()
            .to_lowercase()
            .ends_with("file.txt"));
    }

    #[test]
    fn create_file_creates_missing_parents_and_leaf() {
        let temp = TempDir::new("create");
        let platform = default_path_platform();
        let root = platform.open_root(temp.path()).expect("root");
        let handle = platform
            .create_file(&root, Path::new("nested/out.txt"))
            .expect("create");
        assert!(!handle.is_dir());
        assert!(temp.path().join("nested").join("out.txt").exists());
    }
}
