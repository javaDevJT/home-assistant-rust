//! Zeroconf's projection of Home Assistant's selected Network adapters.
//!
//! Keep this module as the only bridge from the selected Network snapshot to
//! mDNS interface records. In particular, the daemon receives an explicit
//! interface list and must not enumerate host interfaces a second time.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{Context, Result, bail};
use hass_network::{NetworkAdapter, NetworkManager};
use mdns_sd::{IfAddr, IfOperStatus, Ifv4Addr, Ifv6Addr, NetworkInterface};

const MDNS_TARGET: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);

/// Return the selected Network addresses that the Zeroconf daemon may use.
///
/// This mirrors Home Assistant's `_async_get_zc_args` family selection and
/// filtering rules. Darwin and FreeBSD use a single-stack mode, as in the
/// Python implementation; other platforms use both families when selected
/// IPv6 addresses exist. The capability decision is injectable through
/// [`selected_interfaces_from`] for deterministic tests.
pub fn selected_interfaces(network: &NetworkManager) -> Result<Vec<NetworkInterface>> {
    let adapters = network
        .adapters()
        .context("could not read selected Network adapters for Zeroconf")?;
    let dual_stack_supported = functional_dual_stack_supported();
    let mode = address_family_mode(&adapters, dual_stack_supported);
    let default_route_source =
        if mode == AddressFamilyMode::V4 && only_default_interface_enabled(&adapters) {
            Some(
                network
                    .source_ip(Some(MDNS_TARGET))
                    .context("could not determine the selected Zeroconf default IPv4 address")?,
            )
        } else {
            None
        };

    selected_interfaces_from(&adapters, dual_stack_supported, default_route_source)
}

/// Return all selected Network addresses for Zeroconf/SSDP announcements.
///
/// The selected route source is placed first even when the Network snapshot
/// does not contain it, matching the native announce contract. Existing
/// occurrences retain their relative order after the first matching source is
/// moved to the front.
pub fn announce_addresses(network: &NetworkManager) -> Result<Vec<IpAddr>> {
    let adapters = network
        .adapters()
        .context("could not read selected Network adapters for announcements")?;
    let source = network
        .source_ip(Some(MDNS_TARGET))
        .context("could not determine the selected announcement source IPv4 address")?;
    Ok(announce_addresses_from(&adapters, source))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AddressFamilyMode {
    V4,
    V6,
    Both,
}

fn address_family_mode(
    adapters: &[NetworkAdapter],
    dual_stack_supported: bool,
) -> AddressFamilyMode {
    let has_ipv4 = adapters
        .iter()
        .any(|adapter| adapter.enabled && !adapter.ipv4.is_empty());
    let has_ipv6 = adapters
        .iter()
        .any(|adapter| adapter.enabled && !adapter.ipv6.is_empty());

    if dual_stack_supported && has_ipv6 {
        AddressFamilyMode::Both
    } else if !dual_stack_supported && !has_ipv4 {
        // The source selects V6Only even when the selected snapshot is empty.
        AddressFamilyMode::V6
    } else {
        AddressFamilyMode::V4
    }
}

fn only_default_interface_enabled(adapters: &[NetworkAdapter]) -> bool {
    !adapters
        .iter()
        .any(|adapter| adapter.enabled && !adapter.default)
}

/// Pure projection seam: callers inject the dual-stack capability and the
/// route-selected source so no socket or host-interface enumeration is needed
/// to exercise selection behavior.
fn selected_interfaces_from(
    adapters: &[NetworkAdapter],
    dual_stack_supported: bool,
    default_route_source: Option<Ipv4Addr>,
) -> Result<Vec<NetworkInterface>> {
    let mode = address_family_mode(adapters, dual_stack_supported);
    let default_mode = mode == AddressFamilyMode::V4 && only_default_interface_enabled(adapters);

    if default_mode {
        let source = default_route_source.ok_or_else(|| {
            anyhow::anyhow!("the selected default-only IPv4 interface has no route source")
        })?;
        let candidate = adapters
            .iter()
            .filter(|adapter| adapter.enabled)
            .flat_map(|adapter| {
                adapter
                    .ipv4
                    .iter()
                    .filter(move |record| record.address == source)
                    .map(move |record| (adapter, record))
            })
            .find(|(adapter, _)| adapter.default)
            .or_else(|| {
                adapters
                    .iter()
                    .filter(|adapter| adapter.enabled)
                    .flat_map(|adapter| {
                        adapter
                            .ipv4
                            .iter()
                            .filter(move |record| record.address == source)
                            .map(move |record| (adapter, record))
                    })
                    .next()
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "route-selected Zeroconf IPv4 address {source} is not present on a selected Network adapter"
                )
            })?;

        return Ok(vec![interface_v4(candidate.0, candidate.1)?]);
    }

    let mut interfaces = Vec::new();
    for adapter in adapters.iter().filter(|adapter| adapter.enabled) {
        if matches!(mode, AddressFamilyMode::V4 | AddressFamilyMode::Both) {
            for record in &adapter.ipv4 {
                if record.address.is_loopback() {
                    continue;
                }
                interfaces.push(interface_v4(adapter, record)?);
            }
        }

        if matches!(mode, AddressFamilyMode::V6 | AddressFamilyMode::Both) {
            for record in &adapter.ipv6 {
                if record.address.is_loopback() || python_ipv6_is_global(record.address) {
                    continue;
                }
                interfaces.push(interface_v6(adapter, record)?);
            }
        }
    }

    Ok(interfaces)
}

