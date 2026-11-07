use std::fmt;
use std::net::Ipv4Addr;

const ETHERNET_HEADER_LEN: usize = 14;
const IPV4_MIN_HEADER_LEN: usize = 20;
const IPV6_HEADER_LEN: usize = 40;
const UDP_HEADER_LEN: usize = 8;
const BOOTP_FIXED_LEN: usize = 236;
const DHCP_COOKIE: [u8; 4] = [99, 130, 83, 99];
const DHCP_OPTION_PAD: u8 = 0;
const DHCP_OPTION_END: u8 = 255;
const DHCP_OPTION_HOSTNAME: u8 = 12;
const DHCP_OPTION_MESSAGE_TYPE: u8 = 53;
const DHCP_OPTION_REQUESTED_ADDRESS: u8 = 50;
const DHCP_REQUEST: u8 = 3;

/// The packet fields reported by the pinned DHCP watcher.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WireObservation {
    pub ip: String,
    pub hostname: String,
    pub mac: String,
}

/// A malformed frame or DHCP option sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeError {
    Truncated(&'static str),
    Malformed(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated(section) => write!(f, "truncated {section}"),
            Self::Malformed(section) => write!(f, "malformed {section}"),
        }
    }
}

impl std::error::Error for DecodeError {}

#[derive(Default)]
struct DhcpOptions {
    message_type: Option<u8>,
    requested_address: Option<Ipv4Addr>,
    hostname: Option<Vec<u8>>,
}

/// Decode one Ethernet frame into the DHCPREQUEST observation used by HA.
///
/// `Ok(None)` means the frame is outside the Scapy UDP 67/68 admission, is not
/// a DHCPREQUEST, or has no address the watcher would report. Malformed
/// packets return `Err` so capture can discard them without terminating the
/// reader.
pub fn decode_ethernet_frame(frame: &[u8]) -> Result<Option<WireObservation>, DecodeError> {
    let ethernet = frame
        .get(..ETHERNET_HEADER_LEN)
        .ok_or(DecodeError::Truncated("Ethernet header"))?;
    let source_mac = &ethernet[6..12];
    let ether_type = u16::from_be_bytes([ethernet[12], ethernet[13]]);

    let (source_ip, udp_packet) = match ether_type {
        0x0800 => {
            let ip_packet = &frame[ETHERNET_HEADER_LEN..];
            let Some((source_ip, udp_packet)) = ipv4_udp_packet(ip_packet)? else {
                return Ok(None);
            };
            (Some(source_ip), udp_packet)
        }
        0x86dd => {
            let ip_packet = &frame[ETHERNET_HEADER_LEN..];
            let Some(udp_packet) = ipv6_udp_packet(ip_packet)? else {
                return Ok(None);
            };
            // The upstream handler falls back only to Scapy's IPv4 `IP`
            // layer. An IPv6 source address must not become the observation.
            (None, udp_packet)
        }
        // This decoder accepts Ethernet frames with IP payloads. The upstream
        // handler can see DHCP on other capture link types, but those frames
        // need a separate link-layer decoder before UDP/BOOTP can be parsed.
        _ => return Ok(None),
    };

    let Some(udp_payload) = udp_payload(udp_packet)? else {
        return Ok(None);
    };
    let dhcp_options_start = BOOTP_FIXED_LEN + DHCP_COOKIE.len();
    if udp_payload.len() < dhcp_options_start {
        return Err(DecodeError::Truncated("BOOTP/DHCP header"));
    }
    if udp_payload[BOOTP_FIXED_LEN..dhcp_options_start] != DHCP_COOKIE {
        return Ok(None);
    }

    let options = parse_options(&udp_payload[dhcp_options_start..])?;
    if options.message_type != Some(DHCP_REQUEST) {
        return Ok(None);
    }

    let Some(ip) = options.requested_address.or(source_ip) else {
        return Ok(None);
    };
    let hostname = options
        .hostname
        .as_deref()
        .filter(|bytes| !bytes.is_empty())
        .map(decode_hostname)
        .unwrap_or_default();

    Ok(Some(WireObservation {
        ip: ip.to_string(),
        hostname,
        mac: format_mac(source_mac),
    }))
}

