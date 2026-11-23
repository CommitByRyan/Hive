//! Shared helpers for the evaluation harness: temporary databases, metric
//! collection and the stable JSONL output record.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use hive_store::Store;
use hive_types::{EventKind, MissionId, RunId};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A self-cleaning temporary directory for one scenario run.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut dir = std::env::temp_dir();
        dir.push(format!("hive-eval-{name}-{}-{counter}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create eval temp dir");
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn db_path(&self) -> PathBuf {
        self.0.join("hive.db")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One stable output line, matching the runbook schema.
#[derive(Clone, Debug, Serialize)]
pub struct RunRecord {
    pub scenario: String,
    pub commit: String,
    pub run: u32,
    pub workers: usize,
    pub nodes: usize,
    pub wall_ms: u128,
    pub events: u64,
    pub db_bytes: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<&'static str>,
}

impl RunRecord {
    pub fn write_jsonl(&self, out: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            out,
            "{}",
            serde_json::to_string(self).expect("serialize run record")
        )
    }
}

/// Current repository commit (short SHA), or `"unknown"` when Git is absent.
pub fn current_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Create a mission and its first run, returning their ids.
pub fn create_mission_and_run(store: &mut Store, title: &str, goal: &str) -> (MissionId, RunId) {
    let mission = store
        .create_mission(title, goal, now_ms())
        .expect("create mission");
    let run = store
        .create_run(mission.mission_id, now_ms())
        .expect("create run");
    (mission.mission_id, run.run_id)
}

/// A standard single-turn chat completion body for [`ScriptedResponse::ok_json`].
pub fn completion(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-eval",
        "object": "chat.completion",
        "created": 1,
        "model": "auto/eval",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": text}
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

/// Count events of a given kind in a store's full event log.
pub fn count_events(store: &Store, kind: EventKind) -> u64 {
    store
        .read_all_events()
        .expect("read events")
        .into_iter()
        .filter(|event| event.kind == kind)
        .count() as u64
}

/// Size of the database file on disk, in bytes.
pub fn db_bytes(db_path: &Path) -> u64 {
    std::fs::metadata(db_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}