fn announce_addresses_from(adapters: &[NetworkAdapter], source: Ipv4Addr) -> Vec<IpAddr> {
    let mut addresses = Vec::new();
    for adapter in adapters.iter().filter(|adapter| adapter.enabled) {
        addresses.extend(adapter.ipv4.iter().map(|record| IpAddr::V4(record.address)));
        addresses.extend(adapter.ipv6.iter().map(|record| IpAddr::V6(record.address)));
    }

    let source = IpAddr::V4(source);
    if let Some(index) = addresses.iter().position(|address| *address == source) {
        addresses.remove(index);
    }
    addresses.insert(0, source);
    addresses
}

fn interface_v4(
    adapter: &NetworkAdapter,
    record: &hass_network::Ipv4AddressRecord,
) -> Result<NetworkInterface> {
    let prefixlen = record.network_prefix;
    if prefixlen > 32 {
        bail!(
            "invalid IPv4 prefix length {prefixlen} for {}/{} on adapter {}",
            record.address,
            prefixlen,
            adapter.name
        );
    }
    let mask = if prefixlen == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefixlen))
    };

    Ok(NetworkInterface {
        name: adapter.name.clone(),
        index: adapter.index,
        addr: IfAddr::V4(Ifv4Addr {
            ip: record.address,
            netmask: Ipv4Addr::from(mask),
            broadcast: None,
            prefixlen,
        }),
        oper_status: IfOperStatus::Up,
        is_p2p: false,
        #[cfg(windows)]
        adapter_name: String::new(),
    })
}

fn interface_v6(
    adapter: &NetworkAdapter,
    record: &hass_network::Ipv6AddressRecord,
) -> Result<NetworkInterface> {
    let prefixlen = record.network_prefix;
    if prefixlen > 128 {
        bail!(
            "invalid IPv6 prefix length {prefixlen} for {}/{} on adapter {}",
            record.address,
            prefixlen,
            adapter.name
        );
    }
    let mask = if prefixlen == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefixlen))
    };

    Ok(NetworkInterface {
        name: adapter.name.clone(),
        index: adapter.index,
        addr: IfAddr::V6(Ifv6Addr {
            ip: record.address,
            netmask: Ipv6Addr::from(mask),
            broadcast: None,
            prefixlen,
        }),
        oper_status: IfOperStatus::Up,
        is_p2p: false,
        #[cfg(windows)]
        adapter_name: String::new(),
    })
}

