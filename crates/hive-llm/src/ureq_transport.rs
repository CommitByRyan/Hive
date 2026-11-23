//! Blocking `ureq` transport for the OpenAI-compatible chat endpoint.
//!
//! Tests stay on [`crate::MockTransport`]; this adapter is exercised by the
//! ignored smoke test at the bottom of the file. Retry/backoff is intentionally
//! left to [`crate::Gateway`]: 401/400 responses are returned as
//! [`HttpResponse`] values and classified by [`crate::RetryPolicy`].

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use crate::error::LlmError;
use crate::sse::{SseEvent, SseParser};
use crate::transport::{ChatTransport, HttpResponse};
use crate::wire::ChatRequest;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct UreqTransport {
    base_url: String,
    api_key: Option<String>,
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>, timeout: Duration) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let connect_timeout = timeout.min(DEFAULT_CONNECT_TIMEOUT);
        let config = ureq::config::Config::builder()
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .timeout_connect(Some(connect_timeout))
            .timeout_recv_body(Some(timeout))
            .timeout_per_read(Some(timeout))
            .build();
        Self {
            base_url,
            api_key,
            agent: ureq::Agent::new_with_config(config),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn post(&self, request: &ChatRequest) -> Result<ureq::http::Response<ureq::Body>, LlmError> {
        let body = serde_json::to_vec(request)?;
        let mut builder = self
            .agent
            .post(self.endpoint())
            .header("content-type", "application/json");
        if let Some(key) = self.api_key.as_deref().filter(|key| !key.is_empty()) {
            builder = builder.header("authorization", format!("Bearer {key}"));
        }
        builder.send(body).map_err(map_ureq_error)
    }
}

impl ChatTransport for UreqTransport {
    fn send(&self, request: &ChatRequest) -> Result<HttpResponse, LlmError> {
        let response = self.post(request)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let body = read_body(response.into_body())?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    fn stream(
        &self,
        request: &ChatRequest,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), LlmError>,
    ) -> Result<(), LlmError> {
        let response = self.post(request)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = read_body(response.into_body()).unwrap_or_default();
            return Err(LlmError::api(status, &String::from_utf8_lossy(&body)));
        }

        let mut reader = BufReader::new(response.into_body().into_reader());
        let mut parser = SseParser::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader
                .read_until(b'\n', &mut line)
                .map_err(|error| LlmError::Transport(error.to_string()))?;
            if read == 0 {
                break;
            }
            for event in parser.feed(&line) {
                forward_event(&event, sink)?;
            }
        }
        for event in parser.finish() {
            forward_event(&event, sink)?;
        }
        Ok(())
    }
}

fn read_body(body: ureq::Body) -> Result<Vec<u8>, LlmError> {
    let mut buffer = Vec::new();
    body.into_reader()
        .read_to_end(&mut buffer)
        .map_err(|error| LlmError::Transport(error.to_string()))?;
    Ok(buffer)
}

fn forward_event(
    event: &SseEvent,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), LlmError>,
) -> Result<(), LlmError> {
    match event {
        SseEvent::Data(data) => {
            let frame = format!("data: {data}\n\n");
            sink(frame.as_bytes())
        }
        SseEvent::Done => sink(b"data: [DONE]\n\n"),
        SseEvent::Comment(_) => Ok(()),
    }
}

fn map_ureq_error(error: ureq::Error) -> LlmError {
    match error {
        ureq::Error::Timeout(_) => LlmError::Timeout,
        other => LlmError::Transport(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Message;

    /// Opt-in smoke test against the local Magpie gateway.
    ///
    /// `HIVE_MAGPIE_URL` defaults to `http://127.0.0.1:3425/v1`;
    /// `HIVE_MAGPIE_API_KEY` is optional.
    #[test]
    #[ignore = "requires a running Magpie gateway"]
    fn smoke_against_local_magpie() {
        let base_url = std::env::var("HIVE_MAGPIE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:3425/v1".to_owned());
        let api_key = std::env::var("HIVE_MAGPIE_API_KEY").ok();
        let transport = UreqTransport::new(base_url, api_key, Duration::from_secs(60));
        let request = ChatRequest::new("auto/auto", vec![Message::user("Reply with: ok")])
            .with_max_tokens(16);

        let response = transport.send(&request).expect("transport");
        assert!(
            response.is_success(),
            "status {} body {}",
            response.status,
            response.text()
        );
        let completion = response.into_chat_response().expect("chat response");
        assert!(completion.text().is_some_and(|text| !text.is_empty()));
    }
}
