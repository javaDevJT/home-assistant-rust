//! App-owned view of configured serial-port consumers and USB discovery flows.
//!
//! Config entry data and discovery-flow initialization data stay inside
//! `ConfigEntries`; this module projects only the fields the USB API needs.

use hass_api::{UsbDiscoveryFlowUsage, UsbUsageClaim, UsbUsageProvider, UsbUsageSnapshot};
use hass_config::{ConfigEntries, ConfigEntryState};
use hass_usb::PathResolver;
use hass_usb::config_entry_uses_serial_port;
use hass_usb_backend::NativePathResolver;
use serde_json::{Map, Value};

const USB_DISCOVERY_SOURCE: &str = "usb";

/// Literal config-entry paths that Home Assistant reads for serial consumers.
///
/// Keep the key-path order and data-before-options traversal aligned with the
/// pinned `homeassistant.components.usb.consumers` implementation. These are
/// intentionally not recursive searches through arbitrary entry data.
const SERIAL_PORT_KEY_PATHS: &[&[&str]] = &[
    &["device"],
    &["device", "path"],
    &["device_path"],
    &["filename"],
    &["host"],
    &["port"],
    &["serial_port"],
    &["socket_path"],
    &["usb_path"],
];

/// USB usage provider backed only by the app's actual config-entry store.
#[derive(Clone)]
pub struct UsbUsage {
    config_entries: ConfigEntries,
}

impl UsbUsage {
    /// Use the live app-owned config-entry manager as the usage source.
    pub fn new(config_entries: ConfigEntries) -> Self {
        Self { config_entries }
    }
}

impl UsbUsageProvider for UsbUsage {
    fn snapshot(&self) -> UsbUsageSnapshot {
        let (entries, flows) = self
            .config_entries
            .snapshot_with_discovery_flows(USB_DISCOVERY_SOURCE);

        let mut consumers = Vec::new();
        for entry in entries {
            // The upstream async_entries(include_ignore=false) omits ignored
            // entries. The generated SDK list is the authoritative pinned set
            // of config-entry domains; USB matcher targets are unrelated.
            if entry.source == "ignore" || !config_entry_uses_serial_port(&entry.domain) {
                continue;
            }

            let active = config_entry_is_active(entry.state);
            for key_path in SERIAL_PORT_KEY_PATHS {
                // Upstream's loop is key-path first, then entry.data and
                // entry.options. Preserve that order in the snapshot.
                for values in [&entry.data, &entry.options] {
                    let Some(path) =
                        resolve_literal_key_path(values, key_path).and_then(Value::as_str)
                    else {
                        continue;
                    };
                    consumers.push(UsbUsageClaim {
                        path: path.to_owned(),
                        consumer: hass_usb::SerialPortConsumer {
                            kind: hass_usb::SerialPortConsumerKind::ConfigEntry,
                            title: entry.title.clone(),
                            active,
                            domain: Some(entry.domain.clone()),
                            config_entry_id: Some(entry.entry_id.clone()),
                            slug: None,
                        },
                    });
                }
            }
        }

        // Only the typed USB-source snapshot is accepted. Do not forward the
        // original flow map or its other values through this public DTO.
        let discovery_flows = flows
            .into_iter()
            .filter_map(|flow| {
                if flow.data_kind != Some(hass_config::DiscoveryDataKind::Usb) {
                    return None;
                }
                let device = flow.initial_data.get("device")?.as_str()?;
                Some(UsbDiscoveryFlowUsage {
                    device: device.to_owned(),
                    flow_id: flow.flow_id,
                    domain: flow.domain,
                })
            })
            .collect();

        UsbUsageSnapshot {
            consumers,
            discovery_flows,
        }
    }

    fn realpath(&self, path: &str) -> String {
        NativePathResolver.realpath(path)
    }
}

fn resolve_literal_key_path<'a>(
    values: &'a Map<String, Value>,
    path: &[&str],
) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let mut value = values.get(*first)?;
    for key in rest {
        value = value.as_object()?.get(*key)?;
    }
    Some(value)
}

