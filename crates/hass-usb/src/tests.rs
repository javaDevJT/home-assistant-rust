use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Barrier, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use crate::{
    ADD_REMOVE_SCAN_COOLDOWN, CORE_USB_MATCHER_SPECS, DebounceRequest, DiscoveryAction,
    HostPlatform, MANUAL_SCAN_COOLDOWN, PathResolver, PortDevice, ScheduledTask, SerialDevice,
    SerialNumberPolicy, SerialPortConsumerKind, SerialPortInfo, SerialPortProvider, TimerScheduler,
    USBDevice, USBMatcher, UsbError, UsbHotplugWatcher, UsbIntegration, UsbManager, UsbOptions,
    UsbServiceInfo, WatcherStartError, find_moved_serial_port, matching_usb_matchers,
    merge_usb_matchers, serial_path_udev_id, usb_device_from_path, usb_serial_device_from_port,
    usb_service_info_from_device, usb_unique_id_from_service_info,
};

fn device(path: &str) -> USBDevice {
    USBDevice {
        device: path.to_owned(),
        resolved_device: Some(format!("/resolved{path}")),
        serial_number: Some("USB-SERIAL-1".to_owned()),
        manufacturer: Some("Acme Instruments".to_owned()),
        description: Some("Bridge Model B".to_owned()),
        interface_description: Some("Serial interface".to_owned()),
        interface_num: Some(2),
        vid: "1234".to_owned(),
        pid: "5678".to_owned(),
        bcd_device: Some(257),
    }
}

fn matcher(
    domain: &str,
    vid: Option<&str>,
    pid: Option<&str>,
    serial_number: Option<&str>,
    manufacturer: Option<&str>,
    description: Option<&str>,
) -> USBMatcher {
    USBMatcher {
        domain: domain.to_owned(),
        vid: vid.map(str::to_owned),
        pid: pid.map(str::to_owned),
        serial_number: serial_number.map(str::to_owned),
        manufacturer: manufacturer.map(str::to_owned),
        description: description.map(str::to_owned),
    }
}

fn provider_port(path: &str, vid: Option<u16>, pid: Option<u16>) -> SerialPortInfo {
    SerialPortInfo {
        device: path.to_owned(),
        resolved_device: Some(format!("/real{path}")),
        vid,
        pid,
        serial_number: Some("USB-SERIAL-1".to_owned()),
        manufacturer: Some("Acme Instruments".to_owned()),
        description: Some("Bridge Model B".to_owned()),
        bcd_device: Some(257),
        interface_description: Some("Serial interface".to_owned()),
        interface_num: Some(2),
    }
}

fn serial_device(path: &str) -> PortDevice {
    PortDevice::Serial(SerialDevice::new(path))
}

fn policy() -> Arc<dyn SerialNumberPolicy> {
    Arc::new(|serial: Option<&str>| serial.is_none() || serial == Some("placeholder"))
}

#[derive(Default)]
struct MutableProvider {
    ports: Mutex<Vec<SerialPortInfo>>,
    error: Mutex<Option<UsbError>>,
    calls: AtomicUsize,
}

impl MutableProvider {
    fn new(ports: Vec<SerialPortInfo>) -> Self {
        Self {
            ports: Mutex::new(ports),
            error: Mutex::new(None),
            calls: AtomicUsize::new(0),
        }
    }

    fn set_ports(&self, ports: Vec<SerialPortInfo>) {
        *lock(&self.ports) = ports;
        *lock(&self.error) = None;
    }

    fn fail(&self, message: &str) {
        *lock(&self.error) = Some(UsbError::new(message));
    }
}

impl SerialPortProvider for MutableProvider {
    fn list_serial_ports(&self) -> Result<Vec<SerialPortInfo>, UsbError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = lock(&self.error).clone() {
            return Err(error);
        }
        Ok(lock(&self.ports).clone())
    }
}

#[derive(Default)]
struct SchedulerState {
    now: Duration,
    next_id: u64,
    tasks: BTreeMap<(Duration, u64), Box<dyn FnOnce() + Send>>,
    canceled: BTreeSet<u64>,
}

#[derive(Default)]
struct ManualScheduler {
    state: Arc<Mutex<SchedulerState>>,
}

impl ManualScheduler {
    fn advance_by(&self, delta: Duration) {
        let target = lock(&self.state).now + delta;
        loop {
            let next = {
                let mut state = lock(&self.state);
                let Some(key) = state.tasks.keys().next().copied() else {
                    state.now = target;
                    return;
                };
                if key.0 > target {
                    state.now = target;
                    return;
                }
                state.now = key.0;
                let callback = state.tasks.remove(&key);
                let canceled = state.canceled.remove(&key.1);
                if canceled { None } else { callback }
            };
            if let Some(callback) = next {
                callback();
            }
        }
    }

    fn pending_tasks(&self) -> usize {
        lock(&self.state).tasks.len()
    }
}

struct ManualTask {
    id: u64,
    state: Weak<Mutex<SchedulerState>>,
}

impl ScheduledTask for ManualTask {
    fn cancel(&self) {
        if let Some(state) = self.state.upgrade() {
            lock(&state).canceled.insert(self.id);
        }
    }
}

