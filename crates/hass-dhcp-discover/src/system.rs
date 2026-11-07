use std::collections::HashSet;
use std::fs;
use std::future::Future;
use std::net::UdpSocket as StdUdpSocket;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::{NeighborEntry, NetworkProvider, NetworkSnapshot, ScanError};

/// Linux system integration backed by interface enumeration, `/etc/resolv.conf`,
/// `/proc/net/route`, and `/proc/net/arp`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemNetworkProvider;

impl NetworkProvider for SystemNetworkProvider {
    fn resolver_config_revision(
        &self,
    ) -> impl Future<Output = Option<crate::ResolverConfigRevision>> + Send + '_ {
        let revision = fs::metadata("/etc/resolv.conf").ok().and_then(|metadata| {
            Some(crate::ResolverConfigRevision {
                modified_ns: metadata
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_nanos(),
                size: metadata.len(),
            })
        });
        std::future::ready(revision)
    }

    async fn network_snapshot(
        &self,
        selected_local_ip: Option<Ipv4Addr>,
    ) -> Result<NetworkSnapshot, ScanError> {
        if !cfg!(target_os = "linux") {
            return Err(ScanError::Provider(
                "system route and neighbor providers are implemented for Linux only".to_owned(),
            ));
        }
        let local_ip = match selected_local_ip {
            Some(ip) => ip,
            None => select_route_source()?,
        };
        let interfaces =
            if_addrs::get_if_addrs().map_err(|error| ScanError::Provider(error.to_string()))?;
        let interface_prefix = interfaces
            .iter()
            .find_map(|interface| match &interface.addr {
                if_addrs::IfAddr::V4(address) if address.ip == local_ip => {
                    mask_prefix(address.netmask)
                }
                _ => None,
            });
        let nameservers = parse_resolv_conf(&fs::read_to_string("/etc/resolv.conf")?);
        let route_table = fs::read_to_string("/proc/net/route")?;
        let (default_router, _) = parse_proc_routes(&route_table, local_ip);
        Ok(NetworkSnapshot {
            local_ip,
            interface_prefix,
            default_router,
            nameservers,
        })
    }

    async fn read_neighbors<'a>(
        &'a self,
        ips: &'a [Ipv4Addr],
    ) -> Result<Vec<NeighborEntry>, ScanError> {
        if !cfg!(target_os = "linux") {
            return Err(ScanError::Provider(
                "system neighbor-table provider is implemented for Linux only".to_owned(),
            ));
        }
        let requested = ips.iter().copied().collect::<HashSet<_>>();
        let table = fs::read_to_string("/proc/net/arp")?;
        Ok(parse_proc_neighbors(&table)
            .into_iter()
            .filter(|entry| requested.contains(&entry.ip))
            .collect())
    }

    async fn probe_neighbors<'a>(&'a self, ips: &'a [Ipv4Addr]) -> Result<(), ScanError> {
        if ips.is_empty() {
            return Ok(());
        }
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        for ip in ips {
            // Match aiodiscover's empty UDP-to-port-80 cache-population
            // probe. This batch has a deadline and dropping the future
            // closes the socket.
            let destination = SocketAddrV4::new(*ip, 80);
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            if tokio::time::timeout(remaining, socket.send_to(&[], destination))
                .await
                .is_err()
            {
                break;
            }
        }
        Ok(())
    }
}

fn select_route_source() -> Result<Ipv4Addr, ScanError> {
    // Connecting a UDP socket selects the route's source address without
    // transmitting a packet. These targets match aiodiscover's selection
    // order, including its loopback last resort.
    for target in [
        Ipv4Addr::new(10, 255, 255, 255),
        Ipv4Addr::new(224, 0, 0, 251),
        Ipv4Addr::new(8, 8, 8, 8),
        Ipv4Addr::LOCALHOST,
    ] {
        let socket = match StdUdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)) {
            Ok(socket) => socket,
            Err(_) => continue,
        };
        if socket.connect(SocketAddrV4::new(target, 1)).is_ok()
            && let Ok(address) = socket.local_addr()
            && let std::net::SocketAddr::V4(address) = address
            && !address.ip().is_unspecified()
        {
            return Ok(*address.ip());
        }
    }
    Err(ScanError::Provider(
        "could not select a local IPv4 address".to_owned(),
    ))
}

pub(crate) fn parse_resolv_conf(contents: &str) -> Vec<std::net::IpAddr> {
    let mut nameservers = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        if parts.next() != Some("nameserver") {
            continue;
        }
        let Some(value) = parts.next() else {
            continue;
        };
        if let Ok(ip) = value.parse::<std::net::IpAddr>()
            && !nameservers.contains(&ip)
        {
            nameservers.push(ip);
        }
    }
    nameservers
}

pub(crate) fn mask_prefix(mask: Ipv4Addr) -> Option<u8> {
    let bits = u32::from(mask);
    let prefix = bits.leading_ones() as u8;
    let expected = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (bits == expected).then_some(prefix)
}

/// Returns the first up default gateway and the longest directly connected
/// prefix matching `local_ip` from a Linux `/proc/net/route` table.
pub(crate) fn parse_proc_routes(
    contents: &str,
    local_ip: Ipv4Addr,
) -> (Option<Ipv4Addr>, Option<u8>) {
    let mut default_router = None;
    let mut prefix = None;
    for line in contents.lines().skip(1) {
        let columns = line.split_whitespace().collect::<Vec<_>>();
        if columns.len() < 8 {
            continue;
        }
        let (Some(destination), Some(gateway), Some(flags), Some(mask)) = (
            parse_proc_ipv4(columns[1]),
            parse_proc_ipv4(columns[2]),
            u16::from_str_radix(columns[3], 16).ok(),
            parse_proc_ipv4(columns[7]),
        ) else {
            continue;
        };
        if flags & 0x1 == 0 {
            continue;
        }
        if destination.is_unspecified() && flags & 0x2 != 0 && default_router.is_none() {
            default_router = Some(gateway);
        }
        if flags & 0x2 != 0 || (u32::from(local_ip) & u32::from(mask)) != u32::from(destination) {
            continue;
        }
        if let Some(candidate) = mask_prefix(mask)
            && prefix.is_none_or(|current| candidate > current)
        {
            prefix = Some(candidate);
        }
    }
    (default_router, prefix)
}

fn parse_proc_ipv4(value: &str) -> Option<Ipv4Addr> {
    let value = u32::from_str_radix(value, 16).ok()?;
    Some(Ipv4Addr::from(value.to_le_bytes()))
}

pub(crate) fn parse_proc_neighbors(contents: &str) -> Vec<NeighborEntry> {
    contents
        .lines()
        .skip(1)
        .filter_map(|line| {
            let columns = line.split_whitespace().collect::<Vec<_>>();
            if columns.len() < 4 {
                return None;
            }
            let ip = columns[0].parse().ok()?;
            let raw_flags = columns[2].strip_prefix("0x").unwrap_or(columns[2]);
            let flags = u16::from_str_radix(raw_flags, 16).ok()?;
            if flags == 0 {
                return None;
            }
            Some(NeighborEntry {
                ip,
                mac: columns[3].to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) use {
    mask_prefix as test_mask_prefix, parse_proc_neighbors as test_parse_proc_neighbors,
    parse_proc_routes as test_parse_proc_routes, parse_resolv_conf as test_parse_resolv_conf,
};
