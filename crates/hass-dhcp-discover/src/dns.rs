use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::{PtrResolver, short_hostname};

static NEXT_QUERY_ID: AtomicU16 = AtomicU16::new(1);
const DNS_HEADER_LEN: usize = 12;
const DNS_PTR: u16 = 12;
const DNS_IN: u16 = 1;

/// Pure Rust DNS-over-UDP resolver. Production lookups use port 53 and never
/// request recursion; the private `query_ptr_at` entrypoint lets unit tests
/// exercise the actual codec against an ephemeral loopback server.
#[derive(Clone, Copy, Debug, Default)]
pub struct UdpPtrResolver;

impl PtrResolver for UdpPtrResolver {
    async fn lookup_ptr(
        &self,
        nameserver: IpAddr,
        ip: Ipv4Addr,
        timeout: Duration,
    ) -> Option<String> {
        let server = SocketAddr::new(nameserver, 53);
        Self::query_ptr_at(server, ip, timeout).await
    }
}

impl UdpPtrResolver {
    pub(crate) async fn query_ptr_at(
        server: SocketAddr,
        ip: Ipv4Addr,
        timeout: Duration,
    ) -> Option<String> {
        let query_id = NEXT_QUERY_ID.fetch_add(1, Ordering::Relaxed);
        let query_name = reverse_name(ip);
        let request = build_ptr_query(query_id, &query_name)?;
        let exchange = async {
            let bind_addr = match server {
                SocketAddr::V4(_) => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
                SocketAddr::V6(_) => "[::]:0".parse().ok()?,
            };
            let socket = UdpSocket::bind(bind_addr).await.ok()?;
            socket.connect(server).await.ok()?;
            socket.send(&request).await.ok()?;
            let mut response = vec![0u8; 4096];
            let len = socket.recv(&mut response).await.ok()?;
            response.truncate(len);
            parse_ptr_response(&response, query_id, &query_name)
        };
        tokio::time::timeout(timeout, exchange).await.ok().flatten()
    }
}

fn reverse_name(ip: Ipv4Addr) -> String {
    let octets = ip.octets();
    format!(
        "{}.{}.{}.{}.in-addr.arpa",
        octets[3], octets[2], octets[1], octets[0]
    )
}

fn build_ptr_query(query_id: u16, query_name: &str) -> Option<Vec<u8>> {
    let mut packet = Vec::with_capacity(12 + query_name.len() + 6);
    packet.extend_from_slice(&query_id.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes()); // RD=0 and no other flags.
    packet.extend_from_slice(&1u16.to_be_bytes()); // One question.
    packet.extend_from_slice(&0u16.to_be_bytes()); // Answer count.
    packet.extend_from_slice(&0u16.to_be_bytes()); // Authority count.
    packet.extend_from_slice(&0u16.to_be_bytes()); // Additional count.
    for label in query_name.split('.') {
        if label.is_empty() || label.len() > 63 || !label.is_ascii() {
            return None;
        }
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0);
    packet.extend_from_slice(&DNS_PTR.to_be_bytes());
    packet.extend_from_slice(&DNS_IN.to_be_bytes());
    Some(packet)
}

