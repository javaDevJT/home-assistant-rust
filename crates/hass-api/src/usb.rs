//! WebSocket adapter helpers for the USB discovery integration.
//!
//! The command dispatcher owns role and request-schema validation. These
//! helpers keep the USB request semantics and response projection in one
//! place so the dispatcher can map [`hass_usb::UsbError`] without changing its
//! message.

use std::collections::{HashMap, HashSet};

use hass_usb::{
    PortDevice, SerialDevice, SerialPortConsumer, SerialPortConsumerKind, SerialPortScan, UsbError,
    UsbManager,
};
use serde_json::{Value, json};

/// App-owned snapshot of actual serial-port users and USB discovery flows.
///
/// This is intentionally separate from `UsbManager`: config entries, their
/// loaded states, and in-progress flow data belong to the app's ConfigEntries
/// owner. Supervisor apps may be added by an app that has that source; an
/// absent Supervisor provider must not be replaced with manifest guesses.
pub trait UsbUsageProvider: Send + Sync {
    /// Return current configured-port consumers and USB-source flows.
    fn snapshot(&self) -> UsbUsageSnapshot;

    /// Resolve a configured or scanned path using the app's realpath policy.
    /// This is called only for `usb/list_serial_ports` requests with usage.
    fn realpath(&self, path: &str) -> String;

    /// Custom-component domains that override core integrations.
    fn custom_component_domains(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Data needed to add usage information to the serial-port listing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsbUsageSnapshot {
    /// Literal configured serial-port claims from eligible config entries.
    pub consumers: Vec<UsbUsageClaim>,
    /// In-progress flows whose typed USB initial data contains a device path.
    pub discovery_flows: Vec<UsbDiscoveryFlowUsage>,
}

/// One configured path and its exact public consumer record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsbUsageClaim {
    pub path: String,
    pub consumer: SerialPortConsumer,
}

/// Public fields from a USB-source in-progress flow.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsbDiscoveryFlowUsage {
    /// `initial_data.device` from the typed USB service-info payload.
    pub device: String,
    pub flow_id: String,
    pub domain: String,
}

/// Request a scan through the SDK's manual-scan route.
///
/// `UsbManager` suppresses this request when a hotplug observer is active and
/// otherwise applies the source-compatible immediate/debounced behavior.
pub(crate) fn request_scan(manager: &UsbManager) -> Result<(), UsbError> {
    manager.request_scan().map(|_| ())
}

