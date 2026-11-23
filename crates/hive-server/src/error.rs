use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, message)
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
    }

    pub fn store(error: hive_store::StoreError) -> Self {
        match error {
            hive_store::StoreError::NotFound(message) => Self::not_found("not_found", message),
            hive_store::StoreError::DuplicateEvent(message) => {
                Self::conflict("duplicate_event", message)
            }
            hive_store::StoreError::SequenceExhausted => {
                Self::bad_request("sequence_exhausted", "event sequence exhausted")
            }
            hive_store::StoreError::ApprovalReplayed(_) => {
                Self::bad_request("approval_not_pending", "approval already decided")
            }
            hive_store::StoreError::ApprovalConflict(message) => {
                Self::conflict("approval_conflict", message)
            }
            hive_store::StoreError::InvalidSchedule(message) => {
                Self::bad_request("invalid_schedule", message)
            }
            hive_store::StoreError::InvalidTimezone(message) => {
                Self::bad_request("invalid_timezone", message)
            }
            hive_store::StoreError::RoutineLimit => {
                Self::bad_request("routine_limit", "routine limit reached")
            }
            _ => Self::internal(error.to_string()),
        }
    }

    pub fn store_lock() -> Self {
        Self::internal("store lock poisoned")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {
                "code": self.code,
                "message": self.message,
                "details": {},
            }
        });
        crate::json_response(self.status, body)
    }
}
