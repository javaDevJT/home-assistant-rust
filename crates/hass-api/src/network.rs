//! Client origin resolution for HTTP requests.
//!
//! Forwarded addresses are only accepted when explicitly enabled and when the
//! immediate socket peer is in the configured trusted-proxy networks. When a
//! chain is present, trusted hops are peeled from its right edge so a caller
//! cannot replace the client address by prepending a forged value.

use std::net::{IpAddr, SocketAddr};

use axum::http::{HeaderMap, HeaderName};
use hass_core::{Error, Result};
use ipnet::IpNet;

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// Policy for resolving the connecting client and deciding whether it is local.
#[derive(Clone, Debug, Default)]
pub struct NetworkPolicy {
    use_x_forwarded_for: bool,
    trusted_proxies: Vec<IpNet>,
}

/// Resolved client network identity for an HTTP request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientOrigin {
    /// The client's IP address, when the socket peer or forwarded chain supplied one.
    pub client_ip: Option<IpAddr>,
    /// Whether the resolved client address is loopback, private, or link-local.
    pub is_local: bool,
}

impl NetworkPolicy {
    /// Create a policy, accepting trusted proxies as individual IPs or CIDRs.
    ///
    /// Bare IPs are treated as host networks (`/32` for IPv4 and `/128` for
    /// IPv6). CIDRs must name their network address, matching HA's strict
    /// `ip_network` configuration parsing.
    pub fn new(use_x_forwarded_for: bool, trusted_proxies: &[String]) -> Result<Self> {
        let trusted_proxies = trusted_proxies
            .iter()
            .map(|proxy| parse_trusted_proxy(proxy))
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            use_x_forwarded_for,
            trusted_proxies,
        })
    }

    /// Resolve the client address from a socket peer and optional X-Forwarded-For.
    ///
    /// With no forwarding header, the socket peer is the client. A forwarding
    /// header is an error unless forwarding is enabled, at least one proxy is
    /// configured, and the socket peer is trusted. Missing socket information
    /// is never classified as local.
    pub fn resolve(&self, peer: Option<SocketAddr>, headers: &HeaderMap) -> Result<ClientOrigin> {
        let values = headers.get_all(X_FORWARDED_FOR).iter().collect::<Vec<_>>();
        if values.is_empty() {
            return Ok(origin(peer.map(|peer| peer.ip())));
        }

        // HA leaves requests with no identifiable socket peer untouched. In
        // this API that means the origin stays unknown and therefore not local.
        let Some(peer_ip) = peer.map(|peer| peer.ip()) else {
            return Ok(origin(None));
        };

        if !self.use_x_forwarded_for {
            return Err(invalid(
                "X-Forwarded-For is present but forwarded-client support is disabled",
            ));
        }
        if !self.is_trusted(peer_ip) {
            return Err(invalid(
                "X-Forwarded-For was received from an untrusted socket peer",
            ));
        }

        // Repeated X-Forwarded-For fields are valid. HA joins each field's
        // comma-separated values, preserving field order before right peeling.
        let mut forwarded_chain = Vec::new();
        for value in values {
            let value = value
                .to_str()
                .map_err(|_| invalid("malformed X-Forwarded-For header"))?;
            for address in value.split(',') {
                let address = address.trim();
                if address.is_empty() {
                    return Err(invalid("malformed X-Forwarded-For header"));
                }
                let address = address
                    .parse::<IpAddr>()
                    .map_err(|_| invalid("malformed X-Forwarded-For header"))?;
                forwarded_chain.push(address);
            }
        }
        if forwarded_chain.is_empty() {
            return Err(invalid("malformed X-Forwarded-For header"));
        }

        let client_ip = forwarded_chain
            .iter()
            .rev()
            .find(|address| !self.is_trusted(**address))
            .copied()
            // If every forwarded IP is itself trusted, HA chooses the
            // left-most address as the best available client identity.
            .or_else(|| forwarded_chain.first().copied())
            .ok_or_else(|| invalid("malformed X-Forwarded-For header"))?;

        Ok(origin(Some(client_ip)))
    }

    fn is_trusted(&self, address: IpAddr) -> bool {
        self.trusted_proxies
            .iter()
            .any(|network| network.contains(&address))
    }
}

