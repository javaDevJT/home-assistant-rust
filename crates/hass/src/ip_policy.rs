//! Shared IP classification matching the pinned Python 3.14.2 standard library.

use std::net::Ipv4Addr;

pub fn ipv4_is_global(address: Ipv4Addr) -> bool {
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
    let in_private = PRIVATE
        .iter()
        .any(|(network, prefix)| in_prefix(address, *network, *prefix));
    !((in_private && !GLOBAL_EXCEPTIONS.contains(&address))
        || in_prefix(address, Ipv4Addr::new(100, 64, 0, 0), 10))
}

fn in_prefix(address: Ipv4Addr, network: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    u32::from(address) & mask == u32::from(network) & mask
}
