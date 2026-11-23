use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use crate::PolicyError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpClass {
    Loopback,
    Private,
    LinkLocal,
    Multicast,
    Unspecified,
    Broadcast,
    DocumentationReserved,
    Public,
}

pub fn classify_ip(ip: IpAddr) -> IpClass {
    match ip {
        IpAddr::V4(ip) => classify_v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => classify_v4(mapped),
            None => classify_v6(ip),
        },
    }
}

fn classify_v4(ip: Ipv4Addr) -> IpClass {
    let octets = ip.octets();
    let [first, second, _, _] = octets;
    if ip.is_unspecified() {
        IpClass::Unspecified
    } else if ip.is_loopback() {
        IpClass::Loopback
    } else if ip.is_private() {
        IpClass::Private
    } else if ip.is_link_local() {
        IpClass::LinkLocal
    } else if ip.is_multicast() {
        IpClass::Multicast
    } else if ip.is_broadcast() {
        IpClass::Broadcast
    } else if (first == 192 && second == 0 && octets[2] == 2)
        || (first == 198 && second == 51 && octets[2] == 100)
        || (first == 203 && second == 0 && octets[2] == 113)
        || (first == 100 && (64..=127).contains(&second))
        || (first == 192 && second == 0 && octets[2] == 0)
        || (first == 198 && (18..=19).contains(&second))
        || (first == 192 && second == 88 && octets[2] == 99)
        || (first == 0 && !ip.is_unspecified())
        || first >= 240
    {
        IpClass::DocumentationReserved
    } else {
        IpClass::Public
    }
}