pub(crate) fn parse_ptr_response(
    packet: &[u8],
    query_id: u16,
    expected_name: &str,
) -> Option<String> {
    if packet.len() < DNS_HEADER_LEN {
        return None;
    }
    let id = read_u16(packet, 0)?;
    let flags = read_u16(packet, 2)?;
    let question_count = read_u16(packet, 4)? as usize;
    let answer_count = read_u16(packet, 6)? as usize;
    let authority_count = read_u16(packet, 8)? as usize;
    let additional_count = read_u16(packet, 10)? as usize;
    if id != query_id
        || flags & 0x8000 == 0 // response bit
        || flags & 0x7800 != 0 // unsupported opcode
        || flags & 0x0200 != 0 // truncated response
        || flags & 0x000f != 0 // non-NOERROR response
        || question_count != 1
    {
        return None;
    }

    let mut cursor = DNS_HEADER_LEN;
    let question_name = read_dns_name(packet, &mut cursor)?;
    let question_type = read_u16(packet, cursor)?;
    let question_class = read_u16(packet, cursor + 2)?;
    cursor = cursor.checked_add(4)?;
    if !question_name.eq_ignore_ascii_case(expected_name)
        || question_type != DNS_PTR
        || question_class != DNS_IN
    {
        return None;
    }

    let mut selected_name = None;
    for section_index in 0..(answer_count + authority_count + additional_count) {
        let owner = read_dns_name(packet, &mut cursor)?;
        let record_type = read_u16(packet, cursor)?;
        let record_class = read_u16(packet, cursor + 2)?;
        let rdlength = read_u16(packet, cursor + 8)? as usize;
        cursor = cursor.checked_add(10)?;
        let data_end = cursor.checked_add(rdlength)?;
        if data_end > packet.len() {
            return None;
        }

        if section_index < answer_count
            && selected_name.is_none()
            && record_type == DNS_PTR
            && record_class == DNS_IN
            && owner.eq_ignore_ascii_case(expected_name)
        {
            let mut data_cursor = cursor;
            let target = read_dns_name(packet, &mut data_cursor)?;
            if data_cursor != data_end {
                return None;
            }
            selected_name = Some(target);
        }
        cursor = data_end;
    }
    if cursor > packet.len() {
        return None;
    }
    selected_name.and_then(|name| short_hostname(&name))
}

fn read_dns_name(packet: &[u8], cursor: &mut usize) -> Option<String> {
    let mut offset = *cursor;
    let mut resume_cursor = None;
    let mut labels = Vec::new();
    let mut visited_pointers = Vec::new();
    let mut wire_len = 1usize; // Include the terminating root label.

    loop {
        let length = *packet.get(offset)?;
        if length & 0xc0 == 0xc0 {
            let second = *packet.get(offset + 1)?;
            let pointer = (((length & 0x3f) as usize) << 8) | second as usize;
            if pointer >= packet.len() || visited_pointers.contains(&pointer) {
                return None;
            }
            visited_pointers.push(pointer);
            resume_cursor.get_or_insert(offset.checked_add(2)?);
            offset = pointer;
            continue;
        }
        if length & 0xc0 != 0 {
            return None;
        }
        offset = offset.checked_add(1)?;
        if length == 0 {
            *cursor = resume_cursor.unwrap_or(offset);
            return Some(labels.join("."));
        }
        let label_len = length as usize;
        if label_len > 63 {
            return None;
        }
        let end = offset.checked_add(label_len)?;
        let label = std::str::from_utf8(packet.get(offset..end)?).ok()?;
        wire_len = wire_len.checked_add(label_len + 1)?;
        if wire_len > 255 {
            return None;
        }
        labels.push(label.to_owned());
        offset = end;
    }
}

fn read_u16(packet: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *packet.get(offset)?,
        *packet.get(offset + 1)?,
    ]))
}

#[cfg(test)]
pub(crate) fn test_build_ptr_query(query_id: u16, name: &str) -> Option<Vec<u8>> {
    build_ptr_query(query_id, name)
}

#[cfg(test)]
mod name_bounds_tests {
    use super::read_dns_name;

    #[test]
    fn total_dns_name_limit_includes_the_terminating_root_label() {
        fn packet(last_label_len: u8) -> Vec<u8> {
            let mut bytes = Vec::new();
            for length in [63, 63, 63, last_label_len] {
                bytes.push(length);
                bytes.extend(std::iter::repeat_n(b'a', usize::from(length)));
            }
            bytes.push(0);
            bytes
        }
        let valid = packet(61);
        assert_eq!(valid.len(), 255);
        assert!(read_dns_name(&valid, &mut 0).is_some());
        let invalid = packet(62);
        assert_eq!(invalid.len(), 256);
        assert!(read_dns_name(&invalid, &mut 0).is_none());
    }
}
