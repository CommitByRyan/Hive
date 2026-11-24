use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use hive_types::{AgentSessionId, AttemptNo, MissionId, NodeId, RunId, TraceId};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

#[derive(Clone, Debug, Default)]
pub struct LogContext {
    pub trace_id: Option<TraceId>,
    pub span_id: Option<String>,
    pub mission_id: Option<MissionId>,
    pub run_id: Option<RunId>,
    pub node_id: Option<NodeId>,
    pub attempt_no: Option<AttemptNo>,
    pub session_id: Option<AgentSessionId>,
    pub tool: Option<String>,
    pub model: Option<String>,
}

impl LogContext {
    pub fn node(
        trace_id: TraceId,
        mission_id: MissionId,
        run_id: RunId,
        node_id: NodeId,
        attempt_no: AttemptNo,
    ) -> Self {
        Self {
            trace_id: Some(trace_id),
            mission_id: Some(mission_id),
            run_id: Some(run_id),
            node_id: Some(node_id),
            attempt_no: Some(attempt_no),
            ..Self::default()
        }
    }

    pub fn with_span(mut self, span_id: impl Into<String>) -> Self {
        self.span_id = Some(span_id.into());
        self
    }

    pub fn with_session(mut self, session_id: AgentSessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn with_tool(mut self, tool: impl Into<String>) -> Self {
        self.tool = Some(tool.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

pub fn new_span_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_owned()
}

pub fn emit(level: LogLevel, target: &str, message: &str, context: &LogContext) {
    let record = LogRecord {
        ts: now_rfc3339(),
        level,
        trace_id: context.trace_id,
        span_id: context.span_id.as_deref(),
        mission_id: context.mission_id,
        run_id: context.run_id,
        node_id: context.node_id,
        attempt_no: context.attempt_no,
        session_id: context.session_id,
        tool: context.tool.as_deref(),
        model: context.model.as_deref(),
        msg: message,
        target,
    };
    if let Ok(line) = serde_json::to_string(&record) {
        if CAPTURE_ENABLED.load(Ordering::Relaxed) {
            if let Some(buffer) = CAPTURE.get() {
                if let Ok(mut records) = buffer.lock() {
                    records.push(line.clone());
                }
            }
        }
        eprintln!("{line}");
    }
}

pub fn emit_error(target: &str, message: &str, context: &LogContext) {
    emit(LogLevel::Error, target, message, context);
}

pub fn emit_info(target: &str, message: &str, context: &LogContext) {
    emit(LogLevel::Info, target, message, context);
}

static CAPTURE: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static CAPTURE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Test-only hook that records the same JSON Lines written to stderr.
///
/// Capture is off by default, so production logging keeps its single
/// `eprintln!` path. The guard stops capture and clears the buffer on drop.
pub struct LogCapture {
    buffer: &'static Mutex<Vec<String>>,
}

pub fn start_capture() -> LogCapture {
    let buffer = CAPTURE.get_or_init(|| Mutex::new(Vec::new()));
    buffer.lock().expect("log capture poisoned").clear();
    CAPTURE_ENABLED.store(true, Ordering::SeqCst);
    LogCapture { buffer }
}

impl LogCapture {
    pub fn records(&self) -> Vec<String> {
        self.buffer.lock().expect("log capture poisoned").clone()
    }
}

impl Drop for LogCapture {
    fn drop(&mut self) {
        CAPTURE_ENABLED.store(false, Ordering::SeqCst);
        self.buffer.lock().expect("log capture poisoned").clear();
    }
}

#[derive(Serialize)]
struct LogRecord<'a> {
    ts: String,
    level: LogLevel,
    trace_id: Option<TraceId>,
    span_id: Option<&'a str>,
    mission_id: Option<MissionId>,
    run_id: Option<RunId>,
    node_id: Option<NodeId>,
    attempt_no: Option<AttemptNo>,
    session_id: Option<AgentSessionId>,
    tool: Option<&'a str>,
    model: Option<&'a str>,
    msg: &'a str,
    target: &'a str,
}

fn now_rfc3339() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    hive_store::rfc3339_from_ms(millis as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn log_record_has_fixed_fields_and_omits_none() {
        let context = LogContext::default();
        let record = LogRecord {
            ts: "2026-01-01T00:00:00.000Z".to_owned(),
            level: LogLevel::Info,
            trace_id: context.trace_id,
            span_id: context.span_id.as_deref(),
            mission_id: context.mission_id,
            run_id: context.run_id,
            node_id: context.node_id,
            attempt_no: context.attempt_no,
            session_id: context.session_id,
            tool: context.tool.as_deref(),
            model: context.model.as_deref(),
            msg: "safe message",
            target: "hive_runtime",
        };
        let value = serde_json::to_value(&record).expect("json");
        assert_eq!(
            value,
            json!({
                "ts": "2026-01-01T00:00:00.000Z",
                "level": "info",
                "trace_id": null,
                "span_id": null,
                "mission_id": null,
                "run_id": null,
                "node_id": null,
                "attempt_no": null,
                "session_id": null,
                "tool": null,
                "model": null,
                "msg": "safe message",
                "target": "hive_runtime"
            })
        );
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut expected = vec![
            "ts",
            "level",
            "trace_id",
            "span_id",
            "mission_id",
            "run_id",
            "node_id",
            "attempt_no",
            "session_id",
            "tool",
            "model",
            "msg",
            "target",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }
}