impl TimerScheduler for ManualScheduler {
    fn schedule(
        &self,
        delay: Duration,
        callback: Box<dyn FnOnce() + Send + 'static>,
    ) -> Box<dyn ScheduledTask> {
        let id = {
            let mut state = lock(&self.state);
            state.next_id += 1;
            let id = state.next_id;
            let due = state.now + delay;
            state.tasks.insert((due, id), callback);
            id
        };
        Box::new(ManualTask {
            id,
            state: Arc::downgrade(&self.state),
        })
    }
}

fn integration(
    provider: Arc<dyn SerialPortProvider>,
    scheduler: Arc<dyn TimerScheduler>,
    matchers: Vec<USBMatcher>,
    platform: HostPlatform,
) -> (UsbIntegration, UsbManager) {
    let owner = UsbIntegration::new(
        provider,
        scheduler,
        UsbOptions::new(matchers).with_platform(platform),
        policy(),
    );
    let manager = owner.manager();
    (owner, manager)
}

#[test]
fn provider_models_and_service_info_preserve_exact_source_fields() {
    let port = provider_port("/dev/ttyUSB0", Some(0x10C4), Some(0xEA60));
    let PortDevice::Usb(usb) = usb_serial_device_from_port(port.clone()) else {
        panic!("complete VID/PID must produce USBDevice");
    };
    assert_eq!(usb.vid, "10C4");
    assert_eq!(usb.pid, "EA60");
    assert_eq!(usb.bcd_device, Some(257));
    assert_eq!(
        usb.interface_description.as_deref(),
        Some("Serial interface")
    );
    assert_eq!(usb.interface_num, Some(2));
    assert_eq!(usb.resolved_device.as_deref(), Some("/real/dev/ttyUSB0"));

    let mut partial = port;
    partial.pid = None;
    assert!(matches!(
        usb_serial_device_from_port(partial),
        PortDevice::Serial(_)
    ));

    let info = usb_service_info_from_device(&usb);
    assert_eq!(
        info,
        UsbServiceInfo {
            device: "/dev/ttyUSB0".to_owned(),
            vid: "10C4".to_owned(),
            pid: "EA60".to_owned(),
            serial_number: Some("USB-SERIAL-1".to_owned()),
            manufacturer: Some("Acme Instruments".to_owned()),
            description: Some("Bridge Model B".to_owned()),
        }
    );

    let missing_metadata = UsbServiceInfo {
        device: "/dev/ttyA".to_owned(),
        vid: "1111".to_owned(),
        pid: "2222".to_owned(),
        serial_number: None,
        manufacturer: None,
        description: None,
    };
    let mut other_path = missing_metadata.clone();
    other_path.device = "/dev/ttyB".to_owned();
    assert_eq!(
        usb_unique_id_from_service_info(&missing_metadata),
        "1111:2222_None_None_None"
    );
    assert_eq!(
        usb_unique_id_from_service_info(&missing_metadata),
        usb_unique_id_from_service_info(&other_path),
        "consumer identity may collide when metadata is absent"
    );
    assert_eq!(SerialPortConsumerKind::App.as_str(), "app");
    assert_eq!(SerialPortConsumerKind::ConfigEntry.as_str(), "config_entry");
}

#[test]
fn matcher_case_wildcards_specificity_and_stable_ties_match_source() {
    let mut observed = device("/dev/ttyA");
    observed.serial_number = Some("ABC-12".to_owned());
    observed.description = Some("Bridge B".to_owned());
    assert!(crate::usb_device_matches_matcher(
        &observed,
        &matcher("good", Some("1234"), None, Some("abc-*"), None, None)
    ));
    assert!(!crate::usb_device_matches_matcher(
        &observed,
        &matcher("bad-vid-case", Some("abcd"), None, None, None, None)
    ));
    assert!(!crate::usb_device_matches_matcher(
        &observed,
        &matcher(
            "pattern-is-not-lowered",
            None,
            None,
            Some("ABC-*"),
            None,
            None
        )
    ));
    assert!(crate::usb_device_matches_matcher(
        &observed,
        &matcher("class", None, None, None, None, Some("bridge [a-c]"))
    ));
    let mut missing_serial = observed.clone();
    missing_serial.serial_number = None;
    assert!(!crate::usb_device_matches_matcher(
        &missing_serial,
        &matcher("missing-is-false", None, None, Some("*"), None, None)
    ));

    let matchers = vec![
        matcher("less-specific", Some("1234"), None, None, None, None),
        matcher("first-tie", Some("1234"), Some("5678"), None, None, None),
        matcher("second-tie", Some("1234"), Some("5678"), None, None, None),
        matcher(
            "too-specific-but-no-match",
            Some("1234"),
            Some("5678"),
            None,
            None,
            Some("other"),
        ),
    ];
    assert_eq!(
        matching_usb_matchers(&observed, &matchers)
            .into_iter()
            .map(|entry| entry.domain.as_str())
            .collect::<Vec<_>>(),
        ["first-tie", "second-tie"]
    );
    assert_eq!(matchers[0].specificity(), 2);
    assert_eq!(
        merge_usb_matchers(&matchers[..1], &matchers[1..])[0].domain,
        "less-specific"
    );
}

