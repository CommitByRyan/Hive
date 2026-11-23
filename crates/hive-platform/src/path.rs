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
    pub listing_handle_relative: bool,
    pub atomic_replace_handle_relative: bool,
    pub note: &'static str,
}

pub fn path_capabilities() -> PathCapabilities {
    #[cfg(windows)]
    {
        PathCapabilities {
            handle_level: true,
            reparse_detection: true,
            final_path_verification: true,
            listing_handle_relative: true,
            atomic_replace_handle_relative: false,
            note: "Win32 handle traversal and directory enumeration; atomic replace is path-based with parent identity checks before and after, leaving a residual rename window",
        }
    }
    #[cfg(not(windows))]
    {
        PathCapabilities {
            handle_level: false,
            reparse_detection: false,
            final_path_verification: false,
            listing_handle_relative: false,
            atomic_replace_handle_relative: false,
            note:
                "canonicalize + prefix check only; path-based listing and replacement retain TOCTOU windows (application-layer limit)",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryEntryKind {
    File,
    Directory,
    Link,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    pub name: String,
    pub kind: DirectoryEntryKind,
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

    #[cfg(windows)]
    pub fn into_path_handle(self) -> PathHandle {
        let this = std::mem::ManuallyDrop::new(self);
        PathHandle {
            handle: this.handle,
            final_path: this.final_path.clone(),
            is_dir: true,
        }
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

    /// Transfer the owned operating-system handle to a `File` exactly once.
    /// The non-Windows fallback has no owned handle and therefore reopens the
    /// already canonicalized path; its reduced capability is reported above.
    pub fn into_file(self) -> Result<std::fs::File, PlatformError> {
        #[cfg(windows)]
        {
            use std::os::windows::io::FromRawHandle;

            if self.is_dir {
                return Err(PlatformError::Unsupported(format!(
                    "cannot read directory {} as a file",
                    self.final_path.display()
                )));
            }
            let this = std::mem::ManuallyDrop::new(self);
            Ok(unsafe { std::fs::File::from_raw_handle(this.handle) })
        }
        #[cfg(not(windows))]
        {
            std::fs::File::open(&self.final_path).map_err(|source| PlatformError::Io {
                context: format!("open {}", self.final_path.display()),
                source,
            })
        }
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
    fn list_directory(&self, directory: &PathHandle) -> Result<Vec<DirectoryEntry>, PlatformError>;
    fn list_root(&self, root: &RootHandle) -> Result<Vec<DirectoryEntry>, PlatformError>;
    fn atomic_write(
        &self,
        root: &RootHandle,
        relative: &Path,
        content: &[u8],
    ) -> Result<(), PlatformError>;
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

    fn list_directory(&self, directory: &PathHandle) -> Result<Vec<DirectoryEntry>, PlatformError> {
        if !directory.is_dir {
            return Err(PlatformError::Unsupported(format!(
                "{} is not a directory",
                directory.final_path.display()
            )));
        }
        list_directory_handle(directory.handle)
    }

    fn list_root(&self, root: &RootHandle) -> Result<Vec<DirectoryEntry>, PlatformError> {
        list_directory_handle(root.handle)
    }

    fn atomic_write(
        &self,
        root: &RootHandle,
        relative: &Path,
        content: &[u8],
    ) -> Result<(), PlatformError> {
        atomic_write_windows(root, relative, content)
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

    fn list_directory(&self, directory: &PathHandle) -> Result<Vec<DirectoryEntry>, PlatformError> {
        list_directory_fallback(&directory.final_path)
    }

    fn list_root(&self, root: &RootHandle) -> Result<Vec<DirectoryEntry>, PlatformError> {
        list_directory_fallback(&root.final_path)
    }

    fn atomic_write(
        &self,
        root: &RootHandle,
        relative: &Path,
        content: &[u8],
    ) -> Result<(), PlatformError> {
        atomic_write_fallback(root, relative, content)
    }
}

#[cfg(not(windows))]
fn list_directory_fallback(path: &Path) -> Result<Vec<DirectoryEntry>, PlatformError> {
    let entries = std::fs::read_dir(path).map_err(|source| PlatformError::Io {
        context: format!("list {}", path.display()),
        source,
    })?;
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| PlatformError::Io {
            context: format!("list {}", path.display()),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| PlatformError::Io {
            context: format!("inspect {}", entry.path().display()),
            source,
        })?;
        let kind = if file_type.is_symlink() {
            DirectoryEntryKind::Link
        } else if file_type.is_dir() {
            DirectoryEntryKind::Directory
        } else {
            DirectoryEntryKind::File
        };
        result.push(DirectoryEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
        });
    }
    Ok(result)
}

#[cfg(not(windows))]
fn atomic_write_fallback(
    root: &RootHandle,
    relative: &Path,
    content: &[u8],
) -> Result<(), PlatformError> {
    use std::io::Write;

    let target = root.final_path.join(relative);
    let parent = target.parent().ok_or_else(|| {
        PlatformError::Unsupported(format!("{} has no parent directory", target.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(|source| PlatformError::Io {
        context: format!("create parent {}", parent.display()),
        source,
    })?;
    let canonical_parent = std::fs::canonicalize(parent).map_err(|source| PlatformError::Io {
        context: format!("resolve parent {}", parent.display()),
        source,
    })?;
    if !within(&root.final_path, &canonical_parent) {
        return Err(PlatformError::OutsideRoot(format!(
            "{} resolves outside the workspace root",
            parent.display()
        )));
    }
    let file_name = target.file_name().ok_or_else(|| {
        PlatformError::Unsupported(format!("{} has no file name", target.display()))
    })?;
    let temp = unique_temp_path(&canonical_parent, file_name);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|source| PlatformError::Io {
                context: format!("create temporary file {}", temp.display()),
                source,
            })?;
        file.write_all(content)
            .map_err(|source| PlatformError::Io {
                context: format!("write temporary file {}", temp.display()),
                source,
            })?;
        file.sync_all().map_err(|source| PlatformError::Io {
            context: format!("sync temporary file {}", temp.display()),
            source,
        })?;
        drop(file);
        std::fs::rename(&temp, &target).map_err(|source| PlatformError::Io {
            context: format!("replace {}", target.display()),
            source,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn unique_temp_path(parent: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{}.hive-tmp-{}-{counter}",
        file_name.to_string_lossy(),
        std::process::id()
    ))
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
    CreateDirectoryW, CreateFileW, FileIdBothDirectoryInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, MoveFileExW, ReplaceFileW,
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_ID_BOTH_DIR_INFO,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, OPEN_EXISTING,
    REPLACEFILE_WRITE_THROUGH,
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

#[cfg(windows)]
fn list_directory_handle(handle: HANDLE) -> Result<Vec<DirectoryEntry>, PlatformError> {
    const ERROR_NO_MORE_FILES: i32 = 18;

    let mut storage = vec![0u64; 8192];
    let buffer = unsafe {
        std::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<u8>(), storage.len() * 8)
    };
    let mut entries = Vec::new();
    loop {
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileIdBothDirectoryInfo,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
            )
        };
        if ok == 0 {
            let error = last_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
                break;
            }
            if entries.is_empty() && error.kind() == std::io::ErrorKind::InvalidInput {
                return list_directory_path(final_path_of(handle)?);
            }
            return Err(PlatformError::Io {
                context: "GetFileInformationByHandleEx(FileIdBothDirectoryInfo)".to_owned(),
                source: error,
            });
        }

        let mut offset = 0usize;
        loop {
            if offset + std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>() > buffer.len() {
                return Err(PlatformError::Unsupported(
                    "directory enumeration returned a truncated entry".to_owned(),
                ));
            }
            let entry = unsafe { &*buffer.as_ptr().add(offset).cast::<FILE_ID_BOTH_DIR_INFO>() };
            let name_offset =
                std::ptr::addr_of!(entry.FileName) as usize - entry as *const _ as usize;
            let name_bytes = entry.FileNameLength as usize;
            if name_bytes % 2 != 0
                || name_offset
                    .checked_add(name_bytes)
                    .map_or(true, |end| end > buffer.len())
            {
                return Err(PlatformError::Unsupported(
                    "directory enumeration returned an invalid file name".to_owned(),
                ));
            }
            let name = unsafe {
                std::slice::from_raw_parts(
                    buffer.as_ptr().add(offset + name_offset).cast::<u16>(),
                    name_bytes / 2,
                )
            };
            let name = std::ffi::OsString::from_wide(name)
                .to_string_lossy()
                .into_owned();
            if name != "." && name != ".." {
                let kind = if entry.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    DirectoryEntryKind::Link
                } else if entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    DirectoryEntryKind::Directory
                } else {
                    DirectoryEntryKind::File
                };
                entries.push(DirectoryEntry { name, kind });
            }
            if entry.NextEntryOffset == 0 {
                break;
            }
            offset = offset
                .checked_add(entry.NextEntryOffset as usize)
                .ok_or_else(|| {
                    PlatformError::Unsupported("directory enumeration offset overflow".to_owned())
                })?;
        }
    }
    Ok(entries)
}

#[cfg(windows)]
fn list_directory_path(path: PathBuf) -> Result<Vec<DirectoryEntry>, PlatformError> {
    let entries = std::fs::read_dir(&path).map_err(|source| PlatformError::Io {
        context: format!("list {}", path.display()),
        source,
    })?;
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| PlatformError::Io {
            context: format!("list {}", path.display()),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| PlatformError::Io {
            context: format!("inspect {}", entry.path().display()),
            source,
        })?;
        let kind = if file_type.is_symlink() {
            DirectoryEntryKind::Link
        } else if file_type.is_dir() {
            DirectoryEntryKind::Directory
        } else {
            DirectoryEntryKind::File
        };
        result.push(DirectoryEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
        });
    }
    Ok(result)
}

#[cfg(windows)]
fn same_file_identity(
    left: &BY_HANDLE_FILE_INFORMATION,
    right: &BY_HANDLE_FILE_INFORMATION,
) -> bool {
    left.dwVolumeSerialNumber == right.dwVolumeSerialNumber
        && left.nFileIndexHigh == right.nFileIndexHigh
        && left.nFileIndexLow == right.nFileIndexLow
}

#[cfg(windows)]
fn verify_parent_identity(
    root: &RootHandle,
    root_norm: &[String],
    parent_path: &Path,
    expected: &BY_HANDLE_FILE_INFORMATION,
) -> Result<(), PlatformError> {
    let handle = open_directory(parent_path)?;
    let inspected = inspect_handle(handle, parent_path, root, root_norm);
    unsafe {
        CloseHandle(handle);
    }
    let (actual, _) = inspected?;
    if !same_file_identity(expected, &actual) {
        return Err(PlatformError::OutsideRoot(format!(
            "parent directory identity changed while writing {}",
            parent_path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn open_or_create_parent(
    root: &RootHandle,
    root_norm: &[String],
    relative_parent: &Path,
    parent_path: &Path,
) -> Result<(HANDLE, BY_HANDLE_FILE_INFORMATION, PathBuf), PlatformError> {
    match open_directory(parent_path) {
        Ok(handle) => {
            let (info, final_path) = inspect_handle(handle, parent_path, root, root_norm)?;
            Ok((handle, info, final_path))
        }
        Err(PlatformError::NotFound(_)) => {
            if relative_parent.as_os_str().is_empty() {
                return Err(PlatformError::NotFound(format!(
                    "workspace root {} does not exist",
                    parent_path.display()
                )));
            }
            let grandparent = relative_parent.parent().unwrap_or_else(|| Path::new(""));
            let grandparent_path = root.final_path.join(grandparent);
            let (grandparent_handle, _, _) =
                open_or_create_parent(root, root_norm, grandparent, &grandparent_path)?;
            unsafe {
                CloseHandle(grandparent_handle);
            }
            let created = unsafe {
                CreateDirectoryW(wide(parent_path.as_os_str()).as_ptr(), std::ptr::null())
            };
            if created == 0 {
                let error = last_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(PlatformError::Io {
                        context: format!("CreateDirectoryW {}", parent_path.display()),
                        source: error,
                    });
                }
            }
            let handle = open_directory(parent_path)?;
            let (info, final_path) = inspect_handle(handle, parent_path, root, root_norm)?;
            Ok((handle, info, final_path))
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn atomic_write_windows(
    root: &RootHandle,
    relative: &Path,
    content: &[u8],
) -> Result<(), PlatformError> {
    use std::io::Write;
    use std::os::windows::io::FromRawHandle;

    let file_name = relative.file_name().ok_or_else(|| {
        PlatformError::Unsupported("empty relative path cannot be written".to_owned())
    })?;
    let relative_parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let parent_path = root.final_path.join(relative_parent);
    let root_norm = normalized_final(&root.final_path);

    let (parent_handle, parent_info, parent_final) =
        open_or_create_parent(root, &root_norm, relative_parent, &parent_path)?;
    let parent = PathHandle {
        handle: parent_handle,
        final_path: parent_final,
        is_dir: true,
    };

    let target = parent.final_path.join(file_name);
    let target_handle = match open_leaf(&target, OpenMode::Write) {
        Ok(handle) => {
            let (info, final_path) = inspect_handle(handle, &target, root, &root_norm)?;
            if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                unsafe {
                    CloseHandle(handle);
                }
                return Err(PlatformError::Unsupported(format!(
                    "cannot replace directory {}",
                    target.display()
                )));
            }
            Some(PathHandle {
                handle,
                final_path,
                is_dir: false,
            })
        }
        Err(PlatformError::NotFound(_)) => None,
        Err(error) => return Err(error),
    };

    let temp = unique_temp_path(&parent.final_path, file_name);
    let result = (|| {
        let handle = create_leaf(&temp)?;
        let inspected = inspect_handle(handle, &temp, root, &root_norm);
        if let Err(error) = inspected {
            unsafe {
                CloseHandle(handle);
            }
            return Err(error);
        }
        let mut file = unsafe { std::fs::File::from_raw_handle(handle) };
        file.write_all(content)
            .map_err(|source| PlatformError::Io {
                context: format!("write temporary file {}", temp.display()),
                source,
            })?;
        file.sync_all().map_err(|source| PlatformError::Io {
            context: format!("sync temporary file {}", temp.display()),
            source,
        })?;
        drop(file);

        verify_parent_identity(root, &root_norm, &parent_path, &parent_info)?;
        let target_wide = wide(target.as_os_str());
        let temp_wide = wide(temp.as_os_str());
        let backup = target_handle
            .as_ref()
            .map(|_| unique_temp_path(&parent.final_path, file_name));
        let backup_wide = backup.as_ref().map(|path| wide(path.as_os_str()));
        let replaced = if target_handle.is_some() {
            unsafe {
                ReplaceFileW(
                    target_wide.as_ptr(),
                    temp_wide.as_ptr(),
                    backup_wide
                        .as_ref()
                        .map_or(std::ptr::null(), |path| path.as_ptr()),
                    REPLACEFILE_WRITE_THROUGH,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            }
        } else {
            unsafe {
                MoveFileExW(
                    temp_wide.as_ptr(),
                    target_wide.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            }
        };
        if replaced == 0 {
            return Err(PlatformError::Io {
                context: format!("atomically replace {}", target.display()),
                source: last_error(),
            });
        }
        let verification = (|| {
            verify_parent_identity(root, &root_norm, &parent_path, &parent_info)?;
            let final_handle = open_leaf(&target, OpenMode::Read)?;
            let inspected = inspect_handle(final_handle, &target, root, &root_norm);
            unsafe {
                CloseHandle(final_handle);
            }
            inspected.map(|_| ())
        })();
        if verification.is_err() {
            if let Some(backup) = &backup {
                let backup_wide = wide(backup.as_os_str());
                unsafe {
                    ReplaceFileW(
                        target_wide.as_ptr(),
                        backup_wide.as_ptr(),
                        std::ptr::null(),
                        REPLACEFILE_WRITE_THROUGH,
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                };
            } else {
                let _ = std::fs::remove_file(&target);
            }
        }
        if let Some(backup) = &backup {
            let _ = std::fs::remove_file(backup);
        }
        verification
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
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

    #[test]
    fn path_handle_into_file_keeps_single_owner() {
        use std::io::Read;

        let temp = TempDir::new("into-file");
        std::fs::write(temp.path().join("file.txt"), b"owned").expect("write");
        let platform = default_path_platform();
        let root = platform.open_root(temp.path()).expect("root");
        let handle = platform
            .open_checked(&root, Path::new("file.txt"), OpenMode::Read)
            .expect("open");
        let mut file = handle.into_file().expect("into file");
        let mut content = String::new();
        file.read_to_string(&mut content).expect("read");
        drop(file);
        assert_eq!(content, "owned");
        std::fs::remove_file(temp.path().join("file.txt")).expect("single close");
    }
}
