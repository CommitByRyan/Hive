//! Connection-level IP pinning primitives.
//!
//! These primitives resolve a host once, connect to a specific validated IP and
//! re-verify the connected peer. They do not perform policy classification:
//! `hive-policy` decides which addresses are allowed and calls [`connect_pinned`]
//! only with already-validated addresses.

use std::io;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetCapabilities {
    /// The TCP primitive can connect to a validated IP and verify the peer.
    pub connection_pinning: bool,
    /// End-to-end DNS-rebinding resistance: every HTTP/TLS request goes through
    /// the pinned socket. True now that `hive-llm`'s `UreqTransport` resolves
    /// once through `NetworkPolicy` and connects only to validated addresses.
    pub dns_rebinding_resistant: bool,
    pub note: &'static str,
}

pub fn net_capabilities() -> NetCapabilities {
    NetCapabilities {
        connection_pinning: true,
        dns_rebinding_resistant: true,
        note: "hive-llm ureq transport resolves once through NetworkPolicy and connects to validated addresses only; generic network tools remain closed",
    }
}

/// Resolve `host:port` exactly once and return every address. The caller must
/// validate the full set before connecting.
pub fn resolve_all(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    let mut addresses = Vec::new();
    for address in (host, port).to_socket_addrs()? {
        addresses.push(address);
    }
    Ok(addresses)
}

/// A TCP stream connected to an already-validated IP, together with the
/// verified peer address and the original host (kept for TLS SNI / Host).
pub struct PinnedSocket {
    host: String,
    peer: SocketAddr,
    stream: TcpStream,
}

impl PinnedSocket {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }

    pub fn into_stream(self) -> TcpStream {
        self.stream
    }
}

/// Connect to a single validated IP and re-verify the actual peer address.
///
/// No DNS resolution happens here: the caller passes an [`IpAddr`] it already
/// validated. TLS SNI / Host must still use `host`, which is preserved on the
/// returned socket for that purpose.
pub fn connect_pinned(
    host: &str,
    ip: IpAddr,
    port: u16,
    timeout: Option<Duration>,
) -> io::Result<PinnedSocket> {
    let expected = SocketAddr::new(ip, port);
    let stream = match timeout {
        Some(timeout) => TcpStream::connect_timeout(&expected, timeout)?,
        None => TcpStream::connect(expected)?,
    };
    verify_peer(&stream, expected)?;
    Ok(PinnedSocket {
        host: host.to_owned(),
        peer: expected,
        stream,
    })
}

/// Re-check that `stream` is connected to exactly `expected`.
pub fn verify_peer(stream: &TcpStream, expected: SocketAddr) -> io::Result<()> {
    let peer = stream.peer_addr()?;
    if peer != expected {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!("peer {peer} does not match validated {expected}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::time::Duration;

    use super::*;

    #[test]
    fn resolve_all_handles_ip_literal_without_dns() {
        let addresses = resolve_all("8.8.8.8", 53).expect("resolve literal");
        assert_eq!(addresses, vec![SocketAddr::from(([8, 8, 8, 8], 53))]);
    }

    #[test]
    fn connect_pinned_connects_to_validated_ip_and_verifies_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let expected = listener.local_addr().expect("addr");

        let pinned = connect_pinned(
            "localhost",
            expected.ip(),
            expected.port(),
            Some(Duration::from_secs(2)),
        )
        .expect("pinned connect");
        assert_eq!(pinned.peer(), expected);
        assert_eq!(pinned.host(), "localhost");
    }

    #[test]
    fn verify_peer_rejects_mismatched_address() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let actual = listener.local_addr().expect("addr");
        let stream = TcpStream::connect(actual).expect("connect");

        assert!(verify_peer(&stream, actual).is_ok());

        let wrong = SocketAddr::from(([127, 0, 0, 1], actual.port().wrapping_add(1).max(1)));
        assert!(verify_peer(&stream, wrong).is_err());
    }
}
