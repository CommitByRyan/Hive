use serde::Deserialize;
use thiserror::Error;

use crate::retry::FailureKind;
use crate::wire::de_opt_string;

/// Maximum number of body characters retained in an error for diagnostics.
pub const MAX_ERROR_BODY_CHARS: usize = 2048;

/// OpenAI-style error object. `code` is nullable in practice, so it must never
/// be unwrapped or used as the sole classification signal.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ApiErrorBody {
    #[serde(default, deserialize_with = "de_opt_string")]
    pub code: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub message: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub param: Option<String>,
    #[serde(default, rename = "type", deserialize_with = "de_opt_string")]
    pub kind: Option<String>,
}

#[derive(Deserialize, Default)]
struct ApiErrorEnvelope {
    #[serde(default)]
    error: ApiErrorBody,
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("api error {status}: {message}")]
    Api {
        status: u16,
        code: Option<String>,
        kind: Option<String>,
        message: String,
        body: String,
    },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("request timed out")]
    Timeout,
    #[error("request cancelled")]
    Cancelled,
    #[error("sse parse error at line {line}: {reason}")]
    Sse { line: usize, reason: String },
    #[error("tool call {call_index} arguments were truncated by the token limit")]
    TruncatedToolArguments {
        call_index: usize,
        arguments: String,
    },
    #[error("tool call {call_index} arguments are not valid JSON: {reason}")]
    InvalidToolArguments {
        call_index: usize,
        reason: String,
        arguments: String,
    },
    #[error("tool call {call_index} is missing a function name")]
    MissingToolName { call_index: usize },
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("unknown model `{0}`")]
    UnknownModel(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl LlmError {
    /// Build an API error from a raw response body, tolerating malformed JSON.
    pub fn api(status: u16, body: &str) -> Self {
        let parsed = serde_json::from_str::<ApiErrorEnvelope>(body)
            .ok()
            .map(|envelope| envelope.error)
            .unwrap_or_default();
        let fallback = truncate(body, MAX_ERROR_BODY_CHARS);
        let message = parsed
            .message
            .clone()
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| fallback.clone());
        Self::Api {
            status,
            code: parsed.code,
            kind: parsed.kind,
            message,
            body: fallback,
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// Coarse failure class consumed by [`crate::RetryPolicy`].
    pub fn failure_kind(&self) -> FailureKind {
        match self {
            Self::Api { .. } => FailureKind::Api,
            Self::Transport(_) => FailureKind::Transport,
            Self::Timeout => FailureKind::Timeout,
            Self::Cancelled => FailureKind::Cancelled,
            Self::TruncatedToolArguments { .. } => FailureKind::TruncatedToolArguments,
            Self::Sse { .. }
            | Self::InvalidToolArguments { .. }
            | Self::MissingToolName { .. }
            | Self::InvalidResponse(_)
            | Self::UnknownModel(_)
            | Self::Json(_) => FailureKind::InvalidResponse,
        }
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_accepts_null_code() {
        let error = LlmError::api(
            400,
            r#"{"error":{"code":null,"message":"model not found","param":null,"type":"invalid_request_error"}}"#,
        );
        match error {
            LlmError::Api {
                status,
                code,
                kind,
                message,
                ..
            } => {
                assert_eq!(status, 400);
                assert!(code.is_none());
                assert_eq!(kind.as_deref(), Some("invalid_request_error"));
                assert_eq!(message, "model not found");
            }
            other => panic!("expected api error, got {other:?}"),
        }
    }

    #[test]
    fn api_error_falls_back_to_raw_body() {
        let error = LlmError::api(502, "upstream exploded");
        assert_eq!(error.status(), Some(502));
        assert!(error.to_string().contains("upstream exploded"));
        assert_eq!(error.failure_kind(), FailureKind::Api);
    }

    #[test]
    fn api_error_accepts_numeric_code() {
        let body: ApiErrorBody =
            serde_json::from_str(r#"{"code":429,"type":"rate_limit","message":"slow down"}"#)
                .expect("numeric code should not fail parsing");
        assert_eq!(body.code.as_deref(), Some("429"));
        assert_eq!(body.kind.as_deref(), Some("rate_limit"));
    }
}
