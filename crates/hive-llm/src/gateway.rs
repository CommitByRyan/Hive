use crate::accumulate::StreamAccumulator;
use crate::error::LlmError;
use crate::retry::{RetryDecision, RetryPolicy};
use crate::transport::{collect_stream, ChatTransport};
use crate::wire::{ChatRequest, ChatResponse};

/// Minimal synchronous gateway loop.
///
/// It owns no runtime state and no events: callers (H04) map results onto
/// Node/Turn state. The blocking sleep is a placeholder until the async
/// cancellable adapter lands; tests use [`RetryPolicy::immediate`].
pub struct Gateway<T: ChatTransport> {
    transport: T,
    policy: RetryPolicy,
}

impl<T: ChatTransport> Gateway<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            policy: RetryPolicy::default(),
        }
    }

    pub fn with_policy(transport: T, policy: RetryPolicy) -> Self {
        Self { transport, policy }
    }

    pub fn policy(&self) -> RetryPolicy {
        self.policy
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn complete(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let mut attempt = 1;
        loop {
            let error = match self.transport.send(request) {
                Ok(response) if response.is_success() => return response.into_chat_response(),
                Ok(response) => response.into_api_error(),
                Err(error) => error,
            };
            match self.policy.classify_error(attempt, &error) {
                RetryDecision::Retry { delay, .. } => {
                    if !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                    attempt += 1;
                }
                RetryDecision::Fail => return Err(error),
            }
        }
    }

    pub fn collect_stream(&self, request: &ChatRequest) -> Result<StreamAccumulator, LlmError> {
        collect_stream(&self.transport, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{MockResponse, MockTransport};

    fn request() -> ChatRequest {
        ChatRequest::new("auto/auto", vec![crate::wire::Message::user("hi")])
    }

    fn completion(text: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1,
            "model": "auto/auto",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": text}
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
    }

    #[test]
    fn retries_429_then_succeeds() {
        let transport = MockTransport::new(vec![
            MockResponse::text(
                429,
                r#"{"error":{"code":null,"message":"rate limited","type":"rate_limit"}}"#,
            ),
            MockResponse::json(200, &completion("ok")),
        ]);
        let gateway = Gateway::with_policy(transport, RetryPolicy::immediate(3));
        let response = gateway
            .complete(&request())
            .expect("second attempt succeeds");
        assert_eq!(response.text(), Some("ok"));
        assert_eq!(gateway.transport().request_count(), 2);
    }

    #[test]
    fn gives_up_after_max_retries_on_429() {
        let transport = MockTransport::new(vec![
            MockResponse::text(429, "{}"),
            MockResponse::text(429, "{}"),
            MockResponse::text(429, "{}"),
            MockResponse::text(429, "{}"),
        ]);
        let gateway = Gateway::with_policy(transport, RetryPolicy::immediate(3));
        let error = gateway.complete(&request()).expect_err("must give up");
        assert_eq!(error.status(), Some(429));
        assert_eq!(gateway.transport().request_count(), 4);
    }

    #[test]
    fn does_not_retry_bad_model_400() {
        let transport = MockTransport::new(vec![
            MockResponse::text(
                400,
                r#"{"error":{"code":null,"message":"model not found","type":"invalid_request_error"}}"#,
            ),
            MockResponse::json(200, &completion("should not be reached")),
        ]);
        let gateway = Gateway::with_policy(transport, RetryPolicy::immediate(3));
        let error = gateway.complete(&request()).expect_err("400 is terminal");
        assert_eq!(error.status(), Some(400));
        assert_eq!(gateway.transport().request_count(), 1);
        assert!(error.to_string().contains("model not found"));
    }

    #[test]
    fn retries_transport_failures() {
        let transport = MockTransport::default();
        transport.push_failure(LlmError::Transport("connection reset".to_owned()));
        transport.push(MockResponse::json(200, &completion("recovered")));
        let gateway = Gateway::with_policy(transport, RetryPolicy::immediate(3));
        let response = gateway.complete(&request()).expect("retry succeeds");
        assert_eq!(response.text(), Some("recovered"));
        assert_eq!(gateway.transport().request_count(), 2);
    }

    #[test]
    fn stream_collects_content_usage_and_tool_calls() {
        let chunks = vec![
            ": heartbeat\n\n".to_owned(),
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n"
                .to_owned(),
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello \"}}]}\n\n".to_owned(),
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"world\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"city\\\":\"}}]}}]}\n\n"
                .to_owned(),
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Paris\\\"}\"}}]}}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":6,\"total_tokens\":10}}\n\n"
                .to_owned(),
            "data: [DONE]\n\n".to_owned(),
        ];
        let transport = MockTransport::new(vec![MockResponse::sse(chunks)]);
        let gateway = Gateway::new(transport);
        let accumulator = gateway.collect_stream(&request()).expect("stream parses");
        assert_eq!(accumulator.content(), "hello world");
        assert_eq!(
            accumulator.finish_reason(),
            Some(&crate::wire::FinishReason::ToolCalls)
        );
        assert_eq!(
            accumulator.usage().map(|usage| usage.total_tokens),
            Some(10)
        );
        let calls = accumulator.finish().expect("tool args valid");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "lookup");
        assert_eq!(calls[0].arguments, serde_json::json!({"city": "Paris"}));
    }
}