#[test]
fn generated_matcher_and_selector_order_are_pinned() {
    assert_eq!(CORE_USB_MATCHER_SPECS.len(), 32);
    assert_eq!(
        crate::generated::SERIAL_PORT_SELECTOR_INTEGRATIONS.len(),
        19
    );
    assert_eq!(crate::SERIAL_CONFIG_ENTRY_DOMAINS.len(), 39);
    let generated = crate::generated::pinned_usb_matchers();
    assert_eq!(generated[0].domain, "enocean");
    assert_eq!(generated[1].domain, "homeassistant_connect_zbt2");
    assert_eq!(generated[31].domain, "zwave_js");
    assert_eq!(generated[0].description.as_deref(), Some("*usb 300*"));
    assert!(generated.iter().all(|entry| entry.serial_number.is_none()));
    assert!(crate::config_entry_uses_serial_port("aerosmart"));
    assert!(crate::config_entry_uses_serial_port("mysensors"));
    assert!(!crate::config_entry_uses_serial_port(
        "not-a-core-serial-domain"
    ));
    assert!(crate::integration_uses_serial_port(
        "custom_usb",
        &["usb"],
        &[]
    ));
    assert!(crate::integration_uses_serial_port(
        "custom_after",
        &[],
        &["usb"]
    ));
    assert!(crate::integration_uses_serial_port(
        "alarmdecoder",
        &[],
        &[]
    ));
}

#[test]
fn scanner_merge_replaces_exact_path_in_place_and_isolates_failures() {
    let provider = Arc::new(MutableProvider::new(vec![
        provider_port("/dev/ttyA", Some(0x1234), Some(0x5678)),
        provider_port("/dev/ttyB", Some(0x1234), Some(0x5678)),
        provider_port("/dev/ttyS0", None, Some(0x5678)),
    ]));
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) =
        integration(provider, scheduler.clone(), Vec::new(), HostPlatform::Other);
    let first = manager
        .register_serial_port_scanner(|| {
            let mut one = device("/dev/ttyA");
            one.description = Some("first scanner".to_owned());
            Ok(vec![
                PortDevice::Usb(one),
                PortDevice::Usb(device("remote://one")),
            ])
        })
        .unwrap();
    let failure = manager
        .register_serial_port_scanner(|| Err(UsbError::new("scanner unavailable")))
        .unwrap();
    let later = manager
        .register_serial_port_scanner(|| {
            let mut replacement = device("/dev/ttyA");
            replacement.description = Some("later scanner wins".to_owned());
            Ok(vec![
                PortDevice::Usb(replacement),
                PortDevice::Usb(device("remote://two")),
            ])
        })
        .unwrap();

    let scan = manager.scan_serial_ports().unwrap();
    assert_eq!(scan.ports.len(), 5);
    assert_eq!(scan.ports[0].device(), "/dev/ttyA");
    assert_eq!(scan.ports[1].device(), "/dev/ttyB");
    assert_eq!(scan.ports[2].device(), "/dev/ttyS0");
    assert_eq!(scan.ports[3].device(), "remote://one");
    assert_eq!(scan.ports[4].device(), "remote://two");
    assert_eq!(
        scan.ports[0].as_usb().unwrap().description.as_deref(),
        Some("later scanner wins")
    );
    assert_eq!(scan.scanner_failures.len(), 1);
    assert_eq!(
        scan.scanner_failures[0].error.message(),
        "scanner unavailable"
    );

    // Dropping one exact registration leaves the other scanner active.
    drop(first);
    drop(later);
    drop(failure);
    let second = manager.scan_serial_ports().unwrap();
    assert_eq!(second.ports.len(), 3);
    assert_eq!(second.ports[0].device(), "/dev/ttyA");
    owner.stop().unwrap();
    assert_eq!(scheduler.pending_tasks(), 0);
}

