//! Native tracing target catalog and authenticated logger ownership.
use hass_auth::AuthManager;
use hass_core::LoadedComponents;
use hass_core::{Error, Result, Runtime};
use hass_logger::{
    IntegrationCatalog, IntegrationCatalogProvider, LoggerConfig, LoggerFilter, LoggerIntegration,
    LoggerManager,
};
use std::{collections::BTreeSet, sync::Arc};

pub fn catalog() -> IntegrationCatalog {
    const TARGETS: &[(&str, &[&str])] = &[
        ("api", &["hass_api"]),
        ("auth", &["hass_auth"]),
        ("binary_sensor", &["hass_binary_sensor"]),
        ("button", &["hass_button"]),
        ("automation", &["hass_automation"]),
        ("climate", &["hass_climate"]),
        ("config", &["hass_config"]),
        ("cover", &["hass_cover"]),
        ("counter", &["hass::helpers::counter"]),
        ("device_tracker", &["hass_device_tracker"]),
        ("fan", &["hass_fan"]),
        ("group", &["hass_group"]),
        ("homeassistant", &["hass::homeassistant"]),
        ("http", &["hass_api::http"]),
        ("image_upload", &["hass_image_upload", "hass_api::images"]),
        ("input_boolean", &["hass::helpers::input_boolean"]),
        ("input_button", &["hass_native::input_button"]),
        ("input_datetime", &["hass_native::input_datetime"]),
        ("input_number", &["hass::helpers::input_number"]),
        ("input_select", &["hass::helpers::input_select"]),
        ("input_text", &["hass::helpers::input_text"]),
        ("logger", &["hass_logger"]),
        ("light", &["hass_light"]),
        ("media_player", &["hass_media_player", "hass::media_player"]),
        ("mqtt", &["hass_mqtt"]),
        ("network", &["hass_network", "hass_api::network_adapters"]),
        ("persistent_notification", &["hass_notifications"]),
        ("person", &["hass_person", "hass::persons"]),
        ("recorder", &["hass::recorder", "hass_storage"]),
        (
            "repairs",
            &["hass_repairs", "hass_api::repairs", "hass::repairs"],
        ),
        ("rest", &["hass_rest"]),
        ("rest_command", &["hass_rest::command"]),
        ("scene", &["hass_script::scene"]),
        ("schedule", &["hass_native::schedule"]),
        ("script", &["hass_script"]),
        ("sensor", &["hass_sensor"]),
        ("spotify", &["hass_spotify", "hass::spotify"]),
        ("tag", &["hass_tag"]),
        ("sun", &["hass_sun"]),
        (
            "ssdp",
            &[
                "hass_ssdp",
                "hass_api::ssdp",
                "hass::ssdp",
                "hass::discovery",
            ],
        ),
        (
            "system_health",
            &[
                "hass_system_health",
                "hass_api::system_health",
                "hass::system_health",
            ],
        ),
        ("system_log", &["hass_system_log", "hass_api::system_log"]),
        ("switch", &["hass_switch"]),
        ("template", &["hass_template_entities", "hass_template"]),
        (
            "time_date",
            &[
                "hass_system_sensors::time_date",
                "hass::system_sensors::time_date",
            ],
        ),
        ("timer", &["hass_native::timer"]),
        (
            "uptime",
            &[
                "hass_system_sensors::uptime",
                "hass::system_sensors::uptime",
            ],
        ),
        ("webhook", &["hass_webhook", "hass_api::webhooks"]),
        ("websocket_api", &["hass_api::websocket"]),
        ("zone", &["hass_zone", "hass::zones"]),
    ];
    TARGETS
        .iter()
        .map(|(domain, targets)| {
            (
                (*domain).to_owned(),
                targets.iter().map(|target| (*target).to_owned()).collect(),
            )
        })
        .collect()
}

pub fn loaded_catalog(loaded_components: LoadedComponents) -> IntegrationCatalogProvider {
    Arc::new(move || {
        let domains: BTreeSet<String> = loaded_components.all_components().into_iter().collect();
        catalog()
            .into_iter()
            .filter(|(domain, _)| domains.contains(domain))
            .collect()
    })
}

pub struct Logging {
    integration: LoggerIntegration,
    _auth: Arc<AuthManager>,
}

impl Logging {
    pub fn manager(&self) -> LoggerManager {
        self.integration.manager()
    }
    pub fn filter(&self) -> LoggerFilter {
        self.integration.filter()
    }
    pub fn shutdown(self) -> Result<()> {
        self.integration.shutdown()
    }
}

