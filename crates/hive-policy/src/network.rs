use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

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
    enabled: bool,
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
            enabled: true,
        }
    }

    pub fn check(&self, host: &str, port: u16, resolved: &[IpAddr]) -> Result<(), PolicyError> {
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
        if resolved.is_empty() {
            return Err(PolicyError::NetworkAddressDenied(format!(
                "host {host} resolved to no addresses"
            )));
        }
        for ip in resolved {
            let class = classify_ip(*ip);
            if class != IpClass::Public {
                return Err(PolicyError::NetworkAddressDenied(format!(
                    "host {host} resolved to denied {class:?} address {ip}"
                )));
            }
        }
        Ok(())
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
}
