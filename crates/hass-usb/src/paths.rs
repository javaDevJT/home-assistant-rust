use crate::models::{PortDevice, USBDevice, has_unique_serial_number_with};

pub const SERIAL_BY_ID_DIR: &str = "/dev/serial/by-id/";

/// Filesystem path resolution is injected so callers can preserve their
/// platform's `realpath` behavior without making this SDK enumerate devices.
pub trait PathResolver {
    fn realpath(&self, path: &str) -> String;
}

/// Return the udev by-id name behind a by-id path or serial proxy URL.
pub fn serial_path_udev_id(path: &str) -> Option<String> {
    if let Some(id) = path.strip_prefix(SERIAL_BY_ID_DIR) {
        return Some(id.to_owned());
    }

    let query_start = path.find('?')?;
    if path
        .find('#')
        .is_some_and(|fragment| fragment < query_start)
    {
        return None;
    }
    let query = path[query_start + 1..]
        .split('#')
        .next()
        .unwrap_or_default();
    for pair in query.split('&') {
        let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        if form_decode(raw_key) == "port_udev_id" {
            let value = form_decode(raw_value);
            // urllib.parse.parse_qs omits blank values by default.
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Find a unique replacement for a missing configured by-id or proxy path.
/// The source serial-provider policy decides whether a serial number is
/// non-unique; this helper deliberately does not invent that policy.
pub fn find_moved_serial_port(
    device_path: &str,
    scanned_devices: &[USBDevice],
    is_non_unique_serial: impl Fn(Option<&str>) -> bool,
) -> Option<String> {
    let udev_id = serial_path_udev_id(device_path)?;
    if scanned_devices
        .iter()
        .any(|device| device.device == device_path)
    {
        return None;
    }

    let candidates: Vec<_> = scanned_devices
        .iter()
        .filter(|device| {
            has_unique_serial_number_with(&device.as_serial_device(), |serial| {
                is_non_unique_serial(serial)
            }) && serial_path_udev_id(&device.device).as_deref() == Some(udev_id.as_str())
        })
        .map(|device| device.device.clone())
        .collect();
    (candidates.len() == 1).then(|| candidates[0].clone())
}

/// Resolve and find a scanned USB serial device by path. Non-USB serial ports
/// are ignored, and the supplied records are not collapsed by consumer ID.
pub fn usb_device_from_path(
    device_path: &str,
    scanned_ports: &[PortDevice],
    resolver: &impl PathResolver,
) -> Option<USBDevice> {
    let resolved_path = resolver.realpath(device_path);
    scanned_ports.iter().find_map(|port| {
        let PortDevice::Usb(device) = port else {
            return None;
        };
        (resolver.realpath(&device.device) == resolved_path).then(|| device.clone())
    })
}

fn form_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = hex_value(bytes[index + 1]);
                let low = hex_value(bytes[index + 2]);
                if let (Some(high), Some(low)) = (high, low) {
                    decoded.push((high << 4) | low);
                    index += 3;
                } else {
                    decoded.push(b'%');
                    index += 1;
                }
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}