#[test]
fn metadata_change_is_remove_then_add_and_actions_have_no_usb_key() {
    let provider = Arc::new(MutableProvider::new(vec![provider_port(
        "/dev/serial/by-id/usb-a",
        Some(0x1234),
        Some(0x5678),
    )]));
    let scheduler = Arc::new(ManualScheduler::default());
    let matchers = vec![
        matcher("less", Some("1234"), None, None, None, None),
        matcher("tie-a", Some("1234"), Some("5678"), None, None, None),
        matcher("tie-b", Some("1234"), Some("5678"), None, None, None),
    ];
    let (owner, manager) = integration(provider.clone(), scheduler, matchers, HostPlatform::Other);
    let seen = Arc::new(Mutex::new(Vec::<DiscoveryAction>::new()));
    let seen_callback = Arc::clone(&seen);
    let _action_guard = manager
        .register_discovery_callback(move |action| {
            lock(&seen_callback).push(action.clone());
            Ok(())
        })
        .unwrap();
    let initial_order = Arc::new(Mutex::new(Vec::<u8>::new()));
    let order_one = Arc::clone(&initial_order);
    let _initial_one = manager
        .register_initial_scan_callback(move || {
            lock(&order_one).push(1);
            Ok(())
        })
        .unwrap();
    let order_two = Arc::clone(&initial_order);
    let _initial_two = manager
        .register_initial_scan_callback(move || {
            lock(&order_two).push(2);
            Ok(())
        })
        .unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_callback = Arc::clone(&requests);
    let _request_guard = manager
        .register_scan_request_callback(move || {
            requests_callback.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();

    let first = manager.scan_now().unwrap();
    assert!(first.initial_scan_completed);
    assert!(first.removed_devices.is_empty());
    assert_eq!(first.added_devices.len(), 1);
    assert_eq!(*lock(&initial_order), [2, 1]);
    let first_actions = lock(&seen).clone();
    assert_eq!(first_actions.len(), 2);
    for (action, domain) in first_actions.iter().zip(["tie-a", "tie-b"]) {
        let DiscoveryAction::Create(create) = action else {
            panic!("first scan should create flows");
        };
        assert_eq!(create.domain, domain);
        assert_eq!(create.source, "usb");
        assert_eq!(create.context.source, "usb");
        assert_eq!(create.data.device, "/dev/serial/by-id/usb-a");
    }
    let immediate_calls = Arc::new(AtomicUsize::new(0));
    let immediate_count = Arc::clone(&immediate_calls);
    let _immediate_guard = manager
        .register_initial_scan_callback(move || {
            immediate_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    assert_eq!(immediate_calls.load(Ordering::SeqCst), 1);

    let mut changed = provider_port("/dev/serial/by-id/usb-a", Some(0x1234), Some(0x5678));
    changed.description = Some("new metadata".to_owned());
    provider.set_ports(vec![changed]);
    let second = manager.scan_now().unwrap();
    assert_eq!(second.added_devices.len(), 1);
    assert_eq!(second.removed_devices.len(), 1);
    assert_eq!(second.actions.len(), 3);
    let DiscoveryAction::Dismiss(dismiss) = &second.actions[0] else {
        panic!("metadata change emits removal before additions");
    };
    assert_eq!(dismiss.data.description.as_deref(), Some("Bridge Model B"));
    assert_eq!(dismiss.matching_domains, ["tie-a", "tie-b"]);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "direct scans skip request callbacks"
    );

    let all_records = [device("/dev/ttyA"), device("/dev/ttyB")];
    assert_eq!(
        usb_unique_id_from_service_info(&usb_service_info_from_device(&all_records[0])),
        usb_unique_id_from_service_info(&usb_service_info_from_device(&all_records[1])),
        "the helper omits device path, but records remain path-specific"
    );
    let mut path_specific = BTreeSet::new();
    path_specific.extend(all_records);
    assert_eq!(path_specific.len(), 2);
    owner.stop().unwrap();
}

#[test]
fn non_usb_ports_never_enter_discovery_diff_and_empty_success_finishes_initial_scan() {
    let provider = Arc::new(MutableProvider::new(vec![provider_port(
        "/dev/ttyS0",
        None,
        Some(0x5678),
    )]));
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) = integration(
        provider,
        scheduler,
        vec![matcher("all-usb", None, None, None, None, None)],
        HostPlatform::Other,
    );
    let report = manager.scan_now().unwrap();
    assert_eq!(report.serial_ports.ports.len(), 1);
    assert!(matches!(
        &report.serial_ports.ports[0],
        PortDevice::Serial(_)
    ));
    assert!(report.usb_devices.is_empty());
    assert!(report.actions.is_empty());
    assert!(report.initial_scan_completed);
    assert!(manager.initial_scan_done().unwrap());
    owner.stop().unwrap();
}

#[test]
fn event_callback_errors_are_isolated_and_exact_callback_guard_removes_one() {
    let provider = Arc::new(MutableProvider::new(vec![provider_port(
        "/dev/ttyA",
        Some(0x1234),
        Some(0x5678),
    )]));
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) = integration(provider, scheduler, Vec::new(), HostPlatform::Other);
    let first_calls = Arc::new(AtomicUsize::new(0));
    let first_counter = Arc::clone(&first_calls);
    let first = manager
        .register_port_event_callback(move |_, _| {
            first_counter.fetch_add(1, Ordering::SeqCst);
            Err(UsbError::new("callback error"))
        })
        .unwrap();
    let second_calls = Arc::new(AtomicUsize::new(0));
    let second_counter = Arc::clone(&second_calls);
    let second = manager
        .register_port_event_callback(move |added, _| {
            assert_eq!(added.len(), 1);
            second_counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();

    let report = manager.scan_now().unwrap();
    assert_eq!(report.callback_failures.len(), 1);
    assert_eq!(
        report.callback_failures[0].error.message(),
        "callback error"
    );
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);

    drop(first);
    manager.scan_now().unwrap(); // unchanged snapshot does not notify either callback
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    drop(second);
    owner.stop().unwrap();
}

#[test]
fn scanner_guard_removes_exact_registration_and_base_errors_propagate() {
    let provider = Arc::new(MutableProvider::new(vec![provider_port(
        "/dev/ttyA",
        Some(0x1234),
        Some(0x5678),
    )]));
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) =
        integration(provider.clone(), scheduler, Vec::new(), HostPlatform::Other);
    let first = manager
        .register_serial_port_scanner(|| {
            let mut port = device("/dev/ttyA");
            port.description = Some("first".to_owned());
            Ok(vec![PortDevice::Usb(port)])
        })
        .unwrap();
    let second = manager
        .register_serial_port_scanner(|| {
            let mut port = device("/dev/ttyA");
            port.description = Some("second".to_owned());
            Ok(vec![PortDevice::Usb(port)])
        })
        .unwrap();
    drop(first);
    assert_eq!(
        manager.scan_serial_ports().unwrap().ports[0]
            .as_usb()
            .unwrap()
            .description
            .as_deref(),
        Some("second")
    );
    drop(second);
    provider.fail("base enumeration failed");
    assert_eq!(
        manager.scan_serial_ports().unwrap_err().message(),
        "base enumeration failed"
    );
    owner.stop().unwrap();
}

#[test]
fn moved_path_requires_one_matching_unique_serial_candidate() {
    assert_eq!(
        serial_path_udev_id("/dev/serial/by-id/usb-Bridge-if00"),
        Some("usb-Bridge-if00".to_owned())
    );
    assert_eq!(
        serial_path_udev_id("esphome-hass://proxy?port_udev_id=usb%3ABridge+1"),
        Some("usb:Bridge 1".to_owned())
    );
    assert_eq!(
        serial_path_udev_id("serial://proxy?port_udev_id=&port_udev_id=valid"),
        Some("valid".to_owned())
    );
    assert_eq!(
        serial_path_udev_id("serial://proxy#fragment?port_udev_id=hidden"),
        None
    );

    let configured = "/dev/serial/by-id/usb-Bridge";
    let mut moved = device("esphome-hass://proxy?port_udev_id=usb-Bridge");
    moved.serial_number = Some("unique".to_owned());
    let non_unique = device("tcp://proxy?port_udev_id=usb-Bridge");
    let is_non_unique = |serial: Option<&str>| serial.is_none() || serial == Some("placeholder");
    assert_eq!(
        find_moved_serial_port(configured, &[moved.clone()], is_non_unique),
        Some(moved.device.clone())
    );
    assert_eq!(
        find_moved_serial_port(configured, &[moved.clone(), non_unique], is_non_unique),
        None,
        "multiple matching records are ambiguous even with a shared consumer ID"
    );
    assert_eq!(
        find_moved_serial_port(configured, &[device(configured)], is_non_unique),
        None,
        "a still-present configured path is not considered moved"
    );
    let mut placeholder = moved.clone();
    placeholder.serial_number = Some("placeholder".to_owned());
    assert_eq!(
        find_moved_serial_port(configured, &[placeholder], is_non_unique),
        None
    );
}

#[test]
fn usb_path_lookup_uses_injected_realpath_and_ignores_non_usb_ports() {
    struct FakeResolver(BTreeMap<String, String>);
    impl PathResolver for FakeResolver {
        fn realpath(&self, path: &str) -> String {
            self.0.get(path).cloned().unwrap_or_else(|| path.to_owned())
        }
    }
    let resolver = FakeResolver(BTreeMap::from([
        (
            "/dev/serial/by-id/bridge".to_owned(),
            "/dev/ttyUSB0".to_owned(),
        ),
        ("/dev/ttyUSB0".to_owned(), "/dev/ttyUSB0".to_owned()),
    ]));
    let usb = device("/dev/ttyUSB0");
    let ports = vec![serial_device("/dev/ttyUSB0"), PortDevice::Usb(usb.clone())];
    assert_eq!(
        usb_device_from_path("/dev/serial/by-id/bridge", &ports, &resolver),
        Some(usb)
    );
    assert_eq!(
        usb_device_from_path("/dev/missing", &ports, &resolver),
        None
    );
}

#[test]
fn manual_and_change_debouncers_keep_source_cooldown_and_observer_rules() {
    let provider = Arc::new(MutableProvider::new(Vec::new()));
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) = integration(
        provider.clone(),
        scheduler.clone(),
        Vec::new(),
        HostPlatform::Other,
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_callback = Arc::clone(&requests);
    let _request_guard = manager
        .register_scan_request_callback(move || {
            requests_callback.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();

    assert_eq!(manager.request_scan().unwrap(), DebounceRequest::Ran);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(manager.request_scan().unwrap(), DebounceRequest::Queued);
    scheduler.advance_by(MANUAL_SCAN_COOLDOWN - Duration::from_secs(1));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    scheduler.advance_by(Duration::from_secs(1));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(requests.load(Ordering::SeqCst), 2);

    assert_eq!(
        manager.notify_serial_ports_changed().unwrap(),
        DebounceRequest::Queued
    );
    assert_eq!(
        manager.notify_serial_ports_changed().unwrap(),
        DebounceRequest::Queued
    );
    scheduler.advance_by(ADD_REMOVE_SCAN_COOLDOWN);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    assert_eq!(requests.load(Ordering::SeqCst), 3);

    // A request during the add/remove cooldown queues one more run at its end.
    manager.notify_serial_ports_changed().unwrap();
    scheduler.advance_by(ADD_REMOVE_SCAN_COOLDOWN - Duration::from_millis(1));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    scheduler.advance_by(Duration::from_millis(1));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
    assert_eq!(requests.load(Ordering::SeqCst), 4);

    manager.set_observer_active(true).unwrap();
    assert_eq!(manager.request_scan().unwrap(), DebounceRequest::Ignored);
    manager.notify_serial_ports_changed().unwrap();
    owner.stop().unwrap();
    scheduler.advance_by(Duration::from_secs(30));
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        4,
        "stop cancels pending timers"
    );
    assert_eq!(scheduler.pending_tasks(), 0);
    assert!(manager.request_scan().is_err());
}

#[test]
fn watcher_is_strongly_owned_uses_weak_callback_and_is_joined_on_stop() {
    struct FakeWatcher {
        callback: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
        stopped: AtomicBool,
        fail: Mutex<Option<WatcherStartError>>,
    }
    impl FakeWatcher {
        fn fire(&self) {
            if let Some(callback) = lock(&self.callback).clone() {
                callback();
            }
        }
    }
    impl UsbHotplugWatcher for FakeWatcher {
        fn start(&self, callback: Arc<dyn Fn() + Send + Sync>) -> Result<(), WatcherStartError> {
            if let Some(error) = lock(&self.fail).clone() {
                return Err(error);
            }
            *lock(&self.callback) = Some(callback);
            Ok(())
        }

        fn stop_and_join(&self) -> Result<(), UsbError> {
            self.stopped.store(true, Ordering::SeqCst);
            lock(&self.callback).take();
            Ok(())
        }
    }

    let provider = Arc::new(MutableProvider::new(Vec::new()));
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) = integration(
        provider.clone(),
        scheduler.clone(),
        Vec::new(),
        HostPlatform::Other,
    );
    let watcher = Arc::new(FakeWatcher {
        callback: Mutex::new(None),
        stopped: AtomicBool::new(false),
        fail: Mutex::new(None),
    });
    let weak_watcher = Arc::downgrade(&watcher);
    let watcher_trait: Arc<dyn UsbHotplugWatcher> = watcher.clone();
    manager.start_watcher(watcher_trait).unwrap();
    drop(watcher);
    let retained = weak_watcher.upgrade().expect("owner keeps watcher alive");
    retained.fire();
    assert_eq!(manager.request_scan().unwrap(), DebounceRequest::Ignored);
    scheduler.advance_by(ADD_REMOVE_SCAN_COOLDOWN);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    owner.stop().unwrap();
    assert!(retained.stopped.load(Ordering::SeqCst));
    drop(retained);
    assert!(
        weak_watcher.upgrade().is_none(),
        "stop releases watcher ownership"
    );
    scheduler.advance_by(Duration::from_secs(30));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn only_inotify_unavailable_is_a_distinct_watcher_start_failure() {
    struct FailedWatcher(WatcherStartError);
    impl UsbHotplugWatcher for FailedWatcher {
        fn start(&self, _callback: Arc<dyn Fn() + Send + Sync>) -> Result<(), WatcherStartError> {
            Err(self.0.clone())
        }
        fn stop_and_join(&self) -> Result<(), UsbError> {
            Ok(())
        }
    }
    let provider = Arc::new(MutableProvider::new(Vec::new()));
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) = integration(provider, scheduler, Vec::new(), HostPlatform::Other);
    let inotify = manager.start_watcher(Arc::new(FailedWatcher(
        WatcherStartError::InotifyNotAvailable("unsupported fs".to_owned()),
    )));
    assert_eq!(
        inotify,
        Err(WatcherStartError::InotifyNotAvailable(
            "unsupported fs".to_owned()
        ))
    );
    let other = manager.start_watcher(Arc::new(FailedWatcher(WatcherStartError::Other(
        UsbError::new("permission denied"),
    ))));
    assert_eq!(
        other,
        Err(WatcherStartError::Other(UsbError::new("permission denied")))
    );
    assert_eq!(manager.request_scan().unwrap(), DebounceRequest::Ran);
    owner.stop().unwrap();
}

#[test]
fn watcher_join_error_is_returned_after_pending_work_is_canceled() {
    struct JoinErrorWatcher;
    impl UsbHotplugWatcher for JoinErrorWatcher {
        fn start(&self, _callback: Arc<dyn Fn() + Send + Sync>) -> Result<(), WatcherStartError> {
            Ok(())
        }

        fn stop_and_join(&self) -> Result<(), UsbError> {
            Err(UsbError::new("watcher worker failed"))
        }
    }
    let provider = Arc::new(MutableProvider::new(Vec::new()));
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) = integration(
        provider.clone(),
        scheduler.clone(),
        Vec::new(),
        HostPlatform::Other,
    );
    manager.start_watcher(Arc::new(JoinErrorWatcher)).unwrap();
    manager.notify_serial_ports_changed().unwrap();
    assert_eq!(owner.stop().unwrap_err().message(), "watcher worker failed");
    scheduler.advance_by(ADD_REMOVE_SCAN_COOLDOWN);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(manager.scan_now().is_err());
}

