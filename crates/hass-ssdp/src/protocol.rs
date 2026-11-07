use std::net::SocketAddr;

use chrono::{DateTime, Utc};
use url::{Host, Url};

use crate::Headers;

const SEARCH_REQUEST_LINE: &str = "M-SEARCH * HTTP/1.1";
const NOTIFY_REQUEST_LINE: &str = "NOTIFY * HTTP/1.1";
const SEARCH_RESPONSE_LINE: &str = "HTTP/1.1 200 OK";
const SEARCH_DISCOVER: &str = "\"ssdp:discover\"";

/// The three packet forms accepted by the SSDP datagram decoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageKind {
    /// A request sent by a searching client.
    SearchRequest,
    /// A response to an M-SEARCH request.
    SearchResponse,
    /// A NOTIFY advertisement.
    Notification,
}

/// Decoded SSDP packet plus the address and clock context supplied by the caller.
#[derive(Clone, Debug, PartialEq)]
pub struct SsdpMessage {
    /// Trimmed request/status line.
    pub request_line: String,
    /// Packet form inferred from the request/status line.
    pub kind: MessageKind,
    /// Parsed protocol headers and internal metadata.
    pub headers: Headers,
    /// Local receiving address, when available.
    pub local_addr: Option<SocketAddr>,
    /// Remote sender address.
    pub remote_addr: SocketAddr,
    /// Caller-supplied receive time.
    pub received_at: DateTime<Utc>,
    /// UDN extracted from the `USN` field, retaining its spelling.
    pub udn: Option<String>,
    /// The original `LOCATION` string before IPv6 scope adjustment.
    pub original_location: Option<String>,
    /// `LOCATION` adjusted with the remote IPv6 scope when appropriate.
    pub location: Option<String>,
}

/// Errors encountered while decoding a datagram.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeError {
    /// The datagram does not begin with a recognized SSDP line.
    InvalidPacket,
    /// The request/status line is not valid UTF-8.
    InvalidRequestLine,
    /// A header line is malformed or not valid UTF-8.
    InvalidHeader,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPacket => f.write_str("invalid SSDP packet"),
            Self::InvalidRequestLine => f.write_str("invalid SSDP request/status line"),
            Self::InvalidHeader => f.write_str("invalid SSDP header"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Whether the bytes begin with one of the reference parser's accepted lines.
pub fn is_valid_ssdp_packet(data: &[u8]) -> bool {
    !data.is_empty()
        && data.contains(&b'\n')
        && (data.starts_with(NOTIFY_REQUEST_LINE.as_bytes())
            || data.starts_with(SEARCH_REQUEST_LINE.as_bytes())
            || data.starts_with(SEARCH_RESPONSE_LINE.as_bytes()))
}

/// Extract a UDN from a USN value, preserving its original spelling.
pub fn udn_from_usn(usn: &str) -> Option<String> {
    if usn
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("uuid:"))
    {
        Some(usn.split_once("::").map_or(usn, |(udn, _)| udn).to_owned())
    } else {
        None
    }
}

/// Decode a synthetic or received SSDP datagram using the supplied clock value.
pub fn decode_ssdp_packet(
    data: &[u8],
    local_addr: Option<SocketAddr>,
    remote_addr: SocketAddr,
    received_at: DateTime<Utc>,
) -> Result<SsdpMessage, DecodeError> {
    if !is_valid_ssdp_packet(data) {
        return Err(DecodeError::InvalidPacket);
    }

    let mut normalized = Vec::with_capacity(data.len());
    let mut cursor = 0;
    while cursor < data.len() {
        if data[cursor..].starts_with(b"\r\n") {
            normalized.push(b'\n');
            cursor += 2;
        } else {
            normalized.push(data[cursor]);
            cursor += 1;
        }
    }
    let mut lines = normalized.split(|byte| *byte == b'\n');
    let request_line = std::str::from_utf8(lines.next().unwrap_or_default())
        .map_err(|_| DecodeError::InvalidRequestLine)?
        .trim()
        .to_owned();
    let kind = if request_line.starts_with(NOTIFY_REQUEST_LINE) {
        MessageKind::Notification
    } else if request_line.starts_with(SEARCH_REQUEST_LINE) {
        MessageKind::SearchRequest
    } else {
        MessageKind::SearchResponse
    };

    let mut headers = Headers::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let line = std::str::from_utf8(line).map_err(|_| DecodeError::InvalidHeader)?;
        let (name, value) = line.split_once(':').ok_or(DecodeError::InvalidHeader)?;
        if name.is_empty()
            || name
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Err(DecodeError::InvalidHeader);
        }
        headers.insert(name, value.trim().to_owned());
    }

    let udn = headers.get("usn").and_then(udn_from_usn);
    let original_location = headers
        .get("location")
        .filter(|location| !location.trim().is_empty())
        .map(ToOwned::to_owned);
    let location = original_location
        .as_deref()
        .map(|value| adjusted_location(value, remote_addr));
    let host = remote_host_string(remote_addr);
    headers.metadata_mut().timestamp = Some(received_at);
    headers.metadata_mut().host = Some(host);
    headers.metadata_mut().port = Some(remote_addr.port());
    headers.metadata_mut().remote_addr = Some(remote_addr);
    headers.metadata_mut().local_addr = local_addr;
    headers.metadata_mut().udn.clone_from(&udn);
    headers
        .metadata_mut()
        .location_original
        .clone_from(&original_location);

    Ok(SsdpMessage {
        request_line,
        kind,
        headers,
        local_addr,
        remote_addr,
        received_at,
        udn,
        original_location,
        location,
    })
}