fn parse_trusted_proxy(value: &str) -> Result<IpNet> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid("trusted proxy entries must not be empty"));
    }

    if let Ok(address) = value.parse::<IpAddr>() {
        return host_network(address);
    }

    let (address, prefix) = value
        .split_once('/')
        .ok_or_else(|| invalid(format!("invalid trusted proxy network: {value}")))?;
    if prefix.contains('/') {
        return Err(invalid(format!("invalid trusted proxy network: {value}")));
    }

    let address = address
        .trim()
        .parse::<IpAddr>()
        .map_err(|_| invalid(format!("invalid trusted proxy network: {value}")))?;
    let prefix = prefix
        .trim()
        .parse::<u8>()
        .map_err(|_| invalid(format!("invalid trusted proxy network: {value}")))?;
    let network = IpNet::new(address, prefix)
        .map_err(|_| invalid(format!("invalid trusted proxy network: {value}")))?;
    if network.network() != address {
        return Err(invalid(format!(
            "trusted proxy CIDR must use its network address: {value}"
        )));
    }
    Ok(network)
}

fn host_network(address: IpAddr) -> Result<IpNet> {
    let prefix = if address.is_ipv4() { 32 } else { 128 };
    IpNet::new(address, prefix).map_err(|_| invalid("invalid trusted proxy IP address"))
}

fn origin(client_ip: Option<IpAddr>) -> ClientOrigin {
    ClientOrigin {
        is_local: client_ip.is_some_and(is_local_address),
        client_ip,
    }
}

fn is_local_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_private() || address.is_loopback() || address.is_link_local()
        }
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return mapped.is_private() || mapped.is_loopback() || mapped.is_link_local();
            }
            address.is_loopback() || address.is_unique_local() || address.is_unicast_link_local()
        }
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidInput(message.into())
}

#[cfg(test)]
mod tests {
    use super::{ClientOrigin, NetworkPolicy, X_FORWARDED_FOR};
    use axum::http::{HeaderMap, HeaderValue};
    use std::net::SocketAddr;

    fn policy(trusted: &[&str]) -> NetworkPolicy {
        let trusted = trusted
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>();
        NetworkPolicy::new(true, &trusted).expect("valid test proxy configuration")
    }

    fn peer(address: &str) -> SocketAddr {
        address.parse().expect("valid socket address")
    }