/// Freshly enumerate and serialize the USB and non-USB serial-port records.
pub(crate) fn list_serial_ports(
    manager: &UsbManager,
    include_usage: bool,
    usage_provider: Option<&dyn UsbUsageProvider>,
) -> Result<Vec<Value>, UsbError> {
    let scan = manager.scan_serial_ports()?;
    for failure in &scan.scanner_failures {
        tracing::warn!(
            target: "hass_api::usb",
            scanner_id = failure.scanner_id,
            error = %failure.error,
            "USB serial-port scanner callback failed"
        );
    }
    let usage = include_usage.then(|| {
        usage_provider
            .map(UsbUsageProvider::snapshot)
            .unwrap_or_default()
    });
    let paths = PathContext::new(usage_provider.filter(|_| include_usage), &scan);

    let mut consumers_by_device: HashMap<String, Vec<SerialPortConsumer>> = HashMap::new();
    let mut missing_device_order = Vec::new();
    let mut flows_by_device: HashMap<String, Vec<Value>> = HashMap::new();
    let scanned_devices: HashSet<&str> = scan.ports.iter().map(PortDevice::device).collect();

    if let (Some(snapshot), Some(provider)) = (usage.as_ref(), usage_provider) {
        for claim in &snapshot.consumers {
            let Some(device) = paths.claim_device(
                provider,
                &claim.path,
                claim.consumer.domain.as_deref().unwrap_or_default(),
                &scanned_devices,
            ) else {
                continue;
            };
            let is_scanned = scanned_devices.contains(device.as_str());

            // Supervisor app options are useful only for currently enumerated
            // ports. Config-entry claims are retained even when the device is
            // missing so the UI can show present=false.
            if claim.consumer.kind == SerialPortConsumerKind::App && !is_scanned {
                continue;
            }

            let consumers = consumers_by_device.entry(device.clone()).or_default();
            if !is_scanned && consumers.is_empty() && !missing_device_order.contains(&device) {
                missing_device_order.push(device.clone());
            }
            if !consumers.contains(&claim.consumer) {
                consumers.push(claim.consumer.clone());
            }
        }

        // Upstream discovery flows match the path in typed USB init data to
        // the enumerated USB port's exact device path.
        for flow in &snapshot.discovery_flows {
            if scanned_devices.contains(flow.device.as_str()) {
                flows_by_device
                    .entry(flow.device.clone())
                    .or_default()
                    .push(json!({ "flow_id": flow.flow_id, "domain": flow.domain }));
            }
        }
    }

    let mut result = Vec::with_capacity(scan.ports.len() + missing_device_order.len());
    for port in &scan.ports {
        let integrations = match port {
            PortDevice::Usb(device) => {
                let mut seen = HashSet::new();
                manager
                    .matching_integrations(device)?
                    .into_iter()
                    .filter(|domain| seen.insert(domain.clone()))
                    .collect()
            }
            PortDevice::Serial(_) => Vec::new(),
        };
        let device = port.device();
        let mut row = serialize_port(port, integrations, true);
        if include_usage {
            row["discovery_flows"] = json!(flows_by_device.remove(device).unwrap_or_default());
            row["consumers"] = json!(
                consumers_by_device
                    .remove(device)
                    .unwrap_or_default()
                    .iter()
                    .map(serialize_consumer)
                    .collect::<Vec<_>>()
            );
        }
        result.push(row);
    }

    if include_usage {
        for device in missing_device_order {
            if scanned_devices.contains(device.as_str()) {
                continue;
            }
            let consumers = consumers_by_device.remove(&device).unwrap_or_default();
            if consumers.is_empty() {
                continue;
            }
            let port = PortDevice::Serial(SerialDevice::new(device.clone()));
            let present = is_unscannable_port(&device);
            let mut row = serialize_port(&port, Vec::new(), present);
            row["discovery_flows"] = json!([]);
            row["consumers"] = json!(consumers.iter().map(serialize_consumer).collect::<Vec<_>>());
            result.push(row);
        }
    }

    Ok(result)
}

/// Return the generated SerialPortSelector domains, excluding custom overrides.
pub(crate) fn list_serial_integrations(
    usage_provider: Option<&dyn UsbUsageProvider>,
) -> Vec<String> {
    let overridden: HashSet<String> = usage_provider
        .map(UsbUsageProvider::custom_component_domains)
        .unwrap_or_default()
        .into_iter()
        .collect();
    hass_usb::SERIAL_PORT_SELECTOR_INTEGRATIONS
        .iter()
        .filter(|domain| !overridden.contains(**domain))
        .map(|domain| (*domain).to_owned())
        .collect()
}

struct PathContext {
    aliases: HashMap<String, String>,
}

impl PathContext {
    fn new(provider: Option<&dyn UsbUsageProvider>, scan: &SerialPortScan) -> Self {
        let Some(provider) = provider else {
            return Self {
                aliases: HashMap::new(),
            };
        };

        let mut aliases = HashMap::new();
        for port in &scan.ports {
            let resolved_device = match port {
                PortDevice::Usb(device) => device.resolved_device.as_deref(),
                PortDevice::Serial(device) => device.resolved_device.as_deref(),
            };
            let resolved = resolved_device
                .map(str::to_owned)
                .unwrap_or_else(|| provider.realpath(port.device()));
            aliases.insert(resolved, port.device().to_owned());
            aliases.insert(port.device().to_owned(), port.device().to_owned());
        }
        Self { aliases }
    }