#[test]
fn failed_first_scan_does_not_complete_initial_callbacks() {
    let provider = Arc::new(MutableProvider::new(Vec::new()));
    provider.fail("provider offline");
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) =
        integration(provider.clone(), scheduler, Vec::new(), HostPlatform::Other);
    let called = Arc::new(AtomicUsize::new(0));
    let callback_count = Arc::clone(&called);
    let _initial_guard = manager
        .register_initial_scan_callback(move || {
            callback_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    let error = manager.scan_now().unwrap_err();
    assert_eq!(error.message(), "provider offline");
    assert!(!manager.initial_scan_done().unwrap());
    assert_eq!(called.load(Ordering::SeqCst), 0);

    provider.set_ports(Vec::new());
    let report = manager.scan_now().unwrap();
    assert!(report.initial_scan_completed);
    assert!(manager.initial_scan_done().unwrap());
    assert_eq!(called.load(Ordering::SeqCst), 1);
    owner.stop().unwrap();
}

#[test]
fn scan_request_and_initial_callback_order_follow_source_lifecycle() {
    let provider = Arc::new(MutableProvider::new(Vec::new()));
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) = integration(provider, scheduler, Vec::new(), HostPlatform::Other);
    let initial_order = Arc::new(Mutex::new(Vec::<u8>::new()));
    let older = Arc::clone(&initial_order);
    let _older_guard = manager
        .register_initial_scan_callback(move || {
            lock(&older).push(1);
            Ok(())
        })
        .unwrap();
    let newer = Arc::clone(&initial_order);
    let _newer_guard = manager
        .register_initial_scan_callback(move || {
            lock(&newer).push(2);
            Ok(())
        })
        .unwrap();
    let request_order = Arc::new(Mutex::new(Vec::<u8>::new()));
    let first_order = Arc::clone(&request_order);
    let _request_first = manager
        .register_scan_request_callback(move || {
            lock(&first_order).push(1);
            Ok(())
        })
        .unwrap();
    let second_order = Arc::clone(&request_order);
    let _request_second = manager
        .register_scan_request_callback(move || {
            lock(&second_order).push(2);
            Ok(())
        })
        .unwrap();
    assert_eq!(manager.request_scan().unwrap(), DebounceRequest::Ran);
    assert_eq!(*lock(&request_order), [1, 2]);
    assert_eq!(*lock(&initial_order), [2, 1]);
    let late_order = Arc::clone(&initial_order);
    let _late_guard = manager
        .register_initial_scan_callback(move || {
            lock(&late_order).push(3);
            Ok(())
        })
        .unwrap();
    assert_eq!(*lock(&initial_order), [2, 1, 3]);
    owner.stop().unwrap();
}