pub fn start(runtime: Runtime, auth: &AuthManager, config: LoggerConfig) -> Result<Logging> {
    let auth_owner = Arc::new(auth.clone());
    let auth = Arc::downgrade(&auth_owner);
    let integration = LoggerIntegration::start(
        runtime,
        config,
        Arc::new(catalog),
        Arc::new(move |context| {
            if let Some(user_id) = context.user_id.as_deref() {
                let auth = auth
                    .upgrade()
                    .ok_or_else(|| Error::Service("Logger authorization is stopped".into()))?;
                let user = auth.get_user(user_id)?;
                if !user.is_active || (!user.is_owner && !user.is_admin) {
                    return Err(Error::InvalidInput(
                        "Administrator privileges are required".into(),
                    ));
                }
            }
            Ok(())
        }),
    )?;
    Ok(Logging {
        integration,
        _auth: auth_owner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_config::{
        ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult,
        NewConfigEntry, SetupOutcome,
    };
    use hass_core::{Context, MemoryStore, ServiceCall};
    use serde_json::{Map, json};

    struct ReceiptTestDriver;

    impl ConfigEntryDriver for ReceiptTestDriver {
        fn domain(&self) -> &str {
            "spotify"
        }

        fn setup(
            &self,
            runtime: &Runtime,
            _entry: &ConfigEntry,
        ) -> hass_core::Result<SetupOutcome> {
            runtime
                .loaded_components()
                .record_successful_component("spotify.media_player");
            Ok(SetupOutcome::Ready)
        }

        fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> hass_core::Result<()> {
            Ok(())
        }

        fn config_flow(&self, _request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
            Err(Error::Service("test config flow is unused".into()))
        }
    }

    #[test]
    fn loaded_catalog_uses_success_receipts_not_registered_services() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        runtime
            .register_service(
                "media_player",
                "test",
                json!({}),
                Arc::new(|_, _| {
                    Ok(hass_core::ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: Context::default(),
                    })
                }),
            )
            .unwrap();
        let loaded = runtime.loaded_components();
        let provider = loaded_catalog(loaded.clone());

        assert!(!(provider)().contains_key("media_player"));

        loaded.record_successful_component("spotify.media_player");
        assert!((provider)().contains_key("spotify"));
        assert!(!(provider)().contains_key("media_player"));

        loaded.record_successful_component("media_player");
        assert!((provider)().contains_key("media_player"));

        for domain in ["button", "climate", "cover", "fan", "light", "switch"] {
            loaded.record_successful_component(domain);
        }
        let loaded_catalog = (provider)();
        let full_catalog = catalog();
        for domain in ["button", "climate", "cover", "fan", "light", "switch"] {
            assert!(loaded_catalog.contains_key(domain));
            assert_eq!(
                full_catalog.get(domain).unwrap().as_slice(),
                [format!("hass_{domain}")].as_slice()
            );
        }
    }

    #[test]
    fn loaded_catalog_receipts_survive_entry_unload_and_are_runtime_scoped() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let loaded = runtime.loaded_components();
        let provider = loaded_catalog(loaded);
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let driver: Arc<dyn ConfigEntryDriver> = Arc::new(ReceiptTestDriver);
        entries.register_driver(driver).unwrap();
        let entry = entries
            .create_entry(NewConfigEntry::new("spotify", "Fixture", Map::new()))
            .unwrap();

        entries.setup_entry(&entry.entry_id).unwrap();
        assert!((provider)().contains_key("spotify"));
        entries.unload_entry(&entry.entry_id).unwrap();
        assert!((provider)().contains_key("spotify"));

        let fresh_runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        assert!(loaded_catalog(fresh_runtime.loaded_components())().is_empty());
    }

    #[test]
    fn native_target_aliases_keep_integration_levels_independent() {
        let catalog = catalog();
        let mut targets = BTreeSet::new();
        for aliases in catalog.values() {
            for target in aliases {
                assert!(
                    targets.insert(target.replace("::", ".")),
                    "duplicate integration target: {target}"
                );
            }
        }
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let auth = AuthManager::new(runtime.clone()).unwrap();
        let logger = start(runtime, &auth, LoggerConfig::default()).unwrap();
        let manager = logger.manager();
        manager
            .update_settings(
                "timer",
                hass_logger::TargetKind::Integration,
                hass_logger::LoggerLevel::Debug,
                hass_logger::Persistence::None,
            )
            .unwrap();
        assert_eq!(
            manager
                .effective_level("hass_native::timer")
                .unwrap()
                .number(),
            10
        );
        assert_eq!(
            manager
                .effective_level("hass_native::schedule")
                .unwrap()
                .number(),
            20
        );
        assert_eq!(
            manager
                .effective_level("hass_native::input_button")
                .unwrap()
                .number(),
            20
        );
        let info = manager.log_info(&catalog).unwrap();
        assert_eq!(
            info.iter()
                .find(|entry| entry.domain == "timer")
                .unwrap()
                .level,
            10
        );
        assert_eq!(
            info.iter()
                .find(|entry| entry.domain == "schedule")
                .unwrap()
                .level,
            20
        );
        manager
            .update_settings(
                "scene",
                hass_logger::TargetKind::Integration,
                hass_logger::LoggerLevel::Error,
                hass_logger::Persistence::None,
            )
            .unwrap();
        assert_eq!(
            manager
                .effective_level("hass_script::scene")
                .unwrap()
                .number(),
            40
        );
        assert_eq!(manager.effective_level("hass_script").unwrap().number(), 20);
        logger.shutdown().unwrap();
    }

    #[test]
    fn service_auth_lives_with_host_and_checks_current_user_before_cleanup() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let auth = AuthManager::new(runtime.clone()).unwrap();
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "logger-host-owner")
            .unwrap();
        let user = auth
            .create_user_with_username(&owner.id, "user", "User", "logger-host-user", false)
            .unwrap();
        let logger = start(runtime.clone(), &auth, LoggerConfig::default()).unwrap();
        let manager = logger.manager();
        let call = |user_id: String| ServiceCall {
            domain: "logger".into(),
            service: "set_default_level".into(),
            data: Map::from_iter([("level".into(), json!("warning"))]),
            target: Map::new(),
            context: Context {
                user_id: Some(user_id),
                ..Context::default()
            },
        };
        assert!(runtime.call_service(call(user.id.clone())).is_err());
        runtime.call_service(call(owner.id)).unwrap();
        assert_eq!(manager.effective_level("hass").unwrap().number(), 30);
        logger.shutdown().unwrap();
        assert!(manager.effective_level("hass").is_err());
        assert!(runtime.services().get("logger").is_none());
    }
}