/// CPython's `IPv6Address.is_global` is the inverse of `is_private`, except
/// that IPv4-mapped addresses inherit the underlying IPv4 classification.
/// These match CPython 3.14.2's IANA special-purpose ranges and exceptions.
///
/// This explicit mapping preserves Home Assistant's `.is_global` semantics;
/// it is intentionally not Rust's broader "unicast global" classification.
fn python_ipv6_is_global(address: Ipv6Addr) -> bool {
    if let Some(mapped) = ipv4_mapped(address) {
        return python_ipv4_is_global(mapped);
    }

    const PRIVATE: [(Ipv6Addr, u8); 11] = [
        (Ipv6Addr::LOCALHOST, 128),
        (Ipv6Addr::UNSPECIFIED, 128),
        (Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96),
        (Ipv6Addr::new(0x0064, 0xff9b, 0x0001, 0, 0, 0, 0, 0), 48),
        (Ipv6Addr::new(0x0100, 0, 0, 0, 0, 0, 0, 0), 64),
        (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
        (Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0), 32),
        (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
        (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
        (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
        (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    ];
    const EXCEPTIONS: [(Ipv6Addr, u8); 6] = [
        (Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 1), 128),
        (Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 2), 128),
        (Ipv6Addr::new(0x2001, 3, 0, 0, 0, 0, 0, 0), 32),
        (Ipv6Addr::new(0x2001, 4, 0x0112, 0, 0, 0, 0, 0), 48),
        (Ipv6Addr::new(0x2001, 0x0020, 0, 0, 0, 0, 0, 0), 28),
        (Ipv6Addr::new(0x2001, 0x0030, 0, 0, 0, 0, 0, 0), 28),
    ];

    !PRIVATE
        .iter()
        .any(|(network, prefix)| ipv6_in_prefix(address, *network, *prefix))
        || EXCEPTIONS
            .iter()
            .any(|(network, prefix)| ipv6_in_prefix(address, *network, *prefix))
}

fn ipv4_mapped(address: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = address.segments();
    if segments[..5].iter().all(|segment| *segment == 0) && segments[5] == u16::MAX {
        Some(Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        ))
    } else {
        None
    }
}

fn python_ipv4_is_global(address: Ipv4Addr) -> bool {
    const PRIVATE: [(Ipv4Addr, u8); 14] = [
        (Ipv4Addr::new(0, 0, 0, 0), 8),
        (Ipv4Addr::new(10, 0, 0, 0), 8),
        (Ipv4Addr::new(127, 0, 0, 0), 8),
        (Ipv4Addr::new(169, 254, 0, 0), 16),
        (Ipv4Addr::new(172, 16, 0, 0), 12),
        (Ipv4Addr::new(192, 0, 0, 0), 24),
        (Ipv4Addr::new(192, 0, 0, 170), 31),
        (Ipv4Addr::new(192, 0, 2, 0), 24),
        (Ipv4Addr::new(192, 168, 0, 0), 16),
        (Ipv4Addr::new(198, 18, 0, 0), 15),
        (Ipv4Addr::new(198, 51, 100, 0), 24),
        (Ipv4Addr::new(203, 0, 113, 0), 24),
        (Ipv4Addr::new(240, 0, 0, 0), 4),
        (Ipv4Addr::new(255, 255, 255, 255), 32),
    ];
    const GLOBAL_EXCEPTIONS: [Ipv4Addr; 2] =
        [Ipv4Addr::new(192, 0, 0, 9), Ipv4Addr::new(192, 0, 0, 10)];

    let value = u32::from(address);
    let in_private = PRIVATE.iter().any(|(network, prefix)| {
        let shift = 32 - u32::from(*prefix);
        let mask = if *prefix == 0 { 0 } else { u32::MAX << shift };
        value & mask == u32::from(*network) & mask
    });
    let is_exception = GLOBAL_EXCEPTIONS.contains(&address);
    let shared_address_space = ipv4_in_prefix(address, Ipv4Addr::new(100, 64, 0, 0), 10);
    !((in_private && !is_exception) || shared_address_space)
}

fn ipv4_in_prefix(address: Ipv4Addr, network: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    u32::from(address) & mask == u32::from(network) & mask
}

fn ipv6_in_prefix(address: Ipv6Addr, network: Ipv6Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix))
    };
    u128::from(address) & mask == u128::from(network) & mask
}