    fn forwarded(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            X_FORWARDED_FOR,
            HeaderValue::from_str(value).expect("valid header value"),
        );
        headers
    }

    fn origin(ip: &str, is_local: bool) -> ClientOrigin {
        ClientOrigin {
            client_ip: Some(ip.parse().expect("valid IP address")),
            is_local,
        }
    }

    #[test]
    fn peels_trusted_hops_from_the_right_and_ignores_a_forged_left_prefix() {
        let policy = policy(&["10.0.0.0/8", "192.168.0.0/16"]);
        let headers = forwarded("203.0.113.250, 198.51.100.42, 192.168.1.4");

        let resolved = policy
            .resolve(Some(peer("10.2.0.8:8123")), &headers)
            .expect("trusted proxy chain");

        assert_eq!(resolved, origin("198.51.100.42", false));
    }

    #[test]
    fn all_trusted_forwarded_hops_fall_back_to_the_leftmost_address() {
        let policy = policy(&["10.0.0.0/8", "192.168.0.0/16"]);
        let headers = forwarded("192.168.1.20, 10.0.0.3");

        let resolved = policy
            .resolve(Some(peer("10.2.0.8:8123")), &headers)
            .expect("trusted proxy chain");

        assert_eq!(resolved, origin("192.168.1.20", true));
    }

    #[test]
    fn parses_host_proxy_entries_and_rejects_non_network_cidrs() {
        let host_proxy = policy(&["10.0.0.8"]);
        let headers = forwarded("198.51.100.5");
        assert_eq!(
            host_proxy
                .resolve(Some(peer("10.0.0.8:8123")), &headers)
                .expect("exact trusted host"),
            origin("198.51.100.5", false)
        );
        assert!(NetworkPolicy::new(true, &["10.0.0.8/24".to_owned()]).is_err());
        assert!(NetworkPolicy::new(true, &["10.0.0.0/33".to_owned()]).is_err());
        assert!(NetworkPolicy::new(true, &["".to_owned()]).is_err());
    }

    #[test]
    fn rejects_forwarding_when_disabled_unconfigured_or_from_an_untrusted_peer() {
        let headers = forwarded("198.51.100.5");
        let disabled = NetworkPolicy::new(false, &[]).expect("disabled policy");
        assert!(
            disabled
                .resolve(Some(peer("10.0.0.8:8123")), &headers)
                .is_err()
        );

        let unconfigured = NetworkPolicy::new(true, &[]).expect("empty trusted list");
        assert!(
            unconfigured
                .resolve(Some(peer("10.0.0.8:8123")), &headers)
                .is_err()
        );

        assert!(
            policy(&["10.0.0.0/8"])
                .resolve(Some(peer("192.168.1.8:8123")), &headers)
                .is_err()
        );
    }

    #[test]
    fn leaves_forwarded_origin_unknown_without_peer_and_does_not_mark_it_local() {
        let policy = policy(&["10.0.0.0/8"]);
        let headers = forwarded("198.51.100.5");
        let forwarded_without_peer = policy
            .resolve(None, &headers)
            .expect("HA skips forwarding without a socket peer");
        assert_eq!(
            forwarded_without_peer,
            ClientOrigin {
                client_ip: None,
                is_local: false
            }
        );

        let origin = NetworkPolicy::default()
            .resolve(None, &HeaderMap::new())
            .expect("missing peer without forwarding is allowed");
        assert_eq!(
            origin,
            ClientOrigin {
                client_ip: None,
                is_local: false
            }
        );
    }

    #[test]
    fn rejects_malformed_forwarding_headers_and_accepts_repeated_fields() {
        let policy = policy(&["10.0.0.0/8", "192.168.0.0/16"]);
        for value in ["", "198.51.100.5,", ",198.51.100.5", "not-an-ip"] {
            assert!(
                policy
                    .resolve(Some(peer("10.0.0.8:8123")), &forwarded(value))
                    .is_err(),
                "accepted malformed header {value:?}"
            );
        }

        let mut headers = HeaderMap::new();
        headers.append(X_FORWARDED_FOR, HeaderValue::from_static("198.51.100.5"));
        headers.append(X_FORWARDED_FOR, HeaderValue::from_static("192.168.1.8"));
        assert_eq!(
            policy
                .resolve(Some(peer("10.0.0.8:8123")), &headers)
                .expect("multiple header fields form a single chain"),
            origin("198.51.100.5", false)
        );
    }

    #[test]
    fn supports_ipv6_trusted_cidrs_and_classifies_private_and_link_local_addresses() {
        let policy = policy(&["fc00::/7"]);
        let headers = forwarded("2001:db8::42");
        let resolved = policy
            .resolve(Some(peer("[fd00::8]:8123")), &headers)
            .expect("trusted IPv6 proxy");
        assert_eq!(resolved, origin("2001:db8::42", false));

        let direct = NetworkPolicy::default()
            .resolve(Some(peer("[fe80::1]:8123")), &HeaderMap::new())
            .expect("link-local socket peer");
        assert_eq!(direct, origin("fe80::1", true));

        let mapped = NetworkPolicy::default()
            .resolve(Some(peer("[::ffff:192.168.1.8]:8123")), &HeaderMap::new())
            .expect("IPv4-mapped socket peer");
        assert_eq!(mapped, origin("::ffff:192.168.1.8", true));
    }

    #[test]
    fn classifies_ipv4_loopback_private_and_public_addresses() {
        let policy = NetworkPolicy::default();
        assert_eq!(
            policy
                .resolve(Some(peer("127.0.0.1:8123")), &HeaderMap::new())
                .unwrap(),
            origin("127.0.0.1", true)
        );
        assert_eq!(
            policy
                .resolve(Some(peer("172.16.16.1:8123")), &HeaderMap::new())
                .unwrap(),
            origin("172.16.16.1", true)
        );
        assert_eq!(
            policy
                .resolve(Some(peer("203.0.113.1:8123")), &HeaderMap::new())
                .unwrap(),
            origin("203.0.113.1", false)
        );
    }

    #[test]
    fn parses_ipv6_host_addresses_and_keeps_address_families_separate() {
        let v6_policy = policy(&["fd00::2"]);
        let headers = forwarded("198.51.100.5");
        assert_eq!(
            v6_policy
                .resolve(Some(peer("[fd00::2]:8123")), &headers)
                .unwrap(),
            origin("198.51.100.5", false)
        );

        let v4_policy = policy(&["10.0.0.0/8"]);
        assert!(
            v4_policy
                .resolve(Some(peer("[fd00::2]:8123")), &headers)
                .is_err()
        );
    }
}
