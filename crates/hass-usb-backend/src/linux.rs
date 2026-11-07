use std::{
    fs,
    path::{Path, PathBuf},
};

use hass_usb::SerialPortInfo;

use crate::{UsbBackendError, source_description};

const PORT_UNKNOWN: i64 = 0;

pub(super) fn enumerate_linux(
    sys_root: &Path,
    dev_root: &Path,
) -> Result<Vec<SerialPortInfo>, UsbBackendError> {
    let mut by_id_symlinks = std::collections::HashMap::<PathBuf, PathBuf>::new();
    let by_id_path = dev_root.join("serial/by-id");

    // serialx tolerates an absent by-id directory and lets the last alias for
    // a resolved device win, matching normal Path.iterdir ordering semantics.
    for symlink in iterdir_safe(&by_id_path) {
        let resolved = crate::resolve_serial_path(&symlink);
        by_id_symlinks.insert(resolved, symlink);
    }

    let mut results = Vec::new();
    for tty_path in iterdir_safe(&sys_root.join("class/tty")) {
        let Some(tty_name) = tty_path.file_name() else {
            continue;
        };
        if !tty_name.to_string_lossy().starts_with("tty") {
            continue;
        }

        let tty_device = tty_path.join("device");
        if !tty_device.join("driver").exists() {
            continue;
        }

        let device = dev_root.join(tty_name);
        let Ok(resolved) = std::fs::canonicalize(&tty_device) else {
            continue;
        };
        let Ok(subsystem_path) = std::fs::canonicalize(resolved.join("subsystem")) else {
            continue;
        };
        let Some(subsystem) = subsystem_path.file_name().and_then(|v| v.to_str()) else {
            continue;
        };
        let unique_device = by_id_symlinks.get(&device).unwrap_or(&device);

        let info = match subsystem {
            "usb-serial" => {
                let usb_interface = resolved.parent().unwrap_or(&resolved);
                let usb_device = usb_interface.parent().unwrap_or(usb_interface);
                let (
                    vid,
                    pid,
                    bcd_device,
                    interface_num,
                    product,
                    serial,
                    manufacturer,
                    interface_description,
                ) = match read_usb_fields(usb_device, usb_interface, &resolved, true) {
                    Ok(fields) => fields,
                    Err(UsbBackendError::Io { .. }) => continue,
                    Err(error) => return Err(error),
                };
                make_record(
                    unique_device,
                    &device,
                    Some(vid),
                    Some(pid),
                    Some(bcd_device),
                    Some(interface_num),
                    serial,
                    manufacturer,
                    product,
                    interface_description,
                )
            }
            "usb" => {
                // CDC ACM exposes the USB interface itself as `device`.
                let usb_interface = resolved.as_path();
                let usb_device = usb_interface.parent().unwrap_or(usb_interface);
                let (
                    vid,
                    pid,
                    bcd_device,
                    interface_num,
                    product,
                    serial,
                    manufacturer,
                    interface_description,
                ) = match read_usb_fields(usb_device, usb_interface, &resolved, false) {
                    Ok(fields) => fields,
                    Err(UsbBackendError::Io { .. }) => continue,
                    Err(error) => return Err(error),
                };
                make_record(
                    unique_device,
                    &device,
                    Some(vid),
                    Some(pid),
                    Some(bcd_device),
                    Some(interface_num),
                    serial,
                    manufacturer,
                    product,
                    interface_description,
                )
            }
            "serial-base" | "platform" | "pnp" | "amba" => {
                let type_path = tty_path.join("type");
                let Ok(port_type) = fs::read_to_string(&type_path) else {
                    continue;
                };
                let Ok(port_type) = port_type.trim().parse::<i64>() else {
                    return Err(UsbBackendError::InvalidSysfsValue {
                        path: type_path.display().to_string(),
                        value: port_type,
                    });
                };
                if port_type == PORT_UNKNOWN {
                    continue;
                }
                make_record(
                    unique_device,
                    &device,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
            }
            _ => continue,
        };
        results.push(info);
    }
    Ok(results)
}

type UsbFields = (
    u16,
    u16,
    u16,
    u16,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn read_usb_fields(
    usb_device: &Path,
    usb_interface: &Path,
    resolved: &Path,
    usb_serial_driver: bool,
) -> Result<UsbFields, UsbBackendError> {
    let vid = read_hex_u16(usb_device, "idVendor")?;
    let pid = read_hex_u16(usb_device, "idProduct")?;
    let bcd_device = read_hex_u16(usb_device, "bcdDevice")?;
    let interface_num = read_hex_u16(usb_interface, "bInterfaceNumber")?;
    if usb_serial_driver {
        let _port_number = read_decimal_i64(resolved, "port_number")?;
    }
    Ok((
        vid,
        pid,
        bcd_device,
        interface_num,
        read_optional_sysfs(&usb_device.join("product"))?,
        read_optional_sysfs(&usb_device.join("serial"))?,
        read_optional_sysfs(&usb_device.join("manufacturer"))?,
        read_optional_sysfs(&usb_interface.join("interface"))?,
    ))
}

fn read_hex_u16(base: &Path, name: &str) -> Result<u16, UsbBackendError> {
    let path = base.join(name);
    let value = read_required(&path)?;
    let value = value.trim().strip_prefix("0x").unwrap_or(value.trim());
    let parsed =
        u64::from_str_radix(value, 16).map_err(|_| UsbBackendError::InvalidSysfsValue {
            path: path.display().to_string(),
            value: value.to_owned(),
        })?;
    u16::try_from(parsed).map_err(|_| UsbBackendError::InvalidSysfsValue {
        path: path.display().to_string(),
        value: value.to_owned(),
    })
}

fn read_decimal_i64(base: &Path, name: &str) -> Result<i64, UsbBackendError> {
    let path = base.join(name);
    let value = read_required(&path)?;
    value
        .trim()
        .parse::<i64>()
        .map_err(|_| UsbBackendError::InvalidSysfsValue {
            path: path.display().to_string(),
            value,
        })
}

fn read_required(path: &Path) -> Result<String, UsbBackendError> {
    fs::read_to_string(path).map_err(|source| UsbBackendError::Io {
        path: path.display().to_string(),
        source,
    })
}

fn read_optional_sysfs(path: &Path) -> Result<Option<String>, UsbBackendError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let mut value = String::from_utf8(bytes).map_err(|_| UsbBackendError::InvalidUtf8 {
        path: path.display().to_string(),
    })?;
    // The pinned source removes the final decoded character, which is normally
    // the sysfs newline and is observable for malformed synthetic fixtures.
    value.pop();
    Ok(Some(value))
}

fn iterdir_safe(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn make_record(
    device: &Path,
    resolved_device: &Path,
    vid: Option<u16>,
    pid: Option<u16>,
    bcd_device: Option<u16>,
    interface_num: Option<u16>,
    serial_number: Option<String>,
    manufacturer: Option<String>,
    product: Option<String>,
    interface_description: Option<String>,
) -> SerialPortInfo {
    let resolved_device = resolved_device.to_string_lossy().into_owned();
    let description = source_description(
        product.as_deref(),
        interface_description.as_deref(),
        &resolved_device,
    );
    SerialPortInfo {
        device: device.to_string_lossy().into_owned(),
        resolved_device: Some(resolved_device),
        vid,
        pid,
        serial_number,
        manufacturer,
        description: Some(description),
        bcd_device: bcd_device.map(i64::from),
        interface_description,
        interface_num: interface_num.map(i64::from),
    }
}