#[test]
fn initial_scan_callback_failure_stops_reverse_order_dispatch() {
    let provider = Arc::new(MutableProvider::new(Vec::new()));
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) = integration(provider, scheduler, Vec::new(), HostPlatform::Other);
    let order = Arc::new(Mutex::new(Vec::<u8>::new()));
    let older = Arc::clone(&order);
    let _older_guard = manager
        .register_initial_scan_callback(move || {
            lock(&older).push(1);
            Ok(())
        })
        .unwrap();
    let newer = Arc::clone(&order);
    let _newer_guard = manager
        .register_initial_scan_callback(move || {
            lock(&newer).push(2);
            Err(UsbError::new("initial callback failed"))
        })
        .unwrap();
    assert_eq!(
        manager.scan_now().unwrap_err().message(),
        "initial callback failed"
    );
    assert!(manager.initial_scan_done().unwrap());
    assert_eq!(*lock(&order), [2]);
    owner.stop().unwrap();
}

#[test]
fn macos_silabs_duplicate_is_removed_from_discovery_snapshot_only() {
    let mut broken_cp2102n = provider_port("/dev/cu.usbserial-0001", Some(0x10C4), Some(0xEA60));
    broken_cp2102n.serial_number = Some("CP2102N".to_owned());
    let mut working_cp2102n =
        provider_port("/dev/cu.SLAB_USBtoUART0001", Some(0x10C4), Some(0xEA60));
    working_cp2102n.serial_number = Some("CP2102N".to_owned());
    let provider = Arc::new(MutableProvider::new(vec![broken_cp2102n, working_cp2102n]));
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) = integration(provider, scheduler, Vec::new(), HostPlatform::MacOs);
    let report = manager.scan_now().unwrap();
    assert_eq!(report.serial_ports.ports.len(), 2);
    assert_eq!(report.usb_devices.len(), 1);
    assert_eq!(
        report.usb_devices.first().unwrap().device,
        "/dev/cu.SLAB_USBtoUART0001"
    );
    owner.stop().unwrap();
}

