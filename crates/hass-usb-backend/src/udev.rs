/// Inputs used to reproduce the serialx `udev_serial_by_id_stem` helper.
#[derive(Clone, Copy, Debug, Default)]
pub struct UdevPortIdentity<'a> {
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub serial_number: Option<&'a str>,
    pub manufacturer: Option<&'a str>,
    pub product: Option<&'a str>,
    pub interface_num: Option<i64>,
    pub port_number: Option<i64>,
}

/// Compute the udev `/dev/serial/by-id/` stem for a USB serial port.
///
/// This preserves serialx 1.15.1's descriptor sanitization, byte limits,
/// interface formatting, and vendor-driver-only `-portN` suffix.
pub fn udev_serial_by_id_stem(port: &UdevPortIdentity<'_>) -> Option<String> {
    let interface_num = port.interface_num?;
    let vendor = match port.manufacturer {
        Some(value) => udev_sysattr(value),
        None => format_hex(port.vid?).into_bytes(),
    };
    let model = match port.product {
        Some(value) => udev_sysattr(value),
        None => format_hex(port.pid?).into_bytes(),
    };

    let mut id_serial = replace_chars(&replace_whitespace(&vendor, 63));
    id_serial.push(b'_');
    id_serial.extend(replace_chars(&replace_whitespace(&model, 63)));

    if let Some(serial_number) = port.serial_number {
        let serial = udev_sysattr(serial_number);
        if serial
            .iter()
            .all(|byte| (0x20..=0x7f).contains(byte) && *byte != b',')
        {
            let serial = replace_chars(&replace_whitespace(&serial, 511));
            if !serial.is_empty() {
                id_serial.push(b'_');
                id_serial.extend(serial);
            }
        }
    }

    id_serial.truncate(255);
    let id_serial = std::str::from_utf8(&id_serial).ok()?;
    let interface_suffix = if interface_num < 0 {
        format!("-{:02x}", interface_num.unsigned_abs())
    } else {
        format!("{interface_num:02x}")
    };
    let mut stem = format!("usb-{id_serial}-if{interface_suffix}");
    if let Some(port_number) = port.port_number {
        stem.push_str(&format!("-port{port_number}"));
    }
    Some(stem)
}

fn format_hex(value: u16) -> String {
    format!("{value:04x}")
}

fn udev_sysattr(value: &str) -> Vec<u8> {
    let mut value = value
        .as_bytes()
        .split(|byte| *byte == 0)
        .next()
        .unwrap_or_default();
    while matches!(value.last(), Some(b'\r' | b'\n')) {
        value = &value[..value.len() - 1];
    }
    value.to_vec()
}

fn replace_whitespace(value: &[u8], size: usize) -> Vec<u8> {
    let value = &value[..value.len().min(size)];
    let start = value
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        .unwrap_or(value.len());
    let value = &value[start..];
    let end = value
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\x0B' | b'\x0C' | b'\r'))
        .map_or(0, |index| index + 1);
    let value = &value[..end];
    let mut result = Vec::with_capacity(value.len());
    let mut in_whitespace = false;
    for byte in value {
        if matches!(byte, b' ' | b'\t' | b'\n' | b'\x0B' | b'\x0C' | b'\r') {
            if !in_whitespace {
                result.push(b'_');
            }
            in_whitespace = true;
        } else {
            result.push(*byte);
            in_whitespace = false;
        }
    }
    result
}

fn replace_chars(value: &[u8]) -> Vec<u8> {
    let mut result = value.to_vec();
    let mut index = 0;
    while index < result.len() {
        if allowed(result[index]) {
            index += 1;
        } else if result[index..].starts_with(b"\\x") {
            index += 2;
        } else if let Some(length) = valid_utf8_character_len(&result, index) {
            index += length;
        } else {
            result[index] = b'_';
            index += 1;
        }
    }
    result
}

fn allowed(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"#+-.:=@_".contains(&byte)
}

fn valid_utf8_character_len(value: &[u8], index: usize) -> Option<usize> {
    let lead = *value.get(index)?;
    let length = if lead & 0xe0 == 0xc0 {
        2
    } else if lead & 0xf0 == 0xe0 {
        3
    } else if lead & 0xf8 == 0xf0 {
        4
    } else {
        return None;
    };
    let decoded = std::str::from_utf8(value.get(index..index + length)?).ok()?;
    let character = decoded.chars().next()?;
    let codepoint = character as u32;
    if (0xfdd0..=0xfdef).contains(&codepoint) || codepoint & 0xfffe == 0xfffe {
        return None;
    }
    Some(length)
}