    fn claim_device(
        &self,
        provider: &dyn UsbUsageProvider,
        path: &str,
        domain: &str,
        known_devices: &HashSet<&str>,
    ) -> Option<String> {
        let path = serial_port_from_config_value(path, domain, known_devices)?;
        let reported = reportable_path(&path);
        if is_unscannable_port(&path) || is_scanned_port_url(&path) {
            return Some(reported);
        }
        let resolved = provider.realpath(&reported);
        Some(self.aliases.get(&resolved).cloned().unwrap_or(reported))
    }
}

fn serialize_port(port: &PortDevice, matching_integrations: Vec<String>, present: bool) -> Value {
    let (
        resolved_device,
        serial_number,
        manufacturer,
        description,
        interface_description,
        interface_num,
    ) = match port {
        PortDevice::Usb(device) => (
            device.resolved_device.as_deref(),
            device.serial_number.as_deref(),
            device.manufacturer.as_deref(),
            device.description.as_deref(),
            device.interface_description.as_deref(),
            device.interface_num,
        ),
        PortDevice::Serial(device) => (
            device.resolved_device.as_deref(),
            device.serial_number.as_deref(),
            device.manufacturer.as_deref(),
            device.description.as_deref(),
            device.interface_description.as_deref(),
            device.interface_num,
        ),
    };
    let mut row = json!({
        "device": reportable_path(port.device()),
        "resolved_device": resolved_device,
        "serial_number": serial_number,
        "manufacturer": manufacturer,
        "description": description,
        "interface_description": interface_description,
        "interface_num": interface_num,
        "matching_integrations": matching_integrations,
        "present": present,
    });
    if let PortDevice::Usb(device) = port {
        row["vid"] = json!(device.vid);
        row["pid"] = json!(device.pid);
        row["bcd_device"] = json!(device.bcd_device);
    }
    row
}

fn serialize_consumer(consumer: &SerialPortConsumer) -> Value {
    json!({
        "kind": consumer.kind.as_str(),
        "title": consumer.title,
        "active": consumer.active,
        "domain": consumer.domain,
        "config_entry_id": consumer.config_entry_id,
        "slug": consumer.slug,
    })
}

const UNSCANNABLE_PORT_SCHEMES: [&str; 4] = ["esphome://", "rfc2217://", "socket://", "tcp://"];

const SCANNED_PORT_SCHEMES: [&str; 1] = ["esphome-hass://"];

fn serial_port_from_config_value(
    value: &str,
    domain: &str,
    known_devices: &HashSet<&str>,
) -> Option<String> {
    if known_devices.contains(value) {
        return Some(value.to_owned());
    }
    if is_scanned_port_url(value) {
        return Some(value.to_owned());
    }
    if is_unscannable_port(value) {
        return Some(reportable_path(value));
    }

    let mut path = value.to_owned();
    if matches!(domain, "elkm1" | "upb") {
        path = path.strip_prefix("serial://").unwrap_or(&path).to_owned();
        path = path.strip_prefix("device://").unwrap_or(&path).to_owned();
        if let Some((base, suffix)) = path.rsplit_once(':')
            && !suffix.is_empty()
            && suffix.chars().all(|character| character.is_ascii_digit())
        {
            path = base.to_owned();
        }
    }

    path.starts_with("/dev/").then_some(path)
}

fn is_scanned_port_url(path: &str) -> bool {
    SCANNED_PORT_SCHEMES
        .iter()
        .any(|scheme| path.starts_with(scheme))
}

fn is_unscannable_port(path: &str) -> bool {
    UNSCANNABLE_PORT_SCHEMES
        .iter()
        .any(|scheme| path.starts_with(scheme))
}

fn reportable_path(path: &str) -> String {
    if is_unscannable_port(path) {
        // The source strips query material from these claimed URLs. In
        // particular, this prevents Z-Wave's ESPHome `?key=` PSK from being
        // included in a response or a missing-port row.
        path.split_once('?')
            .map_or_else(|| path.to_owned(), |(base, _)| base.to_owned())
    } else {
        path.to_owned()
    }
}