fn ipv4_udp_packet(packet: &[u8]) -> Result<Option<(Ipv4Addr, &[u8])>, DecodeError> {
    if packet.len() < IPV4_MIN_HEADER_LEN {
        return Err(DecodeError::Truncated("IPv4 header"));
    }
    if packet[0] >> 4 != 4 {
        return Err(DecodeError::Malformed("IPv4 version"));
    }
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    if header_len < IPV4_MIN_HEADER_LEN {
        return Err(DecodeError::Malformed("IPv4 header length"));
    }
    if packet.len() < header_len {
        return Err(DecodeError::Truncated("IPv4 options"));
    }

    let total_len = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if total_len < header_len {
        return Err(DecodeError::Malformed("IPv4 total length"));
    }
    if packet.len() < total_len {
        return Err(DecodeError::Truncated("IPv4 payload"));
    }
    if packet[9] != 17 {
        return Ok(None);
    }

    // Scapy does not reassemble fragments in this listener. A fragmented
    // datagram cannot be relied on to contain the complete BOOTP fields.
    let fragment = u16::from_be_bytes([packet[6], packet[7]]);
    if fragment & 0x3fff != 0 {
        return Ok(None);
    }

    let source_ip = Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]);
    Ok(Some((source_ip, &packet[header_len..total_len])))
}

fn ipv6_udp_packet(packet: &[u8]) -> Result<Option<&[u8]>, DecodeError> {
    if packet.len() < IPV6_HEADER_LEN {
        return Err(DecodeError::Truncated("IPv6 header"));
    }
    if packet[0] >> 4 != 6 {
        return Err(DecodeError::Malformed("IPv6 version"));
    }

    let payload_len = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
    let packet_len = IPV6_HEADER_LEN + payload_len;
    if packet.len() < packet_len {
        return Err(DecodeError::Truncated("IPv6 payload"));
    }

    let payload = &packet[IPV6_HEADER_LEN..packet_len];
    let mut next_header = packet[6];
    let mut offset = 0;
    loop {
        match next_header {
            17 => return Ok(Some(&payload[offset..])),
            // Hop-by-Hop, Routing, Destination Options, Mobility, HIP, and
            // Shim6 headers encode their size in 8-octet units.
            0 | 43 | 60 | 135 | 139 | 140 => {
                let (following, length) = ipv6_length_prefixed_extension(payload, offset, 1, 8)?;
                next_header = following;
                offset += length;
            }
            // Authentication Header uses 32-bit words, with two fixed words.
            51 => {
                let (following, length) = ipv6_length_prefixed_extension(payload, offset, 2, 4)?;
                next_header = following;
                offset += length;
            }
            44 => {
                let end = offset + 8;
                let fragment = payload
                    .get(offset..end)
                    .ok_or(DecodeError::Truncated("IPv6 fragment header"))?;
                let offset_flags = u16::from_be_bytes([fragment[2], fragment[3]]);
                // No fragment reassembly is performed. Ignore both later
                // fragments and first fragments marked as having more data.
                if offset_flags & 0xfff9 != 0 {
                    return Ok(None);
                }
                next_header = fragment[0];
                offset = end;
            }
            // ESP and No Next Header do not expose a UDP header here.
            50 | 59 => return Ok(None),
            _ => return Ok(None),
        }
    }
}

fn ipv6_length_prefixed_extension(
    payload: &[u8],
    offset: usize,
    length_add: usize,
    length_multiplier: usize,
) -> Result<(u8, usize), DecodeError> {
    let prefix_end = offset + 2;
    let prefix = payload
        .get(offset..prefix_end)
        .ok_or(DecodeError::Truncated("IPv6 extension header"))?;
    let length = (usize::from(prefix[1]) + length_add) * length_multiplier;
    let end = offset + length;
    let extension = payload
        .get(offset..end)
        .ok_or(DecodeError::Truncated("IPv6 extension header"))?;
    Ok((extension[0], length))
}

fn udp_payload(packet: &[u8]) -> Result<Option<&[u8]>, DecodeError> {
    if packet.len() < UDP_HEADER_LEN {
        return Err(DecodeError::Truncated("UDP header"));
    }
    let source_port = u16::from_be_bytes([packet[0], packet[1]]);
    let destination_port = u16::from_be_bytes([packet[2], packet[3]]);
    if !is_dhcp_capture_port(source_port) && !is_dhcp_capture_port(destination_port) {
        return Ok(None);
    }

    let length = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
    if length < UDP_HEADER_LEN {
        return Err(DecodeError::Malformed("UDP length"));
    }
    if length > packet.len() {
        return Err(DecodeError::Truncated("UDP payload"));
    }
    Ok(Some(&packet[UDP_HEADER_LEN..length]))
}

