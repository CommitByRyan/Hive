use hive_store::Store;
use hive_types::EventInput;

use crate::{ToolError, ToolResult};
use hive_policy::ErrorCode;

pub trait AuditSink {
    fn append(&mut self, event: &EventInput) -> ToolResult<()>;
}

pub struct StoreAuditSink<'a> {
    store: &'a mut Store,
}

impl<'a> StoreAuditSink<'a> {
    pub fn new(store: &'a mut Store) -> Self {
        Self { store }
    }
}

impl AuditSink for StoreAuditSink<'_> {
    fn append(&mut self, event: &EventInput) -> ToolResult<()> {
        self.store
            .append_event(event)
            .map(|_| ())
            .map_err(ToolError::from)
    }
}

#[derive(Clone, Debug, Default)]
pub struct RecordingAuditSink {
    events: Vec<EventInput>,
}

impl RecordingAuditSink {
    pub fn events(&self) -> &[EventInput] {
        &self.events
    }
}

impl AuditSink for RecordingAuditSink {
    fn append(&mut self, event: &EventInput) -> ToolResult<()> {
        self.events.push(event.clone());
        Ok(())
    }
}

pub(crate) fn fail_closed(error: impl std::fmt::Display) -> ToolError {
    ToolError::new(ErrorCode::ToolAuditFailed, error.to_string())
}
