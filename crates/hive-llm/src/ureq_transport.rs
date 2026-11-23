//! Blocking `ureq` transport for the OpenAI-compatible chat endpoint.
//!
//! Tests stay on [`crate::MockTransport`]; this adapter is exercised by the
//! ignored smoke test at the bottom of the file. Retry/backoff is intentionally
//! left to [`crate::Gateway`]: 401/400 responses are returned as
//! [`HttpResponse`] values and classified by [`crate::RetryPolicy`].

use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::time::Duration;

use hive_policy::{NetworkPolicy, PolicyError};
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{Connector, NextTimeout, RustlsConnector, TcpConnector};

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
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        timeout: Duration,
        network: NetworkPolicy,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let connect_timeout = timeout.min(DEFAULT_CONNECT_TIMEOUT);
        let config = ureq::config::Config::builder()
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .timeout_connect(Some(connect_timeout))
            .timeout_recv_body(Some(timeout))
            .timeout_per_read(Some(timeout))
            .max_redirects(0)
            .proxy(None)
            .build();
        let connector = ().chain(TcpConnector::default()).chain(RustlsConnector::default());
        let resolver = PinnedResolver { network };
        Self {
            base_url,
            api_key,
            agent: ureq::Agent::with_parts(config, connector, resolver),
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

/// Resolves the request URI exactly once through [`NetworkPolicy`], returning
/// only validated socket addresses. This prevents ureq's default resolver from
/// performing a second, unvalidated DNS lookup between policy check and connect.
#[derive(Debug)]
struct PinnedResolver {
    network: NetworkPolicy,
}

impl Resolver for PinnedResolver {
    fn resolve(
        &self,
        uri: &Uri,
        _config: &Config,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let scheme = uri
            .scheme()
            .ok_or_else(|| ureq::Error::BadUri("missing scheme".to_owned()))?;
        let authority = uri
            .authority()
            .ok_or_else(|| ureq::Error::BadUri("missing authority".to_owned()))?;
        let host = authority
            .host()
            .strip_prefix('[')
            .and_then(|suffix| suffix.strip_suffix(']'))
            .unwrap_or_else(|| authority.host());
        let port = authority.port_u16().or_else(|| match scheme.as_str() {
            "https" => Some(443),
            "http" => Some(80),
            _ => None,
        });
        let port = port.ok_or_else(|| ureq::Error::BadUri("missing port".to_owned()))?;

        let endpoint = self
            .network
            .resolve_and_validate(host, port)
            .map_err(map_policy_error)?;

        let mut addresses = ResolvedSocketAddrs::from_fn(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
        for address in endpoint.addresses() {
            if addresses.try_push(*address).is_err() {
                break;
            }
        }
        Ok(addresses)
    }
}

#[derive(Debug)]
struct EgressDenied {
    code: &'static str,
    message: String,
}

impl fmt::Display for EgressDenied {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for EgressDenied {}

fn map_policy_error(error: PolicyError) -> ureq::Error {
    ureq::Error::Other(Box::new(EgressDenied {
        code: error.code(),
        message: error.to_string(),
    }))
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
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use crate::wire::Message;

    /// Binds a loopback listener, accepts exactly one connection, drains the
    /// request head and writes the canned HTTP response before exiting.
    fn loopback_server(response: String) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut stream, _peer) = listener.accept().expect("accept");
            drain_request(&mut stream);
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });
        (address, handle)
    }

    /// Read the request head and, when a `Content-Length` is present, the whole
    /// body. Closing a Windows socket with unread body bytes would otherwise
    /// reset the client connection before it can read the response.
    fn drain_request(stream: &mut std::net::TcpStream) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        let header_end = loop {
            match stream.read(&mut chunk) {
                Ok(0) => return,
                Ok(read) => {
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(position) =
                        buffer.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        break position;
                    }
                }
                Err(_) => return,
            }
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|line| line.trim().strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let body_start = header_end + 4;
        let mut remaining = content_length.saturating_sub(buffer.len() - body_start);
        while remaining > 0 {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => remaining = remaining.saturating_sub(read),
                Err(_) => break,
            }
        }
    }

    fn ok_response() -> &'static str {
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
    }

    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(name);
            std::env::set_var(name, value);
            EnvGuard(vec![(name, previous)])
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, previous) in &self.0 {
                match previous {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    fn endpoint_policy(base_url: &str) -> NetworkPolicy {
        let uri = base_url
            .parse::<ureq::http::Uri>()
            .expect("parse magpie url");
        let scheme = uri.scheme().expect("scheme");
        let authority = uri.authority().expect("authority");
        let host = authority
            .host()
            .strip_prefix('[')
            .and_then(|suffix| suffix.strip_suffix(']'))
            .unwrap_or_else(|| authority.host());
        let port = authority.port_u16().or_else(|| match scheme.as_str() {
            "https" => Some(443),
            "http" => Some(80),
            _ => None,
        });
        NetworkPolicy::allow_exact_endpoint(host, port.expect("port"))
    }

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
        let transport = UreqTransport::new(
            base_url.clone(),
            api_key,
            Duration::from_secs(60),
            endpoint_policy(&base_url),
        );
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

    #[test]
    fn pinned_egress_connects_to_allowlisted_loopback_endpoint() {
        let (server_addr, handle) = loopback_server(ok_response().to_owned());
        let transport = UreqTransport::new(
            format!("http://127.0.0.1:{}", server_addr.port()),
            None,
            Duration::from_secs(10),
            NetworkPolicy::allow_exact_endpoint("127.0.0.1", server_addr.port()),
        );
        let request = ChatRequest::new("auto/auto", vec![Message::user("hi")]);

        let response = transport.send(&request).expect("pinned egress");
        assert!(response.is_success());
        assert_eq!(response.text(), "ok");
        handle.join().expect("server thread");
    }

    #[test]
    fn pinned_egress_rejects_denied_endpoint_before_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let address = listener.local_addr().expect("addr");

        let transport = UreqTransport::new(
            format!("http://127.0.0.1:{}", address.port()),
            None,
            Duration::from_secs(10),
            NetworkPolicy::deny_all(),
        );
        let request = ChatRequest::new("auto/auto", vec![Message::user("hi")]);

        let error = transport.send(&request).expect_err("denied before connect");
        assert!(
            matches!(&error, LlmError::Transport(message) if message.contains("network.address_denied")),
            "unexpected error: {error:?}"
        );

        std::thread::sleep(std::time::Duration::from_millis(150));
        match listener.accept() {
            Ok(_) => panic!("connection was made despite deny_all policy"),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("unexpected accept error: {e}"),
        }
    }

    #[test]
    fn pinned_egress_does_not_follow_redirects() {
        let target = TcpListener::bind("127.0.0.1:0").expect("target bind");
        target.set_nonblocking(true).expect("nonblocking");
        let target_port = target.local_addr().expect("target addr").port();

        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let (server_addr, handle) = loopback_server(response);
        let transport = UreqTransport::new(
            format!("http://127.0.0.1:{}", server_addr.port()),
            None,
            Duration::from_secs(10),
            NetworkPolicy::allow_exact_endpoint("127.0.0.1", server_addr.port()),
        );
        let request = ChatRequest::new("auto/auto", vec![Message::user("hi")]);

        let response = transport.send(&request).expect("redirect returned as-is");
        assert_eq!(response.status, 302);
        handle.join().expect("server thread");

        std::thread::sleep(std::time::Duration::from_millis(150));
        match target.accept() {
            Ok(_) => panic!("redirect target was contacted"),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("unexpected target accept error: {e}"),
        }
    }

    #[test]
    fn pinned_egress_ignores_proxy_environment() {
        let proxy = TcpListener::bind("127.0.0.1:0").expect("proxy bind");
        proxy.set_nonblocking(true).expect("nonblocking");
        let proxy_port = proxy.local_addr().expect("proxy addr").port();
        let _http_guard = EnvGuard::set("HTTP_PROXY", &format!("http://127.0.0.1:{proxy_port}"));
        let _lower_guard = EnvGuard::set("http_proxy", &format!("http://127.0.0.1:{proxy_port}"));

        let (server_addr, handle) = loopback_server(ok_response().to_owned());
        let transport = UreqTransport::new(
            format!("http://127.0.0.1:{}", server_addr.port()),
            None,
            Duration::from_secs(10),
            NetworkPolicy::allow_exact_endpoint("127.0.0.1", server_addr.port()),
        );
        let request = ChatRequest::new("auto/auto", vec![Message::user("hi")]);

        let response = transport
            .send(&request)
            .expect("direct egress, proxy ignored");
        assert!(response.is_success());
        handle.join().expect("server thread");

        std::thread::sleep(std::time::Duration::from_millis(150));
        match proxy.accept() {
            Ok(_) => panic!("proxy was contacted"),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("unexpected proxy accept error: {e}"),
        }
    }
}
