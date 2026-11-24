use std::fs;
use std::path::{Path, PathBuf};

use hive_config::Config;
use hive_llm::ModelCatalog;
use hive_server::{build_router, AppState, TICK_PERIOD};
use hive_store::Store;

#[tokio::main]
async fn main() {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("configuration error: {error}");
            std::process::exit(1);
        }
    };

    let db_path = match prepare_database(config.data_dir()) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("database preparation failed: {error}");
            std::process::exit(1);
        }
    };
    let mut store = match Store::open(&db_path) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("failed to open store at {}: {error}", db_path.display());
            std::process::exit(1);
        }
    };

    if let Err(error) = hive_runtime::run_startup(&mut store, hive_store::now_ms()) {
        eprintln!("startup recovery failed: {error}");
        std::process::exit(1);
    }

    let mode = config.mode().to_string();
    let host = config.host().to_owned();
    let port = config.port();
    let state = AppState::new(store, ModelCatalog::new(), mode, host.clone(), port);
    state.start_tick_loop(TICK_PERIOD);
    let bind_addr = state.bind_addr();
    let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("failed to bind {bind_addr}: {error}");
            std::process::exit(1);
        }
    };

    let app = build_router(state);
    eprintln!("hive-server listening on http://{bind_addr}");
    if let Err(error) = axum::serve(listener, app).await {
        eprintln!("server error: {error}");
        std::process::exit(1);
    }
}

fn prepare_database(data_dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(data_dir).map_err(|error| {
        format!(
            "failed to create data directory {}: {error}",
            data_dir.display()
        )
    })?;
    let db_path = data_dir.join("hive.db");
    if !db_path.exists() {
        return Ok(db_path);
    }

    let metadata = fs::metadata(&db_path)
        .map_err(|error| format!("failed to inspect {}: {error}", db_path.display()))?;
    if metadata.len() == 0 {
        eprintln!(
            "database file {} is empty; starting from a fresh schema",
            db_path.display()
        );
        return Ok(db_path);
    }

    let current = hive_store::read_schema_version(&db_path).map_err(|error| {
        format!(
            "failed to read schema version from {}: {error}",
            db_path.display()
        )
    })?;
    if current > hive_store::SCHEMA_VERSION {
        return Err(format!(
            "database schema version {current} is newer than supported {}",
            hive_store::SCHEMA_VERSION
        ));
    }
    if current < hive_store::SCHEMA_VERSION {
        let backups = data_dir.join("backups");
        fs::create_dir_all(&backups).map_err(|error| {
            format!(
                "failed to create backup directory {}: {error}",
                backups.display()
            )
        })?;
        let backup = unique_backup_path(&backups, current, hive_store::now_ms());
        hive_store::backup_database(&db_path, &backup).map_err(|error| {
            format!(
                "failed to back up {} to {}: {error}",
                db_path.display(),
                backup.display()
            )
        })?;
        eprintln!("database backup written to {}", backup.display());
    }
    Ok(db_path)
}

fn unique_backup_path(backups: &Path, version: i64, timestamp_ms: i64) -> PathBuf {
    let base = format!("hive.db.bak-v{version}-{timestamp_ms}");
    let mut path = backups.join(&base);
    let mut suffix = 1_u32;
    while path.exists() {
        path = backups.join(format!("{base}-{suffix}"));
        suffix = suffix.saturating_add(1);
    }
    path
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::Connection;

    use super::*;

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
                "hive-server-startup-{name}-{}-{nanos}-{counter}",
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

    fn set_user_version(path: &Path, version: i64) {
        let conn = Connection::open(path).expect("open raw");
        conn.pragma_update(None, "user_version", version)
            .expect("set version");
    }

    #[test]
    fn older_schema_is_backed_up_before_serve() {
        let dir = TempDir::new("older");
        let data_dir = dir.path();
        let db_path = data_dir.join("hive.db");
        create_database(&db_path);
        set_user_version(&db_path, 5);
        let before = fs::read(&db_path).expect("read db");

        let prepared = prepare_database(data_dir).expect("prepare");
        assert_eq!(prepared, db_path);
        assert_eq!(fs::read(&db_path).expect("read db after"), before);

        let backups = data_dir.join("backups");
        let entries: Vec<PathBuf> = fs::read_dir(&backups)
            .expect("backups")
            .map(|entry| entry.expect("entry").path())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            hive_store::read_schema_version(&entries[0]).expect("backup version"),
            5
        );
    }

    #[test]
    fn current_schema_is_not_backed_up() {
        let dir = TempDir::new("current");
        let data_dir = dir.path();
        let db_path = data_dir.join("hive.db");
        create_database(&db_path);

        prepare_database(data_dir).expect("prepare");
        assert!(!data_dir.join("backups").exists());
    }

    #[test]
    fn newer_schema_refuses_to_start() {
        let dir = TempDir::new("newer");
        let data_dir = dir.path();
        let db_path = data_dir.join("hive.db");
        create_database(&db_path);
        set_user_version(&db_path, hive_store::SCHEMA_VERSION + 1);

        let error = prepare_database(data_dir).expect_err("future schema");
        assert!(error.contains("newer than supported"));
        assert!(!data_dir.join("backups").exists());
    }

    #[test]
    fn empty_database_file_is_treated_as_fresh() {
        let dir = TempDir::new("empty");
        let data_dir = dir.path();
        fs::write(data_dir.join("hive.db"), b"").expect("empty db");

        prepare_database(data_dir).expect("prepare");
        assert!(!data_dir.join("backups").exists());
    }
}