#[test]
fn stop_waits_for_in_progress_provider_work_and_blocks_stale_dispatch() {
    struct BlockingProvider {
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    }
    impl SerialPortProvider for BlockingProvider {
        fn list_serial_ports(&self) -> Result<Vec<SerialPortInfo>, UsbError> {
            self.entered.wait();
            self.release.wait();
            Ok(Vec::new())
        }
    }
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let provider: Arc<dyn SerialPortProvider> = Arc::new(BlockingProvider {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let scheduler = Arc::new(ManualScheduler::default());
    let owner = Arc::new(UsbIntegration::new(
        provider,
        scheduler,
        UsbOptions::new(vec![matcher("all", None, None, None, None, None)]),
        policy(),
    ));
    let manager = owner.manager();
    let scan_manager = manager.clone();
    let scan = thread::spawn(move || scan_manager.scan_now());
    entered.wait();
    let release_thread = {
        let release = Arc::clone(&release);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            release.wait();
        })
    };
    owner.stop().unwrap();
    let scan_result = scan.join().unwrap();
    release_thread.join().unwrap();
    assert!(
        scan_result.is_err(),
        "a scan finishing after stop is not dispatched"
    );
    assert!(manager.scan_now().is_err());
}

#[test]
fn provider_path_lookup_excludes_registered_scanner_contributions() {
    struct Identity;
    impl PathResolver for Identity {
        fn realpath(&self, path: &str) -> String {
            path.into()
        }
    }
    let provider = Arc::new(MutableProvider::new(vec![provider_port(
        "/dev/base",
        Some(0x1234),
        Some(0x5678),
    )]));
    let scheduler = Arc::new(ManualScheduler::default());
    let (_owner, manager) = integration(provider, scheduler, Vec::new(), HostPlatform::Other);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let _scanner = manager
        .register_serial_port_scanner(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(vec![PortDevice::Usb(device("/dev/scanner"))])
        })
        .unwrap();
    assert!(
        manager
            .usb_device_from_path("/dev/scanner", &Identity)
            .unwrap()
            .is_none()
    );
    assert!(
        manager
            .usb_device_from_path("/dev/base", &Identity)
            .unwrap()
            .is_some()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(manager.scan_serial_ports().unwrap().ports.len(), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!manager.initial_scan_done().unwrap());
}