fn is_dhcp_capture_port(port: u16) -> bool {
    matches!(port, 67 | 68)
}

fn parse_options(bytes: &[u8]) -> Result<DhcpOptions, DecodeError> {
    let mut parsed = DhcpOptions::default();
    let mut offset = 0;

    while offset < bytes.len() {
        let code = bytes[offset];
        offset += 1;
        match code {
            DHCP_OPTION_PAD => continue,
            DHCP_OPTION_END => break,
            _ => {}
        }

        let length = *bytes
            .get(offset)
            .ok_or(DecodeError::Truncated("DHCP option length"))?;
        offset += 1;
        let end = offset
            .checked_add(usize::from(length))
            .ok_or(DecodeError::Malformed("DHCP option length"))?;
        let value = bytes
            .get(offset..end)
            .ok_or(DecodeError::Truncated("DHCP option value"))?;
        offset = end;

        match code {
            DHCP_OPTION_MESSAGE_TYPE => {
                if value.len() != 1 {
                    return Err(DecodeError::Malformed("DHCP message-type option"));
                }
                parsed.message_type = Some(value[0]);
            }
            DHCP_OPTION_REQUESTED_ADDRESS => {
                let address: [u8; 4] = value
                    .try_into()
                    .map_err(|_| DecodeError::Malformed("DHCP requested-address option"))?;
                parsed.requested_address = Some(Ipv4Addr::from(address));
            }
            DHCP_OPTION_HOSTNAME => parsed.hostname = Some(value.to_vec()),
            _ => {}
        }
    }

    Ok(parsed)
}

fn decode_hostname(bytes: &[u8]) -> String {
    let Ok(hostname) = std::str::from_utf8(bytes) else {
        return String::from_utf8_lossy(bytes).into_owned();
    };

    // Python's bytes.decode("idna") accepts ASCII labels and decodes punycode
    // labels. Non-ASCII UTF-8 input raises UnicodeError and reaches the pinned
    // UTF-8 replacement fallback, so do not normalize it through UTS #46.
    if !bytes.is_ascii() {
        return hostname.to_owned();
    }

    let decoded = hostname
        .split('.')
        .map(|label| {
            if label
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("xn--"))
            {
                let unicode = idna::punycode::decode_to_string(&label[4..])?;
                let prepared = idna2003_nameprep(&unicode)?;
                let round_trip = idna2003_to_ascii(&prepared)?;
                if !round_trip.eq_ignore_ascii_case(label) {
                    return None;
                }
                Some(unicode)
            } else {
                Some(label.to_owned())
            }
        })
        .collect::<Option<Vec<_>>>();

    // The pinned handler falls back to UTF-8 with replacement if any label
    // raises an IDNA UnicodeError. Since this input is ASCII, that fallback is
    // the original string.
    decoded
        .map(|labels| labels.join("."))
        .unwrap_or_else(|| hostname.to_owned())
}

fn idna2003_nameprep(label: &str) -> Option<String> {
    let unassigned: Vec<char> = label
        .chars()
        .filter(|&character| stringprep::tables::unassigned_code_point(character))
        .collect();
    if unassigned.is_empty() {
        return stringprep::nameprep(label)
            .ok()
            .map(|prepared| prepared.into_owned());
    }

    // CPython's IDNA codec runs RFC 3491 Nameprep with AllowUnassigned=true.
    // stringprep::nameprep enforces RFC 3454's unassigned check, so temporarily
    // replace such code points with an unused ASCII control (left untouched by
    // this profile), then restore them after all mapping, normalization, and
    // bidi checks have run. Python's Unicode 3.2 tables likewise treat these
    // code points as inert during Nameprep.
    for value in 1..=0x1f {
        let marker = char::from_u32(value)?;
        if label.contains(marker) {
            continue;
        }

        let mut nameprep_input = String::with_capacity(label.len());
        for character in label.chars() {
            if stringprep::tables::unassigned_code_point(character) {
                nameprep_input.push(marker);
            } else {
                nameprep_input.push(character);
            }
        }

        let prepared = stringprep::nameprep(&nameprep_input).ok()?.into_owned();
        if prepared
            .chars()
            .filter(|&character| character == marker)
            .count()
            != unassigned.len()
        {
            continue;
        }

        let mut unassigned = unassigned.iter().copied();
        let mut restored = String::with_capacity(prepared.len());
        for character in prepared.chars() {
            if character == marker {
                restored.push(unassigned.next()?);
            } else {
                restored.push(character);
            }
        }
        return unassigned.next().is_none().then_some(restored);
    }

    None
}

