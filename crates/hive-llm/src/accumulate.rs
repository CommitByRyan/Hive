//! Accumulate streamed chat chunks into a final message and tool calls.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::LlmError;
use crate::sse::SseEvent;
use crate::wire::{parse_arguments, ChatChunk, ChunkChoice, FinishReason, Usage};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PendingToolCall {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompletedToolCall {
    pub index: usize,
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
    pub raw_arguments: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SseAction {
    Continue,
    Done,
}

/// Rolling view of a streamed completion.
#[derive(Clone, Debug, Default)]
pub struct StreamAccumulator {
    content: String,
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
    tool_calls: BTreeMap<usize, PendingToolCall>,
    chunks: usize,
    done: bool,
}

/// The deliverable name for the tool-call merge role of [`StreamAccumulator`].
pub type ToolCallAccumulator = StreamAccumulator;

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_chunk(&mut self, chunk: &ChatChunk) {
        self.chunks += 1;
        if let Some(usage) = &chunk.usage {
            self.usage = Some(usage.clone());
        }
        for choice in &chunk.choices {
            self.apply_choice(choice);
        }
    }

    pub fn apply_choice(&mut self, choice: &ChunkChoice) {
        if let Some(text) = &choice.delta.content {
            self.content.push_str(text);
        }
        if let Some(calls) = &choice.delta.tool_calls {
            for delta in calls {
                let entry = self
                    .tool_calls
                    .entry(delta.index)
                    .or_insert_with(|| PendingToolCall {
                        index: delta.index,
                        ..PendingToolCall::default()
                    });
                if entry.id.is_none() {
                    if let Some(id) = &delta.id {
                        if !id.is_empty() {
                            entry.id = Some(id.clone());
                        }
                    }
                }
                if let Some(function) = &delta.function {
                    if entry.name.is_none() {
                        if let Some(name) = &function.name {
                            if !name.is_empty() {
                                entry.name = Some(name.clone());
                            }
                        }
                    }
                    if let Some(arguments) = &function.arguments {
                        entry.arguments.push_str(arguments);
                    }
                }
            }
        }
        if choice.finish_reason.is_some() {
            self.finish_reason = choice.finish_reason.clone();
        }
    }

    pub fn apply_sse(&mut self, event: &SseEvent) -> Result<SseAction, LlmError> {
        match event {
            SseEvent::Comment(_) => Ok(SseAction::Continue),
            SseEvent::Done => {
                self.done = true;
                Ok(SseAction::Done)
            }
            SseEvent::Data(data) => {
                let chunk: ChatChunk = serde_json::from_str(data).map_err(|error| {
                    LlmError::InvalidResponse(format!("invalid stream chunk: {error}"))
                })?;
                self.apply_chunk(&chunk);
                Ok(SseAction::Continue)
            }
        }
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn finish_reason(&self) -> Option<&FinishReason> {
        self.finish_reason.as_ref()
    }

    pub fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }

    pub fn tool_calls(&self) -> &BTreeMap<usize, PendingToolCall> {
        &self.tool_calls
    }

    pub fn pending_tool_call(&self, index: usize) -> Option<&PendingToolCall> {
        self.tool_calls.get(&index)
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Validate accumulated tool arguments. A half-written argument string is
    /// never returned as a value: when the stream was cut by the token limit it
    /// becomes [`LlmError::TruncatedToolArguments`], otherwise
    /// [`LlmError::InvalidToolArguments`].
    pub fn finish(&self) -> Result<Vec<CompletedToolCall>, LlmError> {
        let truncated = self
            .finish_reason
            .as_ref()
            .is_some_and(FinishReason::is_truncated);
        let mut completed = Vec::with_capacity(self.tool_calls.len());
        for (index, pending) in &self.tool_calls {
            let name = pending
                .name
                .as_ref()
                .filter(|name| !name.is_empty())
                .cloned()
                .ok_or(LlmError::MissingToolName { call_index: *index })?;
            let arguments = match parse_arguments(&pending.arguments) {
                Ok(value) => value,
                Err(error) => {
                    if truncated {
                        return Err(LlmError::TruncatedToolArguments {
                            call_index: *index,
                            arguments: pending.arguments.clone(),
                        });
                    }
                    return Err(LlmError::InvalidToolArguments {
                        call_index: *index,
                        reason: error.to_string(),
                        arguments: pending.arguments.clone(),
                    });
                }
            };
            completed.push(CompletedToolCall {
                index: *index,
                id: pending.id.clone(),
                name,
                arguments,
                raw_arguments: pending.arguments.clone(),
            });
        }
        Ok(completed)
    }

    /// Rough completion-token estimate used until authoritative `usage` lands.
    pub fn estimated_completion_tokens(&self) -> u64 {
        let text = self.content.len() as u64;
        let arguments: u64 = self
            .tool_calls
            .values()
            .map(|call| call.arguments.len() as u64)
            .sum();
        (text + arguments).div_ceil(4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(value: serde_json::Value) -> ChatChunk {
        serde_json::from_value(value).expect("chunk fixture")
    }

    #[test]
    fn merges_tool_call_deltas_by_index() {
        let mut accumulator = StreamAccumulator::new();
        accumulator.apply_chunk(&chunk(serde_json::json!({
            "choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call_a", "type": "function",
                 "function": {"name": "lookup", "arguments": "{\"ci"}},
                {"index": 1, "id": "call_b", "type": "function",
                 "function": {"name": "fetch", "arguments": "{\"url\":"}}
            ]}}]
        })));
        accumulator.apply_chunk(&chunk(serde_json::json!({
            "choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 1, "function": {"arguments": "\"https://example.com\"}"}},
                {"index": 0, "function": {"arguments": "ty\":\"Paris\"}"}}
            ]}, "finish_reason": "tool_calls"}]
        })));

        let calls = accumulator.finish().expect("valid arguments");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id.as_deref(), Some("call_a"));
        assert_eq!(calls[0].name, "lookup");
        assert_eq!(calls[0].arguments, serde_json::json!({"city": "Paris"}));
        assert_eq!(calls[1].id.as_deref(), Some("call_b"));
        assert_eq!(
            calls[1].arguments,
            serde_json::json!({"url": "https://example.com"})
        );
        assert_eq!(accumulator.finish_reason(), Some(&FinishReason::ToolCalls));
    }

    #[test]
    fn truncated_tool_arguments_are_rejected() {
        let mut accumulator = StreamAccumulator::new();
        accumulator.apply_chunk(&chunk(serde_json::json!({
            "choices": [{"index": 0, "finish_reason": "length", "delta": {"tool_calls": [
                {"index": 0, "id": "call_x", "type": "function",
                 "function": {"name": "lookup", "arguments": "{\"city\""}}
            ]}}]
        })));
        let error = accumulator.finish().expect_err("half JSON must fail");
        match error {
            LlmError::TruncatedToolArguments { call_index, .. } => assert_eq!(call_index, 0),
            other => panic!("expected truncation error, got {other:?}"),
        }
    }

    #[test]
    fn invalid_arguments_without_length_are_reported() {
        let mut accumulator = StreamAccumulator::new();
        accumulator.apply_chunk(&chunk(serde_json::json!({
            "choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {"tool_calls": [
                {"index": 0, "id": "call_x", "function": {"name": "lookup", "arguments": "not json"}}
            ]}}]
        })));
        match accumulator.finish().expect_err("invalid JSON") {
            LlmError::InvalidToolArguments { call_index, .. } => assert_eq!(call_index, 0),
            other => panic!("expected invalid arguments, got {other:?}"),
        }
    }

    #[test]
    fn usage_only_lands_from_the_final_chunk() {
        let mut accumulator = StreamAccumulator::new();
        accumulator.apply_chunk(&chunk(serde_json::json!({
            "choices": [{"index": 0, "delta": {"content": "hel"}}],
            "usage": null
        })));
        assert!(accumulator.usage().is_none());
        accumulator.apply_chunk(&chunk(serde_json::json!({
            "choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": "lo"}}],
            "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}
        })));
        assert_eq!(accumulator.content(), "hello");
        assert_eq!(accumulator.usage().map(|usage| usage.total_tokens), Some(5));
        assert_eq!(accumulator.estimated_completion_tokens(), 2);
    }

    #[test]
    fn empty_arguments_mean_no_arguments() {
        let mut accumulator = StreamAccumulator::new();
        accumulator.apply_chunk(&chunk(serde_json::json!({
            "choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {"tool_calls": [
                {"index": 0, "id": "call_empty", "function": {"name": "ping"}}
            ]}}]
        })));
        let calls = accumulator.finish().expect("empty args are valid");
        assert_eq!(calls[0].arguments, serde_json::json!({}));
    }
}