#[test]
fn requested_scans_report_nonfatal_scanner_and_event_errors() {
    let provider = Arc::new(MutableProvider::new(vec![provider_port(
        "/dev/fixture",
        Some(0x1234),
        Some(0x5678),
    )]));
    let scheduler = Arc::new(ManualScheduler::default());
    let (_owner, manager) = integration(
        provider.clone(),
        scheduler.clone(),
        Vec::new(),
        HostPlatform::Other,
    );
    let _scanner = manager
        .register_serial_port_scanner(|| Err(UsbError::new("injected scanner error")))
        .unwrap();
    let _event = manager
        .register_port_event_callback(|_, _| Err(UsbError::new("injected event error")))
        .unwrap();
    let errors = Arc::new(Mutex::new(Vec::new()));
    let observed = errors.clone();
    let _error = manager
        .register_scan_error_callback(move |error| lock(&observed).push(error.message().to_owned()))
        .unwrap();
    assert_eq!(manager.request_scan().unwrap(), DebounceRequest::Ran);
    let first = manager.take_scan_errors().unwrap();
    assert_eq!(first.len(), 2);
    assert!(
        first
            .iter()
            .any(|error| error.message().contains("injected scanner error"))
    );
    assert!(
        first
            .iter()
            .any(|error| error.message().contains("injected event error"))
    );
    provider.set_ports(Vec::new());
    manager.notify_serial_ports_changed().unwrap();
    scheduler.advance_by(ADD_REMOVE_SCAN_COOLDOWN);
    assert_eq!(manager.take_scan_errors().unwrap().len(), 2);
    assert_eq!(lock(&errors).len(), 4);
}

#[test]
fn stop_waits_for_requested_callbacks_before_provider_phase() {
    let provider = Arc::new(MutableProvider::default());
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) =
        integration(provider.clone(), scheduler, Vec::new(), HostPlatform::Other);
    let owner = Arc::new(owner);
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let callback_entered = entered.clone();
    let callback_release = release.clone();
    let _callback = manager
        .register_scan_request_callback(move || {
            callback_entered.wait();
            callback_release.wait();
            Ok(())
        })
        .unwrap();
    let requested = manager.clone();
    let request = thread::spawn(move || requested.request_scan());
    entered.wait();
    let (done, receiver) = std::sync::mpsc::channel();
    let stopping = owner.clone();
    let stop = thread::spawn(move || {
        let result = stopping.stop();
        done.send(()).unwrap();
        result
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while manager.initial_scan_done().is_ok() && std::time::Instant::now() < deadline {
        thread::yield_now();
    }
    let admission_closed = manager.initial_scan_done().is_err();
    let completed_early = receiver.recv_timeout(Duration::from_millis(20)).is_ok();
    release.wait();
    stop.join().unwrap().unwrap();
    assert!(request.join().unwrap().is_err());
    assert!(!completed_early, "stop must join the callback phase");
    assert!(admission_closed);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn delayed_scan_shutdown_does_not_dispatch_error_callbacks_after_stop_begins() {
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let observed_entered = entered.clone();
    let observed_release = release.clone();
    let provider = Arc::new(move || {
        observed_entered.wait();
        observed_release.wait();
        Err(UsbError::new("injected provider error during stop"))
    });
    let scheduler = Arc::new(ManualScheduler::default());
    let (owner, manager) =
        integration(provider, scheduler.clone(), Vec::new(), HostPlatform::Other);
    let owner = Arc::new(owner);
    let errors = Arc::new(AtomicUsize::new(0));
    let observed = errors.clone();
    let _callback = manager
        .register_scan_error_callback(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
    manager.notify_serial_ports_changed().unwrap();
    let scan = thread::spawn(move || scheduler.advance_by(ADD_REMOVE_SCAN_COOLDOWN));
    entered.wait();
    let stopping = owner.clone();
    let stop = thread::spawn(move || stopping.stop());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while manager.initial_scan_done().is_ok() && std::time::Instant::now() < deadline {
        thread::yield_now();
    }
    let admission_closed = manager.initial_scan_done().is_err();
    release.wait();
    scan.join().unwrap();
    stop.join().unwrap().unwrap();
    assert!(admission_closed);
    assert_eq!(errors.load(Ordering::SeqCst), 0);
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
