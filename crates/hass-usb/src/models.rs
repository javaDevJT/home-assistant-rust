use std::fmt;

/// A serial port record without complete USB vendor and product identifiers.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SerialDevice {
    pub device: String,
    pub resolved_device: Option<String>,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub description: Option<String>,
    pub interface_description: Option<String>,
    pub interface_num: Option<i64>,
}

impl SerialDevice {
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            resolved_device: None,
            serial_number: None,
            manufacturer: None,
            description: None,
            interface_description: None,
            interface_num: None,
        }
    }
}

/// USB serial device data. Equality and hashing include the path and every
/// metadata field, matching the source dataclass snapshot semantics.
#[allow(non_camel_case_types)]
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct USBDevice {
    pub device: String,
    pub resolved_device: Option<String>,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub description: Option<String>,
    pub interface_description: Option<String>,
    pub interface_num: Option<i64>,
    pub vid: String,
    pub pid: String,
    pub bcd_device: Option<i64>,
}

impl USBDevice {
    pub fn as_serial_device(&self) -> SerialDevice {
        SerialDevice {
            device: self.device.clone(),
            resolved_device: self.resolved_device.clone(),
            serial_number: self.serial_number.clone(),
            manufacturer: self.manufacturer.clone(),
            description: self.description.clone(),
            interface_description: self.interface_description.clone(),
            interface_num: self.interface_num,
        }
    }
}

/// A scanned serial port, retaining the distinction between USB and other
/// serial devices for listing APIs and USB-only discovery diffs.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PortDevice {
    Usb(USBDevice),
    Serial(SerialDevice),
}

impl PortDevice {
    pub fn device(&self) -> &str {
        match self {
            Self::Usb(port) => &port.device,
            Self::Serial(port) => &port.device,
        }
    }

    pub fn as_usb(&self) -> Option<&USBDevice> {
        match self {
            Self::Usb(port) => Some(port),
            Self::Serial(_) => None,
        }
    }
}

/// Provider-neutral equivalent of `serialx.SerialPortInfo`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SerialPortInfo {
    pub device: String,
    pub resolved_device: Option<String>,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub description: Option<String>,
    pub bcd_device: Option<i64>,
    pub interface_description: Option<String>,
    pub interface_num: Option<i64>,
}

/// Integration configuration-flow payload. It intentionally omits resolved
/// path, BCD revision, and interface metadata.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UsbServiceInfo {
    pub device: String,
    pub vid: String,
    pub pid: String,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub description: Option<String>,
}

/// USB page consumer model, separate from the discovery flow payload.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SerialPortConsumer {
    pub kind: SerialPortConsumerKind,
    pub title: String,
    pub active: bool,
    pub domain: Option<String>,
    pub config_entry_id: Option<String>,
    pub slug: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SerialPortConsumerKind {
    ConfigEntry,
    App,
}

impl SerialPortConsumerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfigEntry => "config_entry",
            Self::App => "app",
        }
    }
}

/// Convert a provider record to a USB device. The source requires both IDs;
/// a partial VID/PID pair remains a [`SerialDevice`].
pub fn usb_serial_device_from_port(port: SerialPortInfo) -> PortDevice {
    match (port.vid, port.pid) {
        (Some(_), Some(_)) => PortDevice::Usb(usb_device_from_port(port)),
        _ => PortDevice::Serial(SerialDevice {
            device: port.device,
            resolved_device: port.resolved_device,
            serial_number: port.serial_number,
            manufacturer: port.manufacturer,
            description: port.description,
            interface_description: port.interface_description,
            interface_num: port.interface_num,
        }),
    }
}

/// Convert a provider record known to contain VID and PID. Like the source
/// helper, this asserts that both identifiers are present.
pub fn usb_device_from_port(port: SerialPortInfo) -> USBDevice {
    let SerialPortInfo {
        device,
        resolved_device,
        vid,
        pid,
        serial_number,
        manufacturer,
        description,
        bcd_device,
        interface_description,
        interface_num,
    } = port;
    USBDevice {
        device,
        resolved_device,
        serial_number,
        manufacturer,
        description,
        interface_description,
        interface_num,
        vid: format!("{:04X}", vid.expect("USB port has no VID")),
        pid: format!("{:04X}", pid.expect("USB port has no PID")),
        bcd_device,
    }
}

pub fn usb_service_info_from_device(device: &USBDevice) -> UsbServiceInfo {
    UsbServiceInfo {
        device: device.device.clone(),
        vid: device.vid.clone(),
        pid: device.pid.clone(),
        serial_number: device.serial_number.clone(),
        manufacturer: device.manufacturer.clone(),
        description: device.description.clone(),
    }
}

/// Preserve the source unique-ID output, including its collisions and Python
/// `None` spelling. This consumer helper is never used as scan identity.
pub fn usb_unique_id_from_service_info(info: &UsbServiceInfo) -> String {
    format!(
        "{}:{}_{}_{}_{}",
        info.vid,
        info.pid,
        option_as_python(info.serial_number.as_deref()),
        option_as_python(info.manufacturer.as_deref()),
        option_as_python(info.description.as_deref()),
    )
}

fn option_as_python(value: Option<&str>) -> String {
    value.map_or_else(|| "None".to_owned(), str::to_owned)
}

/// Port identity policy supplied by the pinned serial provider equivalent.
/// The native SDK does not guess which serial numbers the provider considers
/// placeholders or non-unique.
pub fn has_unique_serial_number_with(
    port: &SerialDevice,
    is_non_unique_serial: impl FnOnce(Option<&str>) -> bool,
) -> bool {
    !is_non_unique_serial(port.serial_number.as_deref())
}

impl fmt::Display for PortDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.device())
    }
}