fn idna2003_to_ascii(label: &str) -> Option<String> {
    if label.is_ascii() {
        return (1..64).contains(&label.len()).then(|| label.to_owned());
    }
    if label.to_ascii_lowercase().starts_with("xn--") {
        return None;
    }

    let ascii = format!("xn--{}", idna::punycode::encode_str(label)?);
    (ascii.len() < 64).then_some(ascii)
}

fn format_mac(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|octet| format!("{octet:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub fn request_frame() -> Vec<u8> {
        let options = [DHCP_OPTION_MESSAGE_TYPE, 1, DHCP_REQUEST, DHCP_OPTION_END];
        let mut bootp = vec![0; BOOTP_FIXED_LEN + DHCP_COOKIE.len()];
        bootp[BOOTP_FIXED_LEN..BOOTP_FIXED_LEN + 4].copy_from_slice(&DHCP_COOKIE);
        bootp.extend_from_slice(&options);

        let udp_len = UDP_HEADER_LEN + bootp.len();
        let mut udp = Vec::with_capacity(udp_len);
        udp.extend_from_slice(&68_u16.to_be_bytes());
        udp.extend_from_slice(&67_u16.to_be_bytes());
        udp.extend_from_slice(&(udp_len as u16).to_be_bytes());
        udp.extend_from_slice(&[0, 0]);
        udp.extend_from_slice(&bootp);

        let ip_len = IPV4_MIN_HEADER_LEN + udp.len();
        let mut ip = vec![0; IPV4_MIN_HEADER_LEN];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&(ip_len as u16).to_be_bytes());
        ip[8] = 64;
        ip[9] = 17;
        ip[12..16].copy_from_slice(&[192, 0, 2, 19]);
        ip[16..20].copy_from_slice(&[255; 4]);
        ip.extend_from_slice(&udp);

        let mut ethernet = vec![0; ETHERNET_HEADER_LEN];
        ethernet[6..12].copy_from_slice(&[0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee]);
        ethernet[12..14].copy_from_slice(&0x0800_u16.to_be_bytes());
        ethernet.extend_from_slice(&ip);
        ethernet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE_IP: [u8; 4] = [192, 0, 2, 19];
    const SOURCE_MAC: [u8; 6] = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];

    fn add_option(options: &mut Vec<u8>, code: u8, value: &[u8]) {
        options.push(code);
        options.push(value.len() as u8);
        options.extend_from_slice(value);
    }

    fn request_frame(options: &[u8], source_port: u16, destination_port: u16) -> Vec<u8> {
        let mut bootp = vec![0; BOOTP_FIXED_LEN + DHCP_COOKIE.len()];
        // Deliberately contrast BOOTP addresses and chaddr with the selected
        // packet fields. The source reads neither BOOTP address nor chaddr.
        bootp[12..16].copy_from_slice(&[203, 0, 113, 31]);
        bootp[16..20].copy_from_slice(&[203, 0, 113, 32]);
        bootp[28..34].copy_from_slice(&[0x10, 0x20, 0x30, 0x40, 0x50, 0x60]);
        bootp[BOOTP_FIXED_LEN..BOOTP_FIXED_LEN + 4].copy_from_slice(&DHCP_COOKIE);
        bootp.extend_from_slice(options);

        let udp_len = UDP_HEADER_LEN + bootp.len();
        let mut udp = Vec::with_capacity(udp_len);
        udp.extend_from_slice(&source_port.to_be_bytes());
        udp.extend_from_slice(&destination_port.to_be_bytes());
        udp.extend_from_slice(&(udp_len as u16).to_be_bytes());
        udp.extend_from_slice(&[0, 0]);
        udp.extend_from_slice(&bootp);

        let ip_len = IPV4_MIN_HEADER_LEN + udp.len();
        let mut ip = vec![0; IPV4_MIN_HEADER_LEN];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&(ip_len as u16).to_be_bytes());
        ip[6..8].copy_from_slice(&0x4000_u16.to_be_bytes());
        ip[8] = 64;
        ip[9] = 17;
        ip[12..16].copy_from_slice(&SOURCE_IP);
        ip[16..20].copy_from_slice(&[255, 255, 255, 255]);
        ip.extend_from_slice(&udp);

        let mut ethernet = vec![0; ETHERNET_HEADER_LEN];
        ethernet[0..6].copy_from_slice(&[0xff; 6]);
        ethernet[6..12].copy_from_slice(&SOURCE_MAC);
        ethernet[12..14].copy_from_slice(&0x0800_u16.to_be_bytes());
        ethernet.extend_from_slice(&ip);
        ethernet
    }

    fn ipv6_request_frame(options: &[u8], source_port: u16, destination_port: u16) -> Vec<u8> {
        let ipv4_frame = request_frame(options, source_port, destination_port);
        let udp_start = ETHERNET_HEADER_LEN + IPV4_MIN_HEADER_LEN;
        let udp = &ipv4_frame[udp_start..];

        let mut ipv6 = vec![0; IPV6_HEADER_LEN];
        ipv6[0] = 0x60;
        ipv6[4..6].copy_from_slice(&(udp.len() as u16).to_be_bytes());
        ipv6[6] = 17;
        ipv6[7] = 64;
        ipv6[8] = 0xfe;
        ipv6[9] = 0x80;
        ipv6[23] = 0x13;
        ipv6[24] = 0xff;
        ipv6[25] = 0x02;
        ipv6[39] = 1;
        ipv6.extend_from_slice(udp);

        let mut ethernet = vec![0; ETHERNET_HEADER_LEN];
        ethernet[0..6].copy_from_slice(&[0xff; 6]);
        ethernet[6..12].copy_from_slice(&SOURCE_MAC);
        ethernet[12..14].copy_from_slice(&0x86dd_u16.to_be_bytes());
        ethernet.extend_from_slice(&ipv6);
        ethernet
    }

    fn add_ipv6_hop_by_hop_header(frame: &mut Vec<u8>) {
        let udp_start = ETHERNET_HEADER_LEN + IPV6_HEADER_LEN;
        let udp = frame.split_off(udp_start);
        let mut extension = vec![17, 0, 0, 0, 0, 0, 0, 0];
        extension.extend_from_slice(&udp);
        let payload_len = extension.len() as u16;
        frame[ETHERNET_HEADER_LEN + 4..ETHERNET_HEADER_LEN + 6]
            .copy_from_slice(&payload_len.to_be_bytes());
        frame[ETHERNET_HEADER_LEN + 6] = 0;
        frame.extend_from_slice(&extension);
    }

    fn request_options() -> Vec<u8> {
        let mut options = Vec::new();
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST]);
        options.push(DHCP_OPTION_END);
        options
    }

    #[test]
    fn requested_address_hostname_and_ethernet_source_are_reported() {
        let mut options = Vec::new();
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST]);
        add_option(&mut options, DHCP_OPTION_REQUESTED_ADDRESS, &[10, 1, 2, 3]);
        add_option(&mut options, DHCP_OPTION_HOSTNAME, b"xn--bcher-kva");
        options.push(DHCP_OPTION_END);

        let observation = decode_ethernet_frame(&request_frame(&options, 68, 67))
            .unwrap()
            .unwrap();
        assert_eq!(observation.ip, "10.1.2.3");
        assert_eq!(observation.hostname, "bücher");
        assert_eq!(observation.mac, "02:aa:bb:cc:dd:ee");
    }

    #[test]
    fn idna2003_round_trip_falls_back_for_sharp_s_punycode() {
        for hostname in [b"xn--fa-hia".as_slice(), b"xn--strae-oqa"] {
            let mut options = Vec::new();
            add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST]);
            add_option(&mut options, DHCP_OPTION_HOSTNAME, hostname);
            options.push(DHCP_OPTION_END);

            let observation = decode_ethernet_frame(&request_frame(&options, 68, 67))
                .unwrap()
                .unwrap();
            assert_eq!(observation.hostname, std::str::from_utf8(hostname).unwrap());
        }
    }

    #[test]
    fn renewal_uses_ipv4_source_and_missing_hostname_is_empty() {
        let observation = decode_ethernet_frame(&request_frame(&request_options(), 67, 68))
            .unwrap()
            .unwrap();
        assert_eq!(observation.ip, "192.0.2.19");
        assert_eq!(observation.hostname, "");
    }

    #[test]
    fn ipv6_dhcp_request_requires_requested_address() {
        let mut options = Vec::new();
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST]);
        add_option(&mut options, DHCP_OPTION_REQUESTED_ADDRESS, &[10, 1, 2, 3]);
        options.push(DHCP_OPTION_END);

        let observation = decode_ethernet_frame(&ipv6_request_frame(&options, 68, 67))
            .unwrap()
            .unwrap();
        assert_eq!(observation.ip, "10.1.2.3");
        assert_eq!(observation.mac, "02:aa:bb:cc:dd:ee");

        let mut extension_frame = ipv6_request_frame(&options, 68, 67);
        add_ipv6_hop_by_hop_header(&mut extension_frame);
        assert!(decode_ethernet_frame(&extension_frame).unwrap().is_some());

        assert_eq!(
            decode_ethernet_frame(&ipv6_request_frame(&request_options(), 67, 68)).unwrap(),
            None
        );
    }

    #[test]
    fn ipv6_extension_and_udp_lengths_are_bounds_checked() {
        let mut options = Vec::new();
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST]);
        add_option(&mut options, DHCP_OPTION_REQUESTED_ADDRESS, &[10, 1, 2, 3]);
        options.push(DHCP_OPTION_END);

        let mut malformed_extension = ipv6_request_frame(&options, 68, 67);
        add_ipv6_hop_by_hop_header(&mut malformed_extension);
        malformed_extension[ETHERNET_HEADER_LEN + IPV6_HEADER_LEN + 1] = u8::MAX;
        assert!(matches!(
            decode_ethernet_frame(&malformed_extension),
            Err(DecodeError::Truncated("IPv6 extension header"))
        ));

        let mut malformed_udp = ipv6_request_frame(&options, 68, 67);
        let udp_start = ETHERNET_HEADER_LEN + IPV6_HEADER_LEN;
        malformed_udp[udp_start + 4..udp_start + 6].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(matches!(
            decode_ethernet_frame(&malformed_udp),
            Err(DecodeError::Truncated("UDP payload"))
        ));
    }

    #[test]
    fn duplicate_options_use_last_value_and_end_stops_parsing() {
        let mut options = Vec::new();
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[1]);
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST]);
        add_option(&mut options, DHCP_OPTION_REQUESTED_ADDRESS, &[10, 0, 0, 1]);
        add_option(&mut options, DHCP_OPTION_REQUESTED_ADDRESS, &[10, 0, 0, 2]);
        add_option(&mut options, DHCP_OPTION_HOSTNAME, b"first");
        add_option(&mut options, DHCP_OPTION_HOSTNAME, b"last");
        options.push(DHCP_OPTION_END);
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[1]);

        let observation = decode_ethernet_frame(&request_frame(&options, 68, 67))
            .unwrap()
            .unwrap();
        assert_eq!(observation.ip, "10.0.0.2");
        assert_eq!(observation.hostname, "last");
    }

    #[test]
    fn malformed_idna_falls_back_to_utf8_and_invalid_utf8_uses_replacement() {
        let mut options = Vec::new();
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST]);
        add_option(&mut options, DHCP_OPTION_HOSTNAME, b"bad--label");
        options.push(DHCP_OPTION_END);
        let observation = decode_ethernet_frame(&request_frame(&options, 68, 67))
            .unwrap()
            .unwrap();
        assert_eq!(observation.hostname, "bad--label");

        let mut options = Vec::new();
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST]);
        add_option(&mut options, DHCP_OPTION_HOSTNAME, b"xn--bad-");
        options.push(DHCP_OPTION_END);
        let observation = decode_ethernet_frame(&request_frame(&options, 68, 67))
            .unwrap()
            .unwrap();
        assert_eq!(observation.hostname, "xn--bad-");

        let mut options = Vec::new();
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST]);
        add_option(&mut options, DHCP_OPTION_HOSTNAME, b"ABC.LOCAL");
        options.push(DHCP_OPTION_END);
        let observation = decode_ethernet_frame(&request_frame(&options, 68, 67))
            .unwrap()
            .unwrap();
        assert_eq!(observation.hostname, "ABC.LOCAL");

        let mut options = Vec::new();
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST]);
        add_option(&mut options, DHCP_OPTION_HOSTNAME, "Étage".as_bytes());
        options.push(DHCP_OPTION_END);
        let observation = decode_ethernet_frame(&request_frame(&options, 68, 67))
            .unwrap()
            .unwrap();
        assert_eq!(observation.hostname, "Étage");

        let mut options = Vec::new();
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST]);
        add_option(&mut options, DHCP_OPTION_HOSTNAME, b"xn--BCHER-kva");
        options.push(DHCP_OPTION_END);
        let observation = decode_ethernet_frame(&request_frame(&options, 68, 67))
            .unwrap()
            .unwrap();
        assert_eq!(observation.hostname, "BüCHER");

        let mut options = Vec::new();
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST]);
        add_option(
            &mut options,
            DHCP_OPTION_HOSTNAME,
            &[0xff, b'n', b'o', b'd', b'e'],
        );
        options.push(DHCP_OPTION_END);
        let observation = decode_ethernet_frame(&request_frame(&options, 68, 67))
            .unwrap()
            .unwrap();
        assert_eq!(observation.hostname, "�node");
    }

    #[test]
    fn nonrequests_wrong_ports_and_non_ipv4_frames_are_ignored() {
        let mut discover = Vec::new();
        add_option(&mut discover, DHCP_OPTION_MESSAGE_TYPE, &[1]);
        discover.push(DHCP_OPTION_END);
        assert_eq!(
            decode_ethernet_frame(&request_frame(&discover, 68, 67)).unwrap(),
            None
        );
        assert_eq!(
            decode_ethernet_frame(&request_frame(&request_options(), 66, 69)).unwrap(),
            None
        );

        let mut arp = request_frame(&request_options(), 68, 67);
        arp[12..14].copy_from_slice(&0x0806_u16.to_be_bytes());
        assert_eq!(decode_ethernet_frame(&arp).unwrap(), None);
    }

    #[test]
    fn both_udp_port_directions_are_admitted() {
        let options = request_options();
        assert!(
            decode_ethernet_frame(&request_frame(&options, 67, 50000))
                .unwrap()
                .is_some()
        );
        assert!(
            decode_ethernet_frame(&request_frame(&options, 50000, 68))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn corrupt_option_lengths_and_truncated_headers_are_errors() {
        let mut options = vec![DHCP_OPTION_MESSAGE_TYPE, 1, DHCP_REQUEST];
        options.extend_from_slice(&[DHCP_OPTION_HOSTNAME, 4, b'a', b'b']);
        assert!(matches!(
            decode_ethernet_frame(&request_frame(&options, 68, 67)),
            Err(DecodeError::Truncated("DHCP option value"))
        ));

        assert!(matches!(
            decode_ethernet_frame(&[0; 8]),
            Err(DecodeError::Truncated("Ethernet header"))
        ));
        assert!(matches!(
            decode_ethernet_frame(&request_frame(&request_options(), 68, 67)[..44]),
            Err(DecodeError::Truncated("IPv4 payload"))
        ));
    }

    #[test]
    fn malformed_fixed_options_are_errors() {
        let mut options = Vec::new();
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST, 1]);
        options.push(DHCP_OPTION_END);
        assert!(matches!(
            decode_ethernet_frame(&request_frame(&options, 68, 67)),
            Err(DecodeError::Malformed("DHCP message-type option"))
        ));

        let mut options = Vec::new();
        add_option(&mut options, DHCP_OPTION_MESSAGE_TYPE, &[DHCP_REQUEST]);
        add_option(&mut options, DHCP_OPTION_REQUESTED_ADDRESS, &[10, 0, 0]);
        options.push(DHCP_OPTION_END);
        assert!(matches!(
            decode_ethernet_frame(&request_frame(&options, 68, 67)),
            Err(DecodeError::Malformed("DHCP requested-address option"))
        ));
    }
}