fn classify_v6(ip: Ipv6Addr) -> IpClass {
    let segments = ip.segments();
    if ip.is_unspecified() {
        IpClass::Unspecified
    } else if ip.is_loopback() {
        IpClass::Loopback
    } else if ip.is_multicast() {
        IpClass::Multicast
    } else if segments[0] & 0xfe00 == 0xfc00 {
        IpClass::Private
    } else if segments[0] & 0xffc0 == 0xfe80 {
        IpClass::LinkLocal
    } else if (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || segments[0] == 0x2002
        || (segments[0] == 0x2001 && segments[1] == 0x0000)
    {
        IpClass::DocumentationReserved
    } else {
        IpClass::Public
    }
}

/// Default-deny egress policy. A host must match an allow rule, the port must
/// be allowed, and every resolved address must classify as public.
#[derive(Clone, Debug, Default)]
pub struct NetworkPolicy {
    allowed_hosts: Vec<String>,
    allowed_ports: Vec<u16>,
    exact_endpoints: Vec<ExactEndpoint>,
    enabled: bool,
}

/// A precise host:port rule for a configured gateway. Loopback is permitted
/// only when the configured host itself is a loopback address (for example
/// Magpie at `127.0.0.1:3425`); a non-loopback endpoint still requires every
/// resolved address to be public.
#[derive(Clone, Debug)]
struct ExactEndpoint {
    host: String,
    port: u16,
    loopback_permitted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AddressExpectation {
    PublicOnly,
    LoopbackOnly,
}

impl NetworkPolicy {
    pub fn deny_all() -> Self {
        Self::default()
    }

    pub fn allow(
        hosts: impl IntoIterator<Item = String>,
        ports: impl IntoIterator<Item = u16>,
    ) -> Self {
        Self {
            allowed_hosts: hosts.into_iter().map(|host| host.to_lowercase()).collect(),
            allowed_ports: ports.into_iter().collect(),
            exact_endpoints: Vec::new(),
            enabled: true,
        }
    }

    /// Allow exactly one configured `host:port`, with no suffix or port-range
    /// expansion. Loopback is admitted only when `host` itself is a loopback IP
    /// literal; every other address class is still rejected.
    pub fn allow_exact_endpoint(host: impl Into<String>, port: u16) -> Self {
        let host = host.into().trim().trim_end_matches('.').to_lowercase();
        let loopback_permitted = host
            .parse::<IpAddr>()
            .map(|ip| classify_ip(ip) == IpClass::Loopback)
            .unwrap_or(false);
        Self {
            allowed_hosts: Vec::new(),
            allowed_ports: Vec::new(),
            exact_endpoints: vec![ExactEndpoint {
                host,
                port,
                loopback_permitted,
            }],
            enabled: true,
        }
    }

    pub fn check(&self, host: &str, port: u16, resolved: &[IpAddr]) -> Result<(), PolicyError> {
        let (host, expectation) = self.match_host_port(host, port)?;
        if resolved.is_empty() {
            return Err(PolicyError::NetworkAddressDenied(format!(
                "host {host} resolved to no addresses"
            )));
        }
        for ip in resolved {
            if !self.address_allowed(*ip, expectation) {
                let class = classify_ip(*ip);
                return Err(PolicyError::NetworkAddressDenied(format!(
                    "host {host} resolved to denied {class:?} address {ip}"
                )));
            }
        }
        Ok(())
    }

    fn match_host_port(
        &self,
        host: &str,
        port: u16,
    ) -> Result<(String, AddressExpectation), PolicyError> {
        if !self.enabled {
            return Err(PolicyError::NetworkAddressDenied(
                "network egress is denied by default".to_owned(),
            ));
        }

        let host = host.trim().trim_end_matches('.').to_lowercase();
        if host.is_empty()
            || host.contains('@')
            || host.chars().any(|ch| ch.is_control() || ch.is_whitespace())
        {
            return Err(PolicyError::NetworkEndpointDenied(format!(
                "invalid host: {host}"
            )));
        }
        for endpoint in &self.exact_endpoints {
            if endpoint.host == host && endpoint.port == port {
                let expectation = if endpoint.loopback_permitted {
                    AddressExpectation::LoopbackOnly
                } else {
                    AddressExpectation::PublicOnly
                };
                return Ok((host, expectation));
            }
        }
        if !self.allowed_ports.contains(&port) {
            return Err(PolicyError::NetworkEndpointDenied(format!(
                "port {port} is not allowed"
            )));
        }
        if !self
            .allowed_hosts
            .iter()
            .any(|rule| host_matches(rule, &host))
        {
            return Err(PolicyError::NetworkEndpointDenied(format!(
                "host {host} is not allowed"
            )));
        }
        Ok((host, AddressExpectation::PublicOnly))
    }

    fn address_allowed(&self, ip: IpAddr, expectation: AddressExpectation) -> bool {
        match expectation {
            AddressExpectation::PublicOnly => classify_ip(ip) == IpClass::Public,
            AddressExpectation::LoopbackOnly => classify_ip(ip) == IpClass::Loopback,
        }
    }

    /// Resolve once and validate every A/AAAA address, pinning the validated
    /// set so a caller never lets its HTTP client resolve again.
    pub fn resolve_and_validate(
        &self,
        host: &str,
        port: u16,
    ) -> Result<PinnedEndpoint, PolicyError> {
        let (host, expectation) = self.match_host_port(host, port)?;
        let addresses = hive_platform::resolve_all(&host, port).map_err(|error| {
            PolicyError::NetworkAddressDenied(format!("cannot resolve {host}: {error}"))
        })?;
        if addresses.is_empty() {
            return Err(PolicyError::NetworkAddressDenied(format!(
                "host {host} resolved to no addresses"
            )));
        }
        for address in &addresses {
            if !self.address_allowed(address.ip(), expectation) {
                let class = classify_ip(address.ip());
                return Err(PolicyError::NetworkAddressDenied(format!(
                    "host {host} resolved to denied {class:?} address {}",
                    address.ip()
                )));
            }
        }
        Ok(PinnedEndpoint {
            host,
            port,
            addresses,
        })
    }

    /// Connect to a validated address and verify the peer, trying each pinned
    /// address in order. The original host is kept for TLS SNI / Host.
    pub fn connect_validated(
        &self,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<hive_platform::PinnedSocket, PolicyError> {
        let endpoint = self.resolve_and_validate(host, port)?;
        let mut last_error = None;
        for address in &endpoint.addresses {
            match hive_platform::connect_pinned(host, address.ip(), port, Some(timeout)) {
                Ok(socket) => return Ok(socket),
                Err(error) => last_error = Some(error),
            }
        }
        Err(PolicyError::NetworkEndpointDenied(format!(
            "cannot connect to any validated address for {host}: {}",
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "no addresses".to_owned())
        )))
    }

    /// Connect to an already-validated address without re-resolving or
    /// re-classifying. This is the pin primitive used after
    /// [`Self::resolve_and_validate`].
    pub fn connect_pinned_address(
        &self,
        host: &str,
        address: SocketAddr,
        timeout: Duration,
    ) -> Result<hive_platform::PinnedSocket, PolicyError> {
        hive_platform::connect_pinned(host, address.ip(), address.port(), Some(timeout)).map_err(
            |error| {
                PolicyError::NetworkEndpointDenied(format!("connect to {address} failed: {error}"))
            },
        )
    }

    /// Report the platform's connection-level pinning capability.
    pub fn net_capabilities(&self) -> hive_platform::NetCapabilities {
        hive_platform::net_capabilities()
    }
}

/// A host:port whose addresses have all been resolved once and validated.
#[derive(Clone, Debug)]
pub struct PinnedEndpoint {
    host: String,
    port: u16,
    addresses: Vec<SocketAddr>,
}

impl PinnedEndpoint {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }
}

fn host_matches(rule: &str, host: &str) -> bool {
    host == rule || host.ends_with(&format!(".{rule}"))
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    fn v4(value: &str) -> IpAddr {
        IpAddr::V4(value.parse::<Ipv4Addr>().expect("ipv4"))
    }

    #[test]
    fn classifies_ipv4_ranges() {
        assert_eq!(classify_ip(v4("127.0.0.1")), IpClass::Loopback);
        assert_eq!(classify_ip(v4("10.1.2.3")), IpClass::Private);
        assert_eq!(classify_ip(v4("172.16.0.1")), IpClass::Private);
        assert_eq!(classify_ip(v4("192.168.1.1")), IpClass::Private);
        assert_eq!(classify_ip(v4("169.254.169.254")), IpClass::LinkLocal);
        assert_eq!(classify_ip(v4("224.0.0.1")), IpClass::Multicast);
        assert_eq!(classify_ip(v4("0.0.0.0")), IpClass::Unspecified);
        assert_eq!(classify_ip(v4("255.255.255.255")), IpClass::Broadcast);
        assert_eq!(
            classify_ip(v4("192.0.2.10")),
            IpClass::DocumentationReserved
        );
        assert_eq!(classify_ip(v4("8.8.8.8")), IpClass::Public);
    }

    #[test]
    fn maps_ipv4_mapped_ipv6_before_classification() {
        let mapped: Ipv6Addr = "::ffff:127.0.0.1".parse().expect("mapped");
        assert_eq!(classify_ip(IpAddr::V6(mapped)), IpClass::Loopback);
        let mapped_private: Ipv6Addr = "::ffff:10.0.0.1".parse().expect("mapped private");
        assert_eq!(classify_ip(IpAddr::V6(mapped_private)), IpClass::Private);
    }

    #[test]
    fn classifies_ipv6_ranges() {
        let loopback: Ipv6Addr = "::1".parse().expect("loopback");
        let private: Ipv6Addr = "fd00::1".parse().expect("private");
        let link_local: Ipv6Addr = "fe80::1".parse().expect("link local");
        let documentation: Ipv6Addr = "2001:db8::1".parse().expect("documentation");
        assert_eq!(classify_ip(IpAddr::V6(loopback)), IpClass::Loopback);
        assert_eq!(classify_ip(IpAddr::V6(private)), IpClass::Private);
        assert_eq!(classify_ip(IpAddr::V6(link_local)), IpClass::LinkLocal);
        assert_eq!(
            classify_ip(IpAddr::V6(documentation)),
            IpClass::DocumentationReserved
        );
    }

    #[test]
    fn default_policy_denies_everything() {
        let policy = NetworkPolicy::deny_all();
        assert_eq!(
            policy
                .check("example.com", 443, &[v4("93.184.216.34")])
                .expect_err("denied")
                .code(),
            "network.address_denied"
        );
    }

    #[test]
    fn allow_rule_checks_host_port_and_all_resolved_ips() {
        let policy = NetworkPolicy::allow(["example.com".to_owned()], [443]);
        assert!(policy
            .check("api.example.com", 443, &[v4("93.184.216.34")])
            .is_ok());
        assert_eq!(
            policy
                .check("badexample.com", 443, &[v4("93.184.216.34")])
                .expect_err("host")
                .code(),
            "network.endpoint_denied"
        );
        assert_eq!(
            policy
                .check("example.com", 80, &[v4("93.184.216.34")])
                .expect_err("port")
                .code(),
            "network.endpoint_denied"
        );
        assert_eq!(
            policy
                .check("example.com", 443, &[v4("93.184.216.34"), v4("127.0.0.1")])
                .expect_err("rebinding")
                .code(),
            "network.address_denied"
        );
    }

    #[test]
    fn network_blocks_private_metadata_and_bad_ports() {
        let policy = NetworkPolicy::allow(["example.com".to_owned()], [443]);

        assert_eq!(
            policy
                .check("example.com", 80, &[v4("93.184.216.34")])
                .expect_err("port")
                .code(),
            "network.endpoint_denied"
        );
        assert_eq!(
            policy
                .check("example.com", 443, &[v4("169.254.169.254")])
                .expect_err("metadata")
                .code(),
            "network.address_denied"
        );
        assert_eq!(
            policy
                .check("example.com", 443, &[v4("127.0.0.1")])
                .expect_err("loopback")
                .code(),
            "network.address_denied"
        );
        assert_eq!(
            policy
                .check("example.com", 443, &[v4("10.0.0.1")])
                .expect_err("private")
                .code(),
            "network.address_denied"
        );
        assert_eq!(
            policy
                .check("example.com", 443, &[v4("169.254.10.10")])
                .expect_err("link local")
                .code(),
            "network.address_denied"
        );

        let mapped: Ipv6Addr = "::ffff:127.0.0.1".parse().expect("mapped");
        assert_eq!(
            policy
                .check("example.com", 443, &[IpAddr::V6(mapped)])
                .expect_err("mapped")
                .code(),
            "network.address_denied"
        );
    }

    #[test]
    fn network_pins_validated_ip() {
        let policy = NetworkPolicy::allow(["8.8.8.8".to_owned()], [53]);

        let endpoint = policy
            .resolve_and_validate("8.8.8.8", 53)
            .expect("resolve literal");
        assert_eq!(endpoint.host(), "8.8.8.8");
        assert_eq!(endpoint.port(), 53);
        assert_eq!(
            endpoint.addresses(),
            &[SocketAddr::from(([8, 8, 8, 8], 53))]
        );

        // The pin primitive connects to exactly the validated address and
        // verifies the peer, without a second DNS resolution.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("addr");
        let pinned = policy
            .connect_pinned_address("localhost", address, Duration::from_secs(2))
            .expect("pinned");
        assert_eq!(pinned.peer(), address);
        assert_eq!(pinned.host(), "localhost");

        let capabilities = policy.net_capabilities();
        assert!(capabilities.connection_pinning);
        assert!(capabilities.dns_rebinding_resistant);
    }

    #[test]
    fn exact_endpoint_permits_loopback_only_for_loopback_host() {
        let loopback = NetworkPolicy::allow_exact_endpoint("127.0.0.1", 3425);

        let endpoint = loopback
            .resolve_and_validate("127.0.0.1", 3425)
            .expect("loopback endpoint");
        assert_eq!(endpoint.host(), "127.0.0.1");
        assert_eq!(endpoint.port(), 3425);
        assert_eq!(
            endpoint.addresses(),
            &[SocketAddr::from(([127, 0, 0, 1], 3425))]
        );

        assert_eq!(
            loopback
                .resolve_and_validate("127.0.0.1", 9999)
                .expect_err("wrong port")
                .code(),
            "network.endpoint_denied"
        );
        assert_eq!(
            loopback
                .resolve_and_validate("127.0.0.2", 3425)
                .expect_err("wrong host")
                .code(),
            "network.endpoint_denied"
        );
        assert_eq!(
            loopback
                .resolve_and_validate("localhost", 3425)
                .expect_err("hostname is not the exact endpoint")
                .code(),
            "network.endpoint_denied"
        );

        // A loopback endpoint admits loopback but still rejects public addresses.
        assert!(loopback
            .check("127.0.0.1", 3425, &[v4("127.0.0.1")])
            .is_ok());
        assert_eq!(
            loopback
                .check("127.0.0.1", 3425, &[v4("8.8.8.8")])
                .expect_err("public not loopback")
                .code(),
            "network.address_denied"
        );

        // A non-loopback endpoint keeps the default public-only rule and does
        // not match suffixes or sibling hosts.
        let public = NetworkPolicy::allow_exact_endpoint("example.com", 443);
        assert!(public
            .check("example.com", 443, &[v4("93.184.216.34")])
            .is_ok());
        assert_eq!(
            public
                .check("api.example.com", 443, &[v4("93.184.216.34")])
                .expect_err("exact match only")
                .code(),
            "network.endpoint_denied"
        );
        assert_eq!(
            public
                .check("example.com", 443, &[v4("127.0.0.1")])
                .expect_err("rebinding")
                .code(),
            "network.address_denied"
        );
    }
}
