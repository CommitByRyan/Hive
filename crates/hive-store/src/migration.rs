use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags};

use crate::error::{StoreError, StoreResult};
use crate::schema::{self, BUSY_TIMEOUT_MS, SCHEMA_VERSION};

/// Result of a non-destructive migration dry-run. It never contains any
/// database row data or configured secrets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationDryRunReport {
    pub source: PathBuf,
    pub exists: bool,
    pub current_version: i64,
    pub target_version: i64,
    pub pending_versions: Vec<i64>,
    pub final_version: i64,
    pub copy_path: Option<PathBuf>,
    pub copy_retained: bool,
    pub failure: Option<String>,
    pub warning: Option<String>,
}

/// Read `PRAGMA user_version` without opening the database for writes.
pub fn read_schema_version(path: &Path) -> StoreResult<i64> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// Create a consistent SQLite snapshot with `VACUUM INTO`.
///
/// This reads the source through SQLite instead of copying files directly, so
/// a WAL database is copied at a transactionally consistent point.
pub fn backup_database(source: &Path, destination: &Path) -> StoreResult<()> {
    if destination.exists() {
        return Err(StoreError::Conflict(format!(
            "backup destination already exists: {}",
            destination.display()
        )));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let destination_text = destination.to_str().ok_or_else(|| {
        StoreError::Conflict("backup destination path is not valid UTF-8".to_owned())
    })?;
    let conn = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    conn.execute("VACUUM INTO ?1", params![destination_text])?;
    Ok(())
}

/// Migrate a throwaway copy and report what the real database would do.
///
/// The source database is only read. On success the copy is removed; on a
/// copy or migration failure the copy path is retained for inspection.
pub fn dry_run_migration(source: &Path) -> StoreResult<MigrationDryRunReport> {
    let target_version = SCHEMA_VERSION;
    if !source.exists() {
        return Ok(fresh_report(source, target_version, false));
    }
    let metadata = fs::metadata(source)?;
    if metadata.len() == 0 {
        return Ok(fresh_report(source, target_version, true));
    }

    let current_version = read_schema_version(source)?;
    if current_version > target_version {
        return Err(StoreError::Conflict(format!(
            "database schema version {current_version} is newer than supported {target_version}"
        )));
    }
    let pending_versions: Vec<i64> = ((current_version + 1)..=target_version).collect();
    let copy_path = unique_copy_path(source);

    if let Err(error) = backup_database(source, &copy_path) {
        return Ok(MigrationDryRunReport {
            source: source.to_path_buf(),
            exists: true,
            current_version,
            target_version,
            pending_versions,
            final_version: current_version,
            copy_retained: copy_path.exists(),
            copy_path: Some(copy_path),
            failure: Some(format!("failed to create consistent copy: {error}")),
            warning: None,
        });
    }

    let migrated = migrate_copy(&copy_path, current_version);
    let final_version = match migrated {
        Ok(version) => version,
        Err(error) => {
            return Ok(MigrationDryRunReport {
                source: source.to_path_buf(),
                exists: true,
                current_version,
                target_version,
                pending_versions,
                final_version: current_version,
                copy_retained: true,
                copy_path: Some(copy_path),
                failure: Some(format!("migration failed on copy: {error}")),
                warning: None,
            });
        }
    };

    let warning = match remove_copy(&copy_path) {
        Ok(()) => None,
        Err(error) => Some(format!(
            "failed to remove dry-run copy {}: {error}",
            copy_path.display()
        )),
    };
    let copy_retained = copy_path.exists();

    Ok(MigrationDryRunReport {
        source: source.to_path_buf(),
        exists: true,
        current_version,
        target_version,
        pending_versions,
        final_version,
        copy_path: Some(copy_path),
        copy_retained,
        failure: None,
        warning,
    })
}

fn migrate_copy(path: &Path, expected_current: i64) -> StoreResult<i64> {
    let copy_version = read_schema_version(path)?;
    if copy_version != expected_current {
        return Err(StoreError::Conflict(format!(
            "copy schema version {copy_version} does not match source {expected_current}"
        )));
    }
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    schema::migrate(&mut conn)?;
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn fresh_report(source: &Path, target_version: i64, empty_file: bool) -> MigrationDryRunReport {
    MigrationDryRunReport {
        source: source.to_path_buf(),
        exists: source.exists(),
        current_version: 0,
        target_version,
        pending_versions: (1..=target_version).collect(),
        final_version: target_version,
        copy_path: None,
        copy_retained: false,
        failure: None,
        warning: empty_file.then(|| {
            "database file is empty; it will be initialized from a fresh schema".to_owned()
        }),
    }
}

fn unique_copy_path(source: &Path) -> PathBuf {
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let file_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("hive.db");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let base = format!("{file_name}.dry-run-{}-{nanos}", std::process::id());
    let mut path = parent.join(&base);
    let mut suffix = 1_u32;
    while path.exists() {
        path = parent.join(format!("{base}-{suffix}"));
        suffix = suffix.saturating_add(1);
    }
    path
}

fn remove_copy(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(path, suffix);
        if sidecar.exists() {
            fs::remove_file(sidecar)?;
        }
    }
    Ok(())
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::Store;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "hive-store-migration-{name}-{}-{nanos}-{counter}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn create_database(path: &Path) {
        let store = Store::open(path).expect("create store");
        drop(store);
    }

    fn create_v5_database(path: &Path) {
        let conn = Connection::open(path).expect("open raw");
        conn.execute_batch(crate::schema::V1_SCHEMA).expect("v1");
        conn.execute_batch(crate::schema::V2_SCHEMA).expect("v2");
        conn.execute_batch(crate::schema::V3_SCHEMA).expect("v3");
        conn.execute_batch(crate::schema::V4_SCHEMA).expect("v4");
        conn.execute_batch(crate::schema::V5_SCHEMA).expect("v5");
        conn.pragma_update(None, "user_version", 5).expect("set v5");
    }

    fn set_user_version(path: &Path, version: i64) {
        let conn = Connection::open(path).expect("open raw");
        conn.pragma_update(None, "user_version", version)
            .expect("set version");
    }

    fn sha256(path: &Path) -> String {
        let bytes = fs::read(path).expect("read file");
        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn dry_run_migrates_copy_and_leaves_source_unchanged() {
        let dir = TempDir::new("v5");
        let path = dir.path().join("hive.db");
        create_v5_database(&path);
        let before = sha256(&path);

        let report = dry_run_migration(&path).expect("dry run");
        assert_eq!(report.current_version, 5);
        assert_eq!(report.target_version, SCHEMA_VERSION);
        assert_eq!(report.pending_versions, vec![6]);
        assert_eq!(report.final_version, 6);
        assert!(report.failure.is_none());
        assert!(!report.copy_retained);
        let copy = report.copy_path.expect("copy path");
        assert!(!copy.exists());
        assert_eq!(sha256(&path), before);
    }

    #[test]
    fn dry_run_on_current_database_reports_no_pending_migrations() {
        let dir = TempDir::new("v6");
        let path = dir.path().join("hive.db");
        create_database(&path);

        let report = dry_run_migration(&path).expect("dry run");
        assert_eq!(report.current_version, SCHEMA_VERSION);
        assert!(report.pending_versions.is_empty());
        assert_eq!(report.final_version, SCHEMA_VERSION);
        assert!(report.failure.is_none());
    }

    #[test]
    fn dry_run_rejects_database_newer_than_supported() {
        let dir = TempDir::new("newer");
        let path = dir.path().join("hive.db");
        create_database(&path);
        set_user_version(&path, SCHEMA_VERSION + 1);

        let error = dry_run_migration(&path).expect_err("newer database");
        assert!(error.to_string().contains("newer than supported"));
    }

    #[test]
    fn dry_run_missing_database_reports_fresh_start() {
        let dir = TempDir::new("missing");
        let path = dir.path().join("hive.db");

        let report = dry_run_migration(&path).expect("dry run");
        assert!(!report.exists);
        assert_eq!(report.current_version, 0);
        assert_eq!(report.pending_versions, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(report.final_version, SCHEMA_VERSION);
        assert!(report.copy_path.is_none());
        assert!(report.failure.is_none());
    }

    #[test]
    fn backup_database_copies_without_touching_source() {
        let dir = TempDir::new("backup");
        let path = dir.path().join("hive.db");
        let backup = dir.path().join("backups").join("hive.db.bak");
        create_database(&path);
        let before = sha256(&path);

        backup_database(&path, &backup).expect("backup");
        assert_eq!(
            read_schema_version(&backup).expect("backup version"),
            SCHEMA_VERSION
        );
        assert_eq!(sha256(&path), before);
    }

    #[test]
    fn dry_run_retains_copy_when_migration_fails() {
        let dir = TempDir::new("failed");
        let path = dir.path().join("hive.db");
        create_v5_database(&path);
        {
            let conn = Connection::open(&path).expect("open raw");
            conn.execute_batch("CREATE TABLE routines (x TEXT);")
                .expect("incompatible routines table");
        }
        let before = sha256(&path);

        let report = dry_run_migration(&path).expect("dry run");
        assert_eq!(report.current_version, 5);
        assert_eq!(report.pending_versions, vec![6]);
        assert_eq!(report.final_version, 5);
        assert!(report.failure.is_some());
        assert!(report.copy_retained);
        let copy = report.copy_path.expect("copy path");
        assert!(copy.exists());
        assert_eq!(read_schema_version(&path).expect("source version"), 5);
        assert_eq!(sha256(&path), before);
    }
}