/// Build the exact CRLF-terminated M-SEARCH request used by the reference source.
pub fn build_search_packet(target: SocketAddr, mx: u32, st: &str) -> Vec<u8> {
    format!(
        "{SEARCH_REQUEST_LINE}\r\nHOST:{}\r\nMAN:{SEARCH_DISCOVER}\r\nMX:{mx}\r\nST:{st}\r\n\r\n",
        host_port_string(target)
    )
    .into_bytes()
}

fn host_port_string(address: SocketAddr) -> String {
    match address {
        SocketAddr::V4(v4) => format!("{}:{}", v4.ip(), v4.port()),
        SocketAddr::V6(v6) => {
            let host = remote_host_string(SocketAddr::V6(v6));
            format!("[{host}]:{}", v6.port())
        }
    }
}

fn remote_host_string(address: SocketAddr) -> String {
    match address {
        SocketAddr::V4(v4) => v4.ip().to_string(),
        SocketAddr::V6(v6) if v6.scope_id() != 0 => {
            format!("{}%{}", v6.ip(), v6.scope_id())
        }
        SocketAddr::V6(v6) => v6.ip().to_string(),
    }
}

fn adjusted_location(location: &str, remote_addr: SocketAddr) -> String {
    let SocketAddr::V6(remote) = remote_addr else {
        return location.to_owned();
    };
    if remote.scope_id() == 0 || !remote.ip().is_unicast_link_local() {
        return location.to_owned();
    }

    let Ok(parsed) = Url::parse(location) else {
        return location.to_owned();
    };
    let Some(Host::Ipv6(ip)) = parsed.host() else {
        return location.to_owned();
    };
    if !ip.is_unicast_link_local() {
        return location.to_owned();
    }

    let Some(scheme_end) = location.find("://").map(|index| index + 3) else {
        return location.to_owned();
    };
    let tail = &location[scheme_end..];
    let authority_end = tail.find(['/', '?', '#']).unwrap_or(tail.len());
    let authority = &tail[..authority_end];
    let userinfo_len = authority.rfind('@').map_or(0, |index| index + 1);
    let host_start = scheme_end + userinfo_len;
    let Some(bracket_end_offset) = location[host_start..]
        .find(']')
        .map(|index| index + host_start)
    else {
        return location.to_owned();
    };
    let suffix_start = bracket_end_offset + 1;
    format!(
        "{}[{}%{}]{}",
        &location[..host_start],
        ip,
        remote.scope_id(),
        &location[suffix_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0)
            .single()
            .expect("valid timestamp")
    }

    #[test]
    fn search_packet_matches_reference_bytes() {
        let target: SocketAddr = "239.255.255.250:1900".parse().expect("valid target");
        let packet = build_search_packet(target, 4, "ssdp:all");
        assert_eq!(
            packet,
            b"M-SEARCH * HTTP/1.1\r\nHOST:239.255.255.250:1900\r\nMAN:\"ssdp:discover\"\r\nMX:4\r\nST:ssdp:all\r\n\r\n"
        );
    }

    #[test]
    fn search_packet_formats_scoped_ipv6_host() {
        let target: SocketAddr = "[ff02::c%7]:1900".parse().expect("valid scoped target");
        let packet = String::from_utf8(build_search_packet(target, 2, "upnp:rootdevice"))
            .expect("packet is UTF-8");
        assert!(packet.contains("HOST:[ff02::c%7]:1900\r\n"));
        assert!(packet.ends_with("ST:upnp:rootdevice\r\n\r\n"));
    }

    #[test]
    fn decoder_folds_case_and_preserves_usn_and_location_metadata() {
        let remote: SocketAddr = "192.168.1.8:1900".parse().expect("valid remote");
        let local: SocketAddr = "192.168.1.2:1900".parse().expect("valid local");
        let message = decode_ssdp_packet(
            b"HTTP/1.1 200 OK\r\nUsN: UUID:Device-1::urn:schemas-upnp-org:device:Basic:1\r\nSt:urn:schemas-upnp-org:device:Basic:1\r\nLoCaTiOn:http://192.168.1.8:1400/desc.xml\r\n\r\n",
            Some(local),
            remote,
            at(1_700_000_000),
        )
        .expect("valid packet");

        assert_eq!(message.kind, MessageKind::SearchResponse);
        assert_eq!(message.request_line, "HTTP/1.1 200 OK");
        assert_eq!(
            message.headers.get("ST"),
            Some("urn:schemas-upnp-org:device:Basic:1")
        );
        assert_eq!(message.udn.as_deref(), Some("UUID:Device-1"));
        assert_eq!(
            message.location.as_deref(),
            message.original_location.as_deref()
        );
        assert_eq!(message.remote_addr, remote);
        assert_eq!(message.local_addr, Some(local));
        assert_eq!(
            message.headers.metadata().timestamp,
            Some(at(1_700_000_000))
        );
        assert_eq!(
            message.headers.metadata().location_original.as_deref(),
            Some("http://192.168.1.8:1400/desc.xml")
        );
        assert_eq!(
            message.headers.metadata_json().get("_remote_addr"),
            Some(&serde_json::json!(["192.168.1.8", 1900]))
        );
        let wire_headers = message.headers.to_json();
        assert_eq!(
            wire_headers.get("UsN"),
            Some(&serde_json::json!(
                "UUID:Device-1::urn:schemas-upnp-org:device:Basic:1"
            ))
        );
        assert_eq!(
            wire_headers.get("_udn"),
            Some(&serde_json::json!("UUID:Device-1"))
        );
    }

    #[test]
    fn decoder_adds_remote_scope_to_link_local_location_only() {
        let remote: SocketAddr = "[fe80::2%7]:1900".parse().expect("valid scoped remote");
        let packet = b"NOTIFY * HTTP/1.1\r\nUSN:uuid:Router::upnp:rootdevice\r\nNT:upnp:rootdevice\r\nNTS:ssdp:alive\r\nLOCATION:http://[fe80::1]:80/desc.xml?q=1\r\n\r\n";
        let message =
            decode_ssdp_packet(packet, None, remote, at(1_700_000_000)).expect("valid packet");
        assert_eq!(message.kind, MessageKind::Notification);
        assert_eq!(
            message.original_location.as_deref(),
            Some("http://[fe80::1]:80/desc.xml?q=1")
        );
        assert_eq!(
            message.location.as_deref(),
            Some("http://[fe80::1%7]:80/desc.xml?q=1")
        );
    }

    #[test]
    fn decoder_rejects_unrecognized_or_malformed_datagrams() {
        let remote: SocketAddr = "192.168.1.8:1900".parse().expect("valid remote");
        assert_eq!(
            decode_ssdp_packet(b"GET / HTTP/1.1\r\n\r\n", None, remote, at(1_700_000_000)),
            Err(DecodeError::InvalidPacket)
        );
        assert_eq!(
            decode_ssdp_packet(
                b"HTTP/1.1 200 OK\r\nNo-Colon\r\n\r\n",
                None,
                remote,
                at(1_700_000_000)
            ),
            Err(DecodeError::InvalidHeader)
        );
        assert!(!is_valid_ssdp_packet(b""));
        assert!(!is_valid_ssdp_packet(b"NOTIFY * HTTP/1.1"));
        assert!(is_valid_ssdp_packet(b"NOTIFY * HTTP/1.1\r\n\r\n"));
    }

    #[test]
    fn udn_extraction_is_case_insensitive_and_preserves_spelling() {
        assert_eq!(
            udn_from_usn("uUiD:Mixed::service"),
            Some("uUiD:Mixed".to_owned())
        );
        assert_eq!(
            udn_from_usn("uuid:WholeValue"),
            Some("uuid:WholeValue".to_owned())
        );
        assert_eq!(udn_from_usn("service:without-uuid"), None);
    }
}
