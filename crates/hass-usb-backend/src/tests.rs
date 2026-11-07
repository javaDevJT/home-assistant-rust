use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[cfg(target_os = "linux")]
use std::{
    sync::{atomic::AtomicBool, mpsc},
    time::Duration,
};

use crate::{
    DarwinSerialPortRecord, NativePathResolver, SOURCE_WATCH_EVENT_MASK, UdevPortIdentity,
    map_darwin_serial_records, resolve_serial_path, serial_path_udev_id, udev_serial_by_id_stem,
    watcher_event_requires_notification,
};
use hass_usb::PathResolver;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TempTree(PathBuf);

impl TempTree {
    fn new() -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hass-usb-backend-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create owned temporary fixture directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn maps_injected_darwin_iokit_records_without_host_queries() {
    let records = map_darwin_serial_records([
        DarwinSerialPortRecord {
            device: "/dev/cu.usbserial-ABC".to_owned(),
            vid: Some(0x10c4),
            pid: Some(0xea60),
            serial_number: Some("ABC".to_owned()),
            manufacturer: Some("Silicon Labs".to_owned()),
            product: Some("USB UART".to_owned()),
            bcd_device: Some(0x0101),
            interface_description: Some("CP210x UART".to_owned()),
            interface_num: Some(0),
        },
        DarwinSerialPortRecord {
            device: "/dev/tty.serial-console".to_owned(),
            ..DarwinSerialPortRecord::default()
        },
    ]);

    assert_eq!(records.len(), 2);
    let usb = &records[0];
    assert_eq!(usb.device, "/dev/cu.usbserial-ABC");
    assert_eq!(usb.resolved_device.as_deref(), Some(usb.device.as_str()));
    assert_eq!((usb.vid, usb.pid), (Some(0x10c4), Some(0xea60)));
    assert_eq!(usb.serial_number.as_deref(), Some("ABC"));
    assert_eq!(usb.manufacturer.as_deref(), Some("Silicon Labs"));
    assert_eq!(usb.description.as_deref(), Some("USB UART - CP210x UART"));
    assert_eq!(usb.bcd_device, Some(0x0101));
    assert_eq!(usb.interface_num, Some(0));

    let serial = &records[1];
    assert_eq!(serial.device, "/dev/tty.serial-console");
    assert_eq!(
        serial.resolved_device.as_deref(),
        Some(serial.device.as_str())
    );
    assert_eq!(serial.vid, None);
    assert_eq!(serial.pid, None);
    assert_eq!(serial.description.as_deref(), Some("tty.serial-console"));
}

#[cfg(unix)]
#[test]
fn linux_fixture_enumeration_keeps_by_id_and_resolved_paths_and_native_ports() {
    use std::os::unix::fs::symlink;

    let temp = TempTree::new();
    let sys = temp.path().join("sys");
    let dev_root = temp.path().join("dev");
    fs::create_dir_all(&dev_root).unwrap();
    let dev = fs::canonicalize(dev_root).unwrap();

    let usb_serial = sys.join("devices/pci/usb1/1-1/1-1:1.0/ttyUSB0");
    let usb_interface = usb_serial.parent().unwrap();
    let usb_device = usb_interface.parent().unwrap();
    write(&usb_device.join("idVendor"), "10c4\n");
    write(&usb_device.join("idProduct"), "8a2a\n");
    write(&usb_device.join("bcdDevice"), "0100\n");
    write(&usb_device.join("serial"), "51600403\n");
    write(&usb_device.join("manufacturer"), "Silicon Labs\n");
    write(&usb_device.join("product"), "HubZ Smart Home Controller\n");
    write(&usb_interface.join("bInterfaceNumber"), "00\n");
    write(&usb_interface.join("interface"), "HubZ Z-Wave Com Port\n");
    write(&usb_serial.join("driver"), "");
    write(&usb_serial.join("port_number"), "0\n");
    symlink(sys.join("bus/usb-serial"), usb_serial.join("subsystem")).unwrap();
    fs::create_dir_all(sys.join("bus/usb-serial")).unwrap();

    let cdc_interface = sys.join("devices/pci/usb2/2-1/2-1:1.0");
    let cdc_device = cdc_interface.parent().unwrap();
    write(&cdc_device.join("idVendor"), "303a\n");
    write(&cdc_device.join("idProduct"), "4001\n");
    write(&cdc_device.join("bcdDevice"), "0101\n");
    write(&cdc_device.join("serial"), "A1B2C3\n");
    write(&cdc_device.join("manufacturer"), "Maker\n");
    write(&cdc_device.join("product"), "CDC Port\n");
    write(&cdc_interface.join("bInterfaceNumber"), "01\n");
    write(&cdc_interface.join("interface"), "CDC Interface\n");
    write(&cdc_interface.join("driver"), "");
    symlink(sys.join("bus/usb"), cdc_interface.join("subsystem")).unwrap();
    fs::create_dir_all(sys.join("bus/usb")).unwrap();

    let native_port = sys.join("devices/platform/serial8250/tty/ttyS0");
    write(&native_port.join("driver"), "");
    symlink(sys.join("bus/platform"), native_port.join("subsystem")).unwrap();
    fs::create_dir_all(sys.join("bus/platform")).unwrap();

    write(&dev.join("ttyUSB0"), "fixture device node placeholder");
    write(&dev.join("ttyACM0"), "fixture device node placeholder");
    fs::create_dir_all(sys.join("class/tty/ttyUSB0")).unwrap();
    fs::create_dir_all(sys.join("class/tty/ttyACM0")).unwrap();
    fs::create_dir_all(sys.join("class/tty/ttyS0")).unwrap();
    symlink(&usb_serial, sys.join("class/tty/ttyUSB0/device")).unwrap();
    symlink(&cdc_interface, sys.join("class/tty/ttyACM0/device")).unwrap();
    symlink(&native_port, sys.join("class/tty/ttyS0/device")).unwrap();
    write(&sys.join("class/tty/ttyS0/type"), "1\n");

    let by_id = dev.join("serial/by-id");
    fs::create_dir_all(&by_id).unwrap();
    symlink(
        dev.join("ttyUSB0"),
        by_id.join("usb-Silicon_Labs_HubZ_51600403-if00-port0"),
    )
    .unwrap();

    let records = crate::enumerate_serial_ports_at(&sys, &dev).unwrap();
    assert_eq!(records.len(), 3);
    let usb = records
        .iter()
        .find(|port| port.resolved_device.as_deref() == Some(dev.join("ttyUSB0").to_str().unwrap()))
        .unwrap();
    assert_eq!(
        usb.device,
        by_id
            .join("usb-Silicon_Labs_HubZ_51600403-if00-port0")
            .to_string_lossy()
    );
    assert_eq!((usb.vid, usb.pid), (Some(0x10c4), Some(0x8a2a)));
    assert_eq!(usb.bcd_device, Some(0x0100));
    assert_eq!(usb.interface_num, Some(0));
    assert_eq!(usb.serial_number.as_deref(), Some("51600403"));
    assert_eq!(
        usb.description.as_deref(),
        Some("HubZ Smart Home Controller - HubZ Z-Wave Com Port")
    );

    let cdc = records
        .iter()
        .find(|port| port.resolved_device.as_deref() == Some(dev.join("ttyACM0").to_str().unwrap()))
        .unwrap();
    assert_eq!(cdc.device, dev.join("ttyACM0").to_string_lossy());
    assert_eq!((cdc.vid, cdc.pid), (Some(0x303a), Some(0x4001)));
    assert_eq!(cdc.description.as_deref(), Some("CDC Port - CDC Interface"));

    let native = records
        .iter()
        .find(|port| port.resolved_device.as_deref() == Some(dev.join("ttyS0").to_str().unwrap()))
        .unwrap();
    assert_eq!(native.device, dev.join("ttyS0").to_string_lossy());
    assert_eq!((native.vid, native.pid), (None, None));
    assert_eq!(native.description.as_deref(), Some("ttyS0"));
    assert_eq!(native.serial_number, None);

    // A node can disappear before its sysfs attributes and by-id alias. The
    // provider must preserve the alias spelling through that removal race.
    fs::remove_file(dev.join("ttyUSB0")).unwrap();
    let stale = crate::enumerate_serial_ports_at(&sys, &dev).unwrap();
    let stale_usb = stale.iter().find(|port| port.vid == Some(0x10c4)).unwrap();
    assert_eq!(stale_usb.device, usb.device);
    assert_eq!(stale_usb.resolved_device, usb.resolved_device);
}

#[cfg(unix)]
#[test]
fn linux_fixture_enumeration_skips_incomplete_devices_and_tolerates_absent_roots() {
    use std::os::unix::fs::symlink;

    let temp = TempTree::new();
    let sys = temp.path().join("sys");
    let dev = temp.path().join("dev");
    let disappeared = sys.join("devices/usb/3-1/3-1:1.0/ttyACM9");
    write(&disappeared.join("driver"), "");
    symlink(sys.join("bus/usb"), disappeared.join("subsystem")).unwrap();
    fs::create_dir_all(sys.join("bus/usb")).unwrap();
    fs::create_dir_all(sys.join("class/tty/ttyACM9")).unwrap();
    symlink(&disappeared, sys.join("class/tty/ttyACM9/device")).unwrap();

    assert!(
        crate::enumerate_serial_ports_at(&sys, &dev)
            .unwrap()
            .is_empty()
    );
    assert!(
        crate::enumerate_serial_ports_at(&temp.path().join("missing-sys"), &dev)
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn path_resolution_and_udev_id_helpers_keep_alias_identity() {
    use std::os::unix::fs::symlink;

    let temp = TempTree::new();
    let physical = temp.path().join("dev/ttyUSB7");
    write(&physical, "owned fixture only");
    let alias = temp.path().join("dev/serial/by-id/usb-fixture");
    fs::create_dir_all(alias.parent().unwrap()).unwrap();
    symlink(&physical, &alias).unwrap();
    assert_eq!(
        resolve_serial_path(&alias),
        fs::canonicalize(&physical).unwrap()
    );
    assert_eq!(
        NativePathResolver.realpath(alias.to_str().unwrap()),
        fs::canonicalize(&physical).unwrap().to_string_lossy()
    );
    assert_eq!(
        serial_path_udev_id("/dev/serial/by-id/usb-fixture").as_deref(),
        Some("usb-fixture")
    );
    assert_eq!(
        serial_path_udev_id("socket://host:9000?port_udev_id=&port_udev_id=usb%2Dfoo%2Bbar"),
        Some("usb-foo+bar".to_owned())
    );
}

#[cfg(unix)]
#[test]
fn non_strict_realpath_follows_dangling_links_and_missing_tails() {
    use std::os::unix::fs::symlink;

    let temp = TempTree::new();
    let dev = temp.path().join("dev");
    fs::create_dir_all(dev.join("serial/by-id")).unwrap();
    let dev = fs::canonicalize(dev).unwrap();
    let alias = dev.join("serial/by-id/usb-missing");
    symlink("../../ttyUSB7", &alias).unwrap();
    assert_eq!(resolve_serial_path(&alias), dev.join("ttyUSB7"));
    let directory_alias = dev.join("serial-tree");
    symlink("serial", &directory_alias).unwrap();
    assert_eq!(
        resolve_serial_path(directory_alias.join("by-id/usb-missing")),
        dev.join("ttyUSB7")
    );
    assert_eq!(
        resolve_serial_path(directory_alias.join("absent/../missing/leaf")),
        dev.join("serial/missing/leaf")
    );
    let loop_link = dev.join("cycle");
    symlink("cycle", &loop_link).unwrap();
    assert_eq!(resolve_serial_path(&loop_link), loop_link);
}

#[test]
fn udev_stem_matches_source_fields_and_port_suffix() {
    let stem = udev_serial_by_id_stem(&UdevPortIdentity {
        vid: Some(0x303a),
        pid: Some(0x4001),
        serial_number: Some("10B41DE589FC\n"),
        manufacturer: Some("Nabu Casa"),
        product: Some("ZBT-2"),
        interface_num: Some(0),
        port_number: None,
    })
    .unwrap();
    assert_eq!(stem, "usb-Nabu_Casa_ZBT-2_10B41DE589FC-if00");

    let stem = udev_serial_by_id_stem(&UdevPortIdentity {
        vid: Some(0x10c4),
        pid: Some(0x8a2a),
        serial_number: Some("serial-1"),
        manufacturer: Some("  Example\tMaker  "),
        product: Some("USB Device"),
        interface_num: Some(1),
        port_number: Some(2),
    })
    .unwrap();
    assert_eq!(stem, "usb-Example_Maker_USB_Device_serial-1-if01-port2");
}

#[test]
fn watcher_change_mask_matches_the_pinned_event_set() {
    let bits = [0x100, 0x40, 0x80, 0x400, 0x200, 0x8000];
    assert_eq!(
        bits.iter().fold(0, |all, bit| all | bit),
        SOURCE_WATCH_EVENT_MASK
    );
    assert!(
        bits.iter()
            .all(|mask| watcher_event_requires_notification(*mask))
    );
    assert!(!watcher_event_requires_notification(0x0000_0002)); // MODIFY
    assert!(!watcher_event_requires_notification(0x0000_0020)); // OPEN
}

#[cfg(not(target_os = "linux"))]
#[test]
fn watcher_reports_only_the_source_inotify_unavailable_case() {
    let watcher = crate::NativeSerialPortWatcher::new();
    let result = hass_usb::UsbHotplugWatcher::start(&watcher, Arc::new(|| {}));
    assert!(matches!(
        result,
        Err(hass_usb::WatcherStartError::InotifyNotAvailable(message))
            if message == "inotify unavailable"
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn watcher_observes_owned_fixture_tree_and_joins_on_stop() {
    use std::sync::mpsc::RecvTimeoutError;

    let temp = TempTree::new();
    let root = temp.path().join("usb-tree");
    fs::create_dir_all(&root).unwrap();
    let (send, receive) = mpsc::channel();
    let watcher = crate::NativeSerialPortWatcher::with_root(&root);
    hass_usb::UsbHotplugWatcher::start(
        &watcher,
        Arc::new(move || {
            let _ = send.send(());
        }),
    )
    .unwrap();

    let nested = root.join("001");
    fs::create_dir(&nested).unwrap();
    receive.recv_timeout(Duration::from_secs(2)).unwrap();
    write(&nested.join("002"), "owned watcher fixture");
    receive.recv_timeout(Duration::from_secs(2)).unwrap();
    hass_usb::UsbHotplugWatcher::stop_and_join(&watcher).unwrap();
    assert!(matches!(
        receive.recv_timeout(Duration::from_millis(20)),
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected)
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn concurrent_watcher_stops_both_wait_for_an_admitted_callback() {
    let temp = TempTree::new();
    let root = temp.path().join("usb-tree");
    fs::create_dir_all(&root).unwrap();
    let (entered, entered_receiver) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let blocked = std::sync::Mutex::new(blocked);
    let watcher = Arc::new(crate::NativeSerialPortWatcher::with_root(&root));
    watcher
        .start_watching(Arc::new(move || {
            entered.send(()).unwrap();
            blocked.lock().unwrap().recv().unwrap();
        }))
        .unwrap();
    fs::create_dir(root.join("001")).unwrap();
    entered_receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let (done, completed) = mpsc::channel();
    let stops = (0..2)
        .map(|_| {
            let watcher = watcher.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let result = watcher.stop_and_join();
                done.send(()).unwrap();
                result
            })
        })
        .collect::<Vec<_>>();
    let completed_early = completed.recv_timeout(Duration::from_millis(20)).is_ok();
    release.send(()).unwrap();
    for stop in stops {
        stop.join().unwrap().unwrap();
    }
    assert!(!completed_early, "every stop must wait for the old worker");
}

#[cfg(target_os = "linux")]
#[test]
fn watcher_drop_cancels_and_joins_owned_worker() {
    struct DropNotice(Arc<AtomicBool>);
    impl Drop for DropNotice {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let temp = TempTree::new();
    let root = temp.path().join("usb-tree");
    fs::create_dir_all(&root).unwrap();
    let callback_dropped = Arc::new(AtomicBool::new(false));
    let notice = DropNotice(Arc::clone(&callback_dropped));
    let callback = Arc::new(move || {
        let _keep_alive = &notice;
    });
    let watcher = crate::NativeSerialPortWatcher::with_root(&root);
    hass_usb::UsbHotplugWatcher::start(&watcher, callback).unwrap();
    drop(watcher);
    assert!(callback_dropped.load(Ordering::Acquire));
}

fn write(path: &Path, value: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, value).unwrap();
}
