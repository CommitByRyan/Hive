//! Transport seam: the gateway core never performs network I/O itself.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use serde::de::DeserializeOwned;

use crate::accumulate::{SseAction, StreamAccumulator};
use crate::error::LlmError;
use crate::sse::SseParser;
use crate::wire::{ChatRequest, ChatResponse};

#[derive(Clone, Debug, Default)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: BTreeMap::new(),
            body: body.into(),
        }
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T, LlmError> {
        serde_json::from_slice(&self.body)
            .map_err(|error| LlmError::InvalidResponse(format!("invalid response body: {error}")))
    }

    pub fn into_chat_response(self) -> Result<ChatResponse, LlmError> {
        if !self.is_success() {
            return Err(self.into_api_error());
        }
        self.json()
    }

    pub fn into_api_error(self) -> LlmError {
        LlmError::api(self.status, &self.text())
    }
}

/// Blocking transport abstraction. Real HTTP lives behind this trait (H04);
/// tests inject [`MockTransport`] so the default test suite never touches the
/// network.
pub trait ChatTransport {
    fn send(&self, request: &ChatRequest) -> Result<HttpResponse, LlmError>;

    /// Stream raw body chunks to `sink`. The default implementation performs a
    /// single non-streaming send and forwards the whole body.
    fn stream(
        &self,
        request: &ChatRequest,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), LlmError>,
    ) -> Result<(), LlmError> {
        let response = self.send(request)?;
        if !response.is_success() {
            return Err(response.into_api_error());
        }
        sink(&response.body)
    }
}

/// Drive a streaming request through [`SseParser`] and [`StreamAccumulator`].
///
/// Call [`StreamAccumulator::finish`] afterwards to validate tool arguments.
pub fn collect_stream(
    transport: &dyn ChatTransport,
    request: &ChatRequest,
) -> Result<StreamAccumulator, LlmError> {
    let mut parser = SseParser::new();
    let mut accumulator = StreamAccumulator::new();
    let mut done = false;
    transport.stream(request, &mut |chunk| {
        if done {
            return Ok(());
        }
        for event in parser.feed(chunk) {
            if accumulator.apply_sse(&event)? == SseAction::Done {
                done = true;
                break;
            }
        }
        Ok(())
    })?;
    if !done {
        for event in parser.finish() {
            if accumulator.apply_sse(&event)? == SseAction::Done {
                break;
            }
        }
    }
    Ok(accumulator)
}

/// Scripted transport response used by tests and local fake servers.
pub struct MockResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    chunks: Option<Vec<Vec<u8>>>,
    failure: Option<LlmError>,
}

impl std::fmt::Debug for MockResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MockResponse")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .field("chunked", &self.chunks.is_some())
            .field("failure", &self.failure)
            .finish()
    }
}

impl MockResponse {
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: BTreeMap::new(),
            body: body.into().into_bytes(),
            chunks: None,
            failure: None,
        }
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self::text(
            status,
            serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned()),
        )
    }

    /// SSE response whose body is delivered in the given chunk boundaries.
    pub fn sse(chunks: Vec<String>) -> Self {
        Self::sse_bytes(chunks.into_iter().map(String::into_bytes).collect())
    }

    pub fn sse_bytes(chunks: Vec<Vec<u8>>) -> Self {
        let body = chunks.concat();
        Self {
            status: 200,
            headers: BTreeMap::new(),
            body,
            chunks: Some(chunks),
            failure: None,
        }
    }

    pub fn failure(error: LlmError) -> Self {
        Self {
            status: 0,
            headers: BTreeMap::new(),
            body: Vec::new(),
            chunks: None,
            failure: Some(error),
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    fn into_http_response(self) -> HttpResponse {
        HttpResponse {
            status: self.status,
            headers: self.headers,
            body: self.body,
        }
    }
}

enum Scripted {
    Response(MockResponse),
    Failure(LlmError),
}

/// In-memory transport. Each call consumes the next scripted response and
/// records the request so tests can assert retry counts.
#[derive(Default)]
pub struct MockTransport {
    script: Mutex<VecDeque<Scripted>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl MockTransport {
    pub fn new(script: Vec<MockResponse>) -> Self {
        let transport = Self::default();
        for response in script {
            transport.push(response);
        }
        transport
    }

    pub fn push(&self, response: MockResponse) {
        self.script
            .lock()
            .expect("mock script lock")
            .push_back(Scripted::Response(response));
    }

    pub fn push_failure(&self, error: LlmError) {
        self.script
            .lock()
            .expect("mock script lock")
            .push_back(Scripted::Failure(error));
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("mock requests lock").len()
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("mock requests lock").clone()
    }

    fn record(&self, request: &ChatRequest) {
        self.requests
            .lock()
            .expect("mock requests lock")
            .push(request.clone());
    }

    fn next(&self) -> Result<Scripted, LlmError> {
        self.script
            .lock()
            .expect("mock script lock")
            .pop_front()
            .ok_or_else(|| LlmError::Transport("mock script exhausted".to_owned()))
    }
}

impl ChatTransport for MockTransport {
    fn send(&self, request: &ChatRequest) -> Result<HttpResponse, LlmError> {
        self.record(request);
        match self.next()? {
            Scripted::Response(response) => Ok(response.into_http_response()),
            Scripted::Failure(error) => Err(error),
        }
    }

    fn stream(
        &self,
        request: &ChatRequest,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), LlmError>,
    ) -> Result<(), LlmError> {
        self.record(request);
        match self.next()? {
            Scripted::Failure(error) => Err(error),
            Scripted::Response(response) => {
                if !(200..300).contains(&response.status) {
                    return Err(response.into_http_response().into_api_error());
                }
                match response.chunks {
                    Some(chunks) => {
                        for chunk in chunks {
                            sink(&chunk)?;
                        }
                        Ok(())
                    }
                    None => sink(&response.body),
                }
            }
        }
    }
}