fn config_entry_is_active(state: ConfigEntryState) -> bool {
    matches!(
        state,
        ConfigEntryState::Loaded
            | ConfigEntryState::SetupRetry
            | ConfigEntryState::SetupInProgress
            | ConfigEntryState::UnloadInProgress
            | ConfigEntryState::FailedUnload
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use hass_config::{
        ConfigEntryDriver, ConfigEntryState, ConfigFlowRequest, ConfigFlowResult,
        ConfigSubentryFlowRequest, ConfigSubentryFlowResult, ConfigSubentryTypeInfo,
        NewConfigEntry, SetupOutcome,
    };
    use hass_core::Runtime;
    use hass_storage::SqliteStore;
    use serde_json::{Map, Value, json};

    use super::UsbUsage;
    use hass_api::UsbUsageProvider;
    use hass_config::ConfigEntries;

    fn config_entries() -> (tempfile::TempDir, ConfigEntries) {
        let directory = tempfile::tempdir().expect("temporary runtime directory");
        let store = Arc::new(
            SqliteStore::open(directory.path().join("config.sqlite")).expect("SQLite store"),
        );
        let runtime = Runtime::new(store).expect("runtime");
        (
            directory,
            ConfigEntries::new(runtime).expect("config entries"),
        )
    }

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().expect("object value").clone()
    }

    fn add_entry(
        entries: &ConfigEntries,
        domain: &str,
        title: &str,
        source: &str,
        data: Value,
        options: Value,
    ) -> hass_config::ConfigEntry {
        let mut seed = NewConfigEntry::new(domain, title, map(data));
        seed.source = source.into();
        seed.options = map(options);
        entries.create_entry(seed).expect("create config entry")
    }

    #[test]
    fn snapshot_extracts_only_literal_paths_for_eligible_nonignored_entries() {
        let (_runtime_dir, entries) = config_entries();
        let entry = add_entry(
            &entries,
            "zha",
            "ZHA radio",
            "user",
            json!({
                "device": {"path": "/dev/data-nested", "other": "/dev/ignored-nested"},
                "device_path": "/dev/data-device-path",
                "filename": "/dev/data-filename",
                "host": "/dev/data-host",
                "port": "/dev/data-port",
                "serial_port": "/dev/data-serial-port",
                "socket_path": "socket://fake.invalid/data-uart",
                "usb_path": "/dev/data-usb-path",
                "unrelated": "/dev/ignored-unrelated"
            }),
            json!({
                "device": "/dev/options-device",
                "device_path": "/dev/options-device-path",
                "filename": "/dev/options-filename",
                "host": "/dev/options-host",
                "port": "/dev/options-port",
                "serial_port": "/dev/options-serial-port",
                "socket_path": "socket://fake.invalid/options-uart",
                "usb_path": "/dev/options-usb-path",
                "unrelated": "/dev/ignored-option"
            }),
        );
        add_entry(
            &entries,
            "zha",
            "Ignored ZHA",
            "ignore",
            json!({"device": "/dev/ignored-entry"}),
            json!({}),
        );
        add_entry(
            &entries,
            "not_a_serial_integration",
            "Unknown integration",
            "user",
            json!({"device": "/dev/unknown-domain"}),
            json!({}),
        );
        add_entry(
            &entries,
            "mysensors",
            "Known exception",
            "user",
            json!({"device": "/dev/mysensors"}),
            json!({}),
        );

        let snapshot = UsbUsage::new(entries).snapshot();
        let actual: Vec<_> = snapshot
            .consumers
            .iter()
            .filter(|claim| claim.consumer.domain.as_deref() == Some("zha"))
            .map(|claim| claim.path.as_str())
            .collect();
        assert_eq!(
            actual,
            [
                "/dev/options-device",
                "/dev/data-nested",
                "/dev/data-device-path",
                "/dev/options-device-path",
                "/dev/data-filename",
                "/dev/options-filename",
                "/dev/data-host",
                "/dev/options-host",
                "/dev/data-port",
                "/dev/options-port",
                "/dev/data-serial-port",
                "/dev/options-serial-port",
                "socket://fake.invalid/data-uart",
                "socket://fake.invalid/options-uart",
                "/dev/data-usb-path",
                "/dev/options-usb-path",
            ]
        );
        assert!(
            snapshot
                .consumers
                .iter()
                .any(|claim| claim.path == "/dev/mysensors")
        );

        let zha_claim = snapshot
            .consumers
            .iter()
            .find(|claim| claim.path == "/dev/options-device")
            .expect("literal device path claim");
        assert_eq!(
            zha_claim.consumer,
            hass_usb::SerialPortConsumer {
                kind: hass_usb::SerialPortConsumerKind::ConfigEntry,
                title: "ZHA radio".into(),
                active: false,
                domain: Some("zha".into()),
                config_entry_id: Some(entry.entry_id),
                slug: None,
            }
        );
        assert!(snapshot.discovery_flows.is_empty());
    }

    #[derive(Default)]
    struct FlowDriver;

    impl ConfigEntryDriver for FlowDriver {
        fn domain(&self) -> &str {
            "zha"
        }

        fn setup(
            &self,
            _runtime: &Runtime,
            entry: &hass_config::ConfigEntry,
        ) -> hass_core::Result<SetupOutcome> {
            if entry.title == "Retrying radio" {
                Ok(SetupOutcome::RetryAfter {
                    seconds: 30,
                    reason: Some("fixture retry".into()),
                })
            } else {
                Ok(SetupOutcome::Ready)
            }
        }

        fn unload(
            &self,
            _runtime: &Runtime,
            _entry: &hass_config::ConfigEntry,
        ) -> hass_core::Result<()> {
            Ok(())
        }

        fn config_flow(&self, _request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
            Ok(ConfigFlowResult::form("confirm", None))
        }

        fn supports_options(&self) -> bool {
            true
        }

        fn options_flow(
            &self,
            _request: &ConfigFlowRequest,
        ) -> hass_core::Result<ConfigFlowResult> {
            Ok(ConfigFlowResult::form("options", None))
        }

        fn supported_subentry_types(
            &self,
            _entry: &hass_config::ConfigEntry,
        ) -> BTreeMap<String, ConfigSubentryTypeInfo> {
            BTreeMap::from([("radio".into(), ConfigSubentryTypeInfo::default())])
        }

        fn subentry_flow(
            &self,
            _request: &ConfigSubentryFlowRequest,
        ) -> hass_core::Result<ConfigSubentryFlowResult> {
            Ok(ConfigSubentryFlowResult::Form {
                step_id: "radio".into(),
                data_schema: None,
                errors: None,
                description_placeholders: None,
            })
        }
    }

    #[test]
    fn snapshot_projects_only_usb_source_flow_device_fields() {
        let (_runtime_dir, entries) = config_entries();
        entries
            .register_driver(Arc::new(FlowDriver))
            .expect("register flow driver");
        let entry = add_entry(
            &entries,
            "zha",
            "ZHA radio",
            "user",
            json!({"device": "/dev/radio0"}),
            json!({}),
        );

        let mut usb_context = Map::new();
        usb_context.insert("source_device_hint".into(), json!("not projected"));
        let mut usb_data = Map::new();
        usb_data.insert("device".into(), json!("/dev/usb-radio"));
        usb_data.insert("serial_number".into(), json!("private flow field"));
        usb_data.insert("token".into(), json!("fake-flow-token"));
        let usb_flow = entries
            .init_config_flow_with_typed_input(
                "zha",
                "usb",
                usb_context,
                Some(usb_data),
                Some(hass_config::DiscoveryDataKind::Usb),
            )
            .expect("start USB discovery flow");
        entries
            .submit_flow(
                usb_flow["flow_id"].as_str().unwrap(),
                map(json!({"device":"/dev/current-form-step"})),
            )
            .unwrap();

        entries
            .init_config_flow_with_input(
                "zha",
                "usb",
                Map::new(),
                Some(map(
                    json!({"device":"/dev/untyped-usb-map", "data_kind":"Usb"}),
                )),
            )
            .unwrap();

        let mut user_data = Map::new();
        user_data.insert("device".into(), json!("/dev/user-flow"));
        entries
            .init_config_flow_with_input("zha", "user", Map::new(), Some(user_data))
            .expect("start user flow");

        let mut options_data = Map::new();
        options_data.insert("device".into(), json!("/dev/options-flow"));
        entries
            .init_options_flow_with_input(&entry.entry_id, Some(options_data))
            .expect("start options flow");

        let mut subentry_data = Map::new();
        subentry_data.insert("device".into(), json!("/dev/subentry-flow"));
        entries
            .init_subentry_flow(
                &entry.entry_id,
                "radio",
                "user",
                None,
                Map::new(),
                Some(subentry_data),
            )
            .expect("start subentry flow");

        let snapshot = UsbUsage::new(entries).snapshot();
        assert_eq!(snapshot.discovery_flows.len(), 1);
        let flow = &snapshot.discovery_flows[0];
        assert_eq!(flow.device, "/dev/usb-radio");
        assert_eq!(flow.domain, "zha");
        assert!(!flow.flow_id.is_empty());
        let debug = format!("{snapshot:?}");
        assert!(!debug.contains("private flow field"));
        assert!(!debug.contains("fake-flow-token"));
        assert!(!debug.contains("source_device_hint"));
    }

    #[test]
    fn reports_loaded_and_retrying_entries_as_active() {
        let (_runtime_dir, entries) = config_entries();
        let driver = Arc::new(FlowDriver);
        entries
            .register_driver(driver)
            .expect("register setup driver");
        let loaded = add_entry(
            &entries,
            "zha",
            "Loaded radio",
            "user",
            json!({"device": "/dev/loaded"}),
            json!({}),
        );
        let retrying = add_entry(
            &entries,
            "zha",
            "Retrying radio",
            "user",
            json!({"device": "/dev/retrying"}),
            json!({}),
        );
        add_entry(
            &entries,
            "zha",
            "Not loaded radio",
            "user",
            json!({"device": "/dev/not-loaded"}),
            json!({}),
        );

        assert_eq!(
            entries.setup_entry(&loaded.entry_id).unwrap().state,
            ConfigEntryState::Loaded
        );
        assert_eq!(
            entries.setup_entry(&retrying.entry_id).unwrap().state,
            ConfigEntryState::SetupRetry
        );

        let snapshot = UsbUsage::new(entries).snapshot();
        let active_by_path: BTreeMap<_, _> = snapshot
            .consumers
            .iter()
            .map(|claim| (claim.path.as_str(), claim.consumer.active))
            .collect();
        assert!(active_by_path["/dev/loaded"]);
        assert!(active_by_path["/dev/retrying"]);
        assert!(!active_by_path["/dev/not-loaded"]);
    }

    #[cfg(unix)]
    #[test]
    fn realpath_resolves_owned_symlinks_and_normalizes_missing_paths() {
        use std::os::unix::fs::symlink;

        let (_runtime_dir, entries) = config_entries();
        let usage = UsbUsage::new(entries);
        let temp = tempfile::tempdir().expect("temporary path tree");
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).expect("create real directory");
        let endpoint = real.join("tty-test");
        std::fs::write(&endpoint, b"owned fixture").expect("create fixture endpoint");
        let alias = temp.path().join("by-id");
        symlink(&endpoint, &alias).expect("create owned symlink");

        assert_eq!(
            usage.realpath(alias.to_str().unwrap()),
            endpoint
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        );

        let missing = temp
            .path()
            .join("missing-parent")
            .join("..")
            .join("missing");
        let canonical_root = temp.path().canonicalize().unwrap();
        assert_eq!(
            usage.realpath(missing.to_str().unwrap()),
            canonical_root
                .join("missing")
                .to_string_lossy()
                .into_owned()
        );
    }

    #[test]
    fn active_state_list_is_the_source_compatible_set() {
        use super::config_entry_is_active;

        assert!(config_entry_is_active(ConfigEntryState::Loaded));
        assert!(config_entry_is_active(ConfigEntryState::SetupRetry));
        assert!(config_entry_is_active(ConfigEntryState::SetupInProgress));
        assert!(config_entry_is_active(ConfigEntryState::UnloadInProgress));
        assert!(config_entry_is_active(ConfigEntryState::FailedUnload));
        assert!(!config_entry_is_active(ConfigEntryState::NotLoaded));
        assert!(!config_entry_is_active(ConfigEntryState::SetupError));
        assert!(!config_entry_is_active(ConfigEntryState::MigrationError));
    }
}