fn functional_dual_stack_supported() -> bool {
    !cfg!(any(target_os = "freebsd", target_os = "macos"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_network::{Ipv4AddressRecord, Ipv6AddressRecord};

    fn adapter(
        name: &str,
        index: u32,
        enabled: bool,
        default: bool,
        ipv4: &[(Ipv4Addr, u8)],
        ipv6: &[(Ipv6Addr, u8)],
    ) -> NetworkAdapter {
        NetworkAdapter {
            name: name.to_owned(),
            index: Some(index),
            enabled,
            auto: false,
            default,
            ipv4: ipv4
                .iter()
                .map(|(address, network_prefix)| Ipv4AddressRecord {
                    address: *address,
                    network_prefix: *network_prefix,
                })
                .collect(),
            ipv6: ipv6
                .iter()
                .map(|(address, network_prefix)| Ipv6AddressRecord {
                    address: *address,
                    flowinfo: 0,
                    scope_id: index,
                    network_prefix: *network_prefix,
                })
                .collect(),
        }
    }

    #[test]
    fn default_only_v4_maps_to_the_route_source_on_the_selected_adapter() {
        let source = Ipv4Addr::new(192, 168, 1, 8);
        let adapters = [adapter(
            "en0",
            7,
            true,
            true,
            &[(Ipv4Addr::new(192, 168, 1, 7), 24), (source, 24)],
            &[],
        )];

        let interfaces = selected_interfaces_from(&adapters, false, Some(source)).unwrap();

        assert_eq!(interfaces.len(), 1);
        assert_eq!(interfaces[0].name, "en0");
        assert_eq!(interfaces[0].index, Some(7));
        assert_eq!(interfaces[0].ip(), IpAddr::V4(source));
    }

    #[test]
    fn explicit_dual_stack_filters_loopback_and_python_global_ipv6() {
        let adapters = [adapter(
            "lan0",
            4,
            true,
            false,
            &[
                (Ipv4Addr::new(127, 0, 0, 1), 8),
                (Ipv4Addr::new(192, 168, 1, 4), 24),
            ],
            &[
                (Ipv6Addr::LOCALHOST, 128),
                ("fd12::4".parse().unwrap(), 64),
                ("fe80::4".parse().unwrap(), 64),
                ("2001:db8::4".parse().unwrap(), 64),
                ("2001:4860::4".parse().unwrap(), 64),
            ],
        )];

        let interfaces = selected_interfaces_from(&adapters, true, None).unwrap();

        // Python classifies the documentation prefix as non-global, so the
        // source keeps it alongside ULA and link-local addresses.
        assert_eq!(interfaces.len(), 4);
        assert_eq!(
            interfaces[0].ip(),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 4))
        );
        assert_eq!(interfaces[1].ip(), "fd12::4".parse::<IpAddr>().unwrap());
        assert_eq!(interfaces[2].ip(), "fe80::4".parse::<IpAddr>().unwrap());
        assert_eq!(interfaces[3].ip(), "2001:db8::4".parse::<IpAddr>().unwrap());
        assert!(
            interfaces
                .iter()
                .all(|interface| interface.index == Some(4))
        );
    }

    #[test]
    fn selected_v6_is_used_when_no_ipv4_exists() {
        let address = "fd12::9".parse().unwrap();
        let adapters = [adapter("wlan0", 9, true, true, &[], &[(address, 64)])];

        let interfaces = selected_interfaces_from(&adapters, false, None).unwrap();

        assert_eq!(interfaces.len(), 1);
        assert_eq!(interfaces[0].ip(), IpAddr::V6(address));
    }

    #[test]
    fn empty_single_stack_selection_is_idle_without_a_default_ipv4_route() {
        assert!(
            selected_interfaces_from(&[], false, None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(address_family_mode(&[], false), AddressFamilyMode::V6);
    }

    #[test]
    fn announce_source_is_first_even_when_absent_and_other_addresses_are_retained() {
        let v4 = Ipv4Addr::new(192, 168, 1, 5);
        let v6 = "fd12::5".parse().unwrap();
        let source = Ipv4Addr::new(192, 168, 1, 1);
        let adapters = [
            adapter("lan0", 1, true, true, &[(v4, 24)], &[(v6, 64)]),
            adapter(
                "guest0",
                2,
                false,
                false,
                &[(Ipv4Addr::new(10, 0, 0, 2), 24)],
                &[],
            ),
        ];

        assert_eq!(
            announce_addresses_from(&adapters, source),
            vec![IpAddr::V4(source), IpAddr::V4(v4), IpAddr::V6(v6)]
        );
    }

    #[test]
    fn selected_interface_prefixes_are_validated() {
        let adapters = [adapter(
            "lan0",
            1,
            true,
            false,
            &[(Ipv4Addr::new(192, 168, 1, 5), 33)],
            &[],
        )];

        assert!(selected_interfaces_from(&adapters, false, None).is_err());
    }

    #[test]
    fn python_ipv6_global_mapping_handles_exceptions_and_mapped_addresses() {
        assert!(!python_ipv6_is_global("2001:db8::1".parse().unwrap()));
        assert!(python_ipv6_is_global("2001:1::1".parse().unwrap()));
        assert!(python_ipv6_is_global("2001:4860::1".parse().unwrap()));
        assert!(!python_ipv6_is_global(
            "::ffff:192.168.1.1".parse().unwrap()
        ));
        assert!(python_ipv6_is_global("::ffff:8.8.8.8".parse().unwrap()));
    }
}
