use hass_application_credentials::{
    ApplicationCredentialsError, ApplicationCredentialsIntegration, ApplicationCredentialsPlatform,
    AuthorizationServer, ClientCredential, ConfigCatalog, CreateCredential, CredentialChangeType,
    CredentialRecord, CustomOAuth2Implementation, ResolvedOAuthImplementation, STORAGE_KEY,
    SUBSCRIPTION_CAPACITY, SubscriptionError,
};
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult,
    NewConfigEntry, SetupOutcome,
};
use hass_core::{
    Error as CoreError, Event, EventHistoryQuery, HistoryQuery, HistorySeries, MemoryStore,
    PurgeResult, Runtime, State, Store,
};
use hass_storage::SqliteStore;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::tempdir;

fn integration_with_store(
    store: Arc<dyn Store>,
) -> (Runtime, ConfigEntries, ApplicationCredentialsIntegration) {
    let runtime = Runtime::new(store).unwrap();
    let config_entries = ConfigEntries::new(runtime.clone()).unwrap();
    let integration =
        ApplicationCredentialsIntegration::new(runtime.clone(), config_entries.clone()).unwrap();
    (runtime, config_entries, integration)
}

fn default_platform() -> ApplicationCredentialsPlatform {
    ApplicationCredentialsPlatform::new().with_authorization_server_factory(|_| {
        Box::pin(async {
            Ok(AuthorizationServer {
                authorize_url: "https://auth.example/authorize".to_owned(),
                token_url: "https://auth.example/token".to_owned(),
            })
        })
    })
}

#[tokio::test]
async fn platform_closure_destructors_can_release_nested_registration_guards() {
    for shutdown in [false, true] {
        let (completed, receiver) = tokio::sync::oneshot::channel();
        let runtime = tokio::runtime::Handle::current();
        let worker = std::thread::spawn(move || {
            let (_, _, integration) = integration_with_store(Arc::new(MemoryStore::default()));
            let registry = integration.platform_registry();
            let nested = registry
                .register("nested_fixture", default_platform())
                .unwrap();
            let parent = ApplicationCredentialsPlatform::new().with_authorization_server_factory(
                move |_| {
                    let _keep_registration = &nested;
                    Box::pin(async {
                        Ok(AuthorizationServer {
                            authorize_url: "https://auth.example/authorize".into(),
                            token_url: "https://auth.example/token".into(),
                        })
                    })
                },
            );
            let parent_guard = registry.register("parent_fixture", parent).unwrap();
            if shutdown {
                runtime.block_on(integration.shutdown());
                assert!(registry.loaded_domains().is_err());
            } else {
                drop(parent_guard);
                assert!(registry.loaded_domains().unwrap().is_empty());
            }
            let _ = completed.send(());
        });
        // Use an independent std thread: a broken destructor must fail this
        // bounded check rather than strand Tokio's blocking-pool shutdown.
        tokio::time::timeout(Duration::from_secs(2), receiver)
            .await
            .expect("platform destructor reentered a held registry lock")
            .unwrap();
        worker.join().unwrap();
    }
}

fn create(domain: &str, client_id: &str, secret: &str) -> CreateCredential {
    CreateCredential {
        domain: domain.to_owned(),
        client_id: client_id.to_owned(),
        client_secret: secret.to_owned(),
        auth_domain: None,
        name: None,
    }
}

fn config_entry(entries: &ConfigEntries, domain: &str, auth_implementation: &str) -> String {
    if entries.driver(domain).is_none() {
        entries
            .register_driver(Arc::new(FixtureConfigEntryDriver {
                domain: domain.to_owned(),
            }))
            .unwrap();
    }
    let mut data = Map::new();
    data.insert("auth_implementation".to_owned(), json!(auth_implementation));
    entries
        .create_entry(NewConfigEntry::new(domain, "Fixture", data))
        .unwrap()
        .entry_id
}

struct FixtureConfigEntryDriver {
    domain: String,
}

impl ConfigEntryDriver for FixtureConfigEntryDriver {
    fn domain(&self) -> &str {
        &self.domain
    }

    fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> hass_core::Result<SetupOutcome> {
        Ok(SetupOutcome::Ready)
    }

    fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> hass_core::Result<()> {
        Ok(())
    }

    fn config_flow(&self, _request: &ConfigFlowRequest) -> hass_core::Result<ConfigFlowResult> {
        Err(CoreError::InvalidInput("fixture flow is unused".to_owned()))
    }
}

#[tokio::test]
async fn unicode_credential_ids_follow_pinned_transliteration_and_collision_suffixes() {
    let (runtime, _, integration) = integration_with_store(Arc::new(MemoryStore::default()));
    let _platform = integration
        .platform_registry()
        .register("demo", default_platform())
        .unwrap();
    let manager = integration.manager();
    for (client_id, expected) in [
        ("café", "demo_cafe"),
        ("cafe", "demo_cafe_2"),
        ("汉字", "demo_yi_zi"),
        ("κόσμος", "demo_kosmos"),
        ("a&#X26;b", "demo_a_x26_b"),
    ] {
        let item = manager
            .create_admin(create("demo", client_id, "synthetic-secret"))
            .unwrap();
        assert_eq!(item.id, expected);
        assert_eq!(item.client_id, client_id);
    }
    let stored = runtime.get_data(STORAGE_KEY).unwrap().unwrap();
    assert_eq!(stored["items"][1]["id"], "demo_cafe_2");
    assert_eq!(stored["items"][2]["client_id"], "汉字");
}

#[tokio::test]
async fn create_trims_required_values_persists_optional_fields_and_builds_default_descriptor() {
    let (runtime, _entries, integration) = integration_with_store(Arc::new(MemoryStore::default()));
    let registry = integration.platform_registry();
    let _platform = registry.register("demo", default_platform()).unwrap();
    let manager = integration.manager();

    let record = manager
        .create_admin(CreateCredential {
            domain: "demo".to_owned(),
            client_id: "\u{1c} Client.ID \u{1f}".to_owned(),
            client_secret: "\u{1d}  private-client-secret  \u{1e}".to_owned(),
            auth_domain: None,
            name: None,
        })
        .unwrap();

    assert_eq!(record.id, "demo_client_id");
    assert_eq!(record.client_id, "Client.ID");
    assert_eq!(record.client_secret, "private-client-secret");
    assert_eq!(record.auth_domain, None);
    assert_eq!(record.name, None);
    assert!(!format!("{record:?}").contains("private-client-secret"));
    assert_eq!(manager.list_admin().unwrap(), vec![record.clone()]);
    let persisted = runtime.get_data(STORAGE_KEY).unwrap().unwrap();
    assert_eq!(persisted["version"], json!(1));
    assert_eq!(persisted["items"][0]["id"], json!("demo_client_id"));
    assert_eq!(
        persisted["items"][0]["client_secret"],
        json!("private-client-secret")
    );
    assert_eq!(
        serde_json::to_value(&record).unwrap()["client_secret"],
        json!("private-client-secret")
    );

    let provider_credentials = manager.credentials_for_provider("demo").unwrap();
    assert_eq!(provider_credentials.len(), 1);
    assert_eq!(provider_credentials[0].auth_domain, record.id);

    let implementations = manager.provide_implementations("demo").await.unwrap();
    let [ResolvedOAuthImplementation::Local(local)] = implementations.as_slice() else {
        panic!("expected one default local implementation");
    };
    assert_eq!(local.auth_domain, record.id);
    assert_eq!(local.client_id, "Client.ID");
    assert_eq!(local.name, "Client.ID");
    assert_eq!(local.client_secret(), "private-client-secret");
    assert!(!format!("{local:?}").contains("private-client-secret"));
}

#[tokio::test]
async fn source_config_catalog_invokes_placeholders_and_lists_only_loaded_providers() {
    let (_runtime, _entries, integration) =
        integration_with_store(Arc::new(MemoryStore::default()));
    let placeholder_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&placeholder_calls);
    let platform = default_platform().with_description_placeholders(move |_| {
        let calls = Arc::clone(&calls);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok([(
                "redirect_uri".to_owned(),
                "https://example/callback".to_owned(),
            )]
            .into_iter()
            .collect())
        })
    });
    let _platform = integration
        .platform_registry()
        .register("loaded_provider", platform)
        .unwrap();
    let manager = integration.manager();

    let catalog: ConfigCatalog = manager.config().await.unwrap();
    assert_eq!(catalog.domains, vec!["loaded_provider"]);
    assert_eq!(
        catalog.integrations["loaded_provider"]
            .description_placeholders
            .as_ref()
            .unwrap()["redirect_uri"],
        "https://example/callback"
    );
    assert_eq!(placeholder_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        integration.platform_registry().loaded_domains().unwrap(),
        vec!["loaded_provider"]
    );
}

#[tokio::test]
async fn imported_alias_is_resolved_and_delete_guard_protects_alias_and_stable_id() {
    let (_runtime, entries, integration) = integration_with_store(Arc::new(MemoryStore::default()));
    let _platform = integration
        .platform_registry()
        .register("demo", default_platform())
        .unwrap();
    let manager = integration.manager();
    let imported = ClientCredential {
        client_id: " legacy-app ".to_owned(),
        client_secret: " legacy-secret ".to_owned(),
        name: None,
    };

    assert!(
        manager
            .import_client_credential("demo", imported.clone(), None)
            .unwrap()
    );
    let item = manager.list_admin().unwrap().remove(0);
    assert_eq!(item.id, "demo_legacy_app");
    assert_eq!(item.auth_domain.as_deref(), Some("demo"));
    assert_eq!(item.name.as_deref(), Some("Import from configuration.yaml"));
    assert!(
        !manager
            .import_client_credential(
                "demo",
                ClientCredential {
                    client_secret: "must-not-replace".to_owned(),
                    ..imported
                },
                None
            )
            .unwrap()
    );
    assert_eq!(
        manager.list_admin().unwrap()[0].client_secret,
        "legacy-secret"
    );

    let entry_id = config_entry(&entries, "demo", "demo");
    assert_eq!(
        manager
            .credential_id_for_config_entry(&entry_id)
            .unwrap()
            .as_deref(),
        Some(item.id.as_str())
    );
    assert!(matches!(
        manager.delete_admin(&item.id),
        Err(ApplicationCredentialsError::CredentialInUse { .. })
    ));

    entries.remove_entry(&entry_id).unwrap();
    assert!(matches!(
        manager.credential_id_for_config_entry(&entry_id),
        Err(ApplicationCredentialsError::ConfigEntryNotFound(_))
    ));
    manager.delete_admin(&item.id).unwrap();

    let second = manager
        .create_admin(CreateCredential {
            domain: "demo".to_owned(),
            client_id: "other".to_owned(),
            client_secret: "another-secret".to_owned(),
            auth_domain: Some("compatibility-alias".to_owned()),
            name: None,
        })
        .unwrap();
    let id_entry = config_entry(&entries, "demo", &second.id);
    assert!(matches!(
        manager.delete_admin(&second.id),
        Err(ApplicationCredentialsError::CredentialInUse { .. })
    ));
    entries.remove_entry(&id_entry).unwrap();
}

#[tokio::test]
async fn duplicate_aliases_preserve_source_last_value_and_block_live_deletion() {
    let (_runtime, entries, integration) = integration_with_store(Arc::new(MemoryStore::default()));
    let _platform = integration
        .platform_registry()
        .register("demo", default_platform())
        .unwrap();
    let manager = integration.manager();
    let first = manager
        .create_admin(CreateCredential {
            auth_domain: Some("same-alias".to_owned()),
            ..create("demo", "first", "first-secret")
        })
        .unwrap();
    let second = manager
        .create_admin(CreateCredential {
            auth_domain: Some("same-alias".to_owned()),
            ..create("demo", "second", "second-secret")
        })
        .unwrap();

    let projected = manager.credentials_for_provider("demo").unwrap();
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].auth_domain, "same-alias");
    assert_eq!(projected[0].credential.client_secret, "second-secret");
    let entry_id = config_entry(&entries, "demo", "same-alias");
    assert_eq!(
        manager
            .credential_id_for_config_entry(&entry_id)
            .unwrap()
            .as_deref(),
        Some(first.id.as_str())
    );
    assert!(matches!(
        manager.delete_admin(&first.id),
        Err(ApplicationCredentialsError::CredentialInUse { .. })
    ));
    assert!(matches!(
        manager.delete_admin(&second.id),
        Err(ApplicationCredentialsError::CredentialInUse { .. })
    ));
}

#[tokio::test]
async fn custom_factory_is_invoked_and_wins_over_default_factory() {
    struct FixtureCustom;
    impl CustomOAuth2Implementation for FixtureCustom {
        fn name(&self) -> Option<&str> {
            Some("fixture custom")
        }
    }

    let (_runtime, _entries, integration) =
        integration_with_store(Arc::new(MemoryStore::default()));
    let calls = Arc::new(AtomicUsize::new(0));
    let custom_calls = Arc::clone(&calls);
    let platform =
        default_platform().with_custom_auth_factory(move |_, auth_domain, credential| {
            let custom_calls = Arc::clone(&custom_calls);
            let auth_domain = auth_domain.to_owned();
            Box::pin(async move {
                assert_eq!(auth_domain, "custom-alias");
                assert_eq!(credential.client_secret, "custom-secret");
                custom_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Arc::new(FixtureCustom) as Arc<dyn CustomOAuth2Implementation>)
            })
        });
    let _platform = integration
        .platform_registry()
        .register("custom_provider", platform)
        .unwrap();
    let manager = integration.manager();
    manager
        .create_admin(CreateCredential {
            auth_domain: Some("custom-alias".to_owned()),
            ..create("custom_provider", "app", "custom-secret")
        })
        .unwrap();

    let implementations = manager
        .provide_implementations("custom_provider")
        .await
        .unwrap();
    assert_eq!(implementations.len(), 1);
    assert!(matches!(
        &implementations[0],
        ResolvedOAuthImplementation::Custom { auth_domain, implementation }
            if auth_domain == "custom-alias" && implementation.name() == Some("fixture custom")
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn duplicate_or_absent_provider_and_unsupported_update_follow_source_contract() {
    let (_runtime, _entries, integration) =
        integration_with_store(Arc::new(MemoryStore::default()));
    let manager = integration.manager();
    assert!(matches!(
        manager.create_admin(create("absent", "app", "secret")),
        Err(ApplicationCredentialsError::NoPlatform(domain)) if domain == "absent"
    ));
    assert!(matches!(
        integration
            .platform_registry()
            .register("empty", ApplicationCredentialsPlatform::new()),
        Err(ApplicationCredentialsError::InvalidPlatform(_))
    ));
    let registry = integration.platform_registry();
    let first = registry.register("demo", default_platform()).unwrap();
    let _other = registry.register("other", default_platform()).unwrap();
    assert!(matches!(
        registry.register("demo", default_platform()),
        Err(ApplicationCredentialsError::DuplicatePlatform(_))
    ));
    drop(first);
    assert_eq!(registry.loaded_domains().unwrap(), vec!["other"]);
    assert!(matches!(
        manager.update_admin("missing"),
        Err(ApplicationCredentialsError::ItemNotFound(_))
    ));
    let item = manager
        .create_admin(create("other", "app", "secret"))
        .unwrap();
    assert!(matches!(
        manager.update_admin(&item.id),
        Err(ApplicationCredentialsError::UpdatesNotSupported)
    ));
}

#[tokio::test]
async fn observer_snapshot_is_race_free_and_stream_is_bounded_to_64_events() {
    let (_runtime, _entries, integration) =
        integration_with_store(Arc::new(MemoryStore::default()));
    let _platform = integration
        .platform_registry()
        .register("demo", default_platform())
        .unwrap();
    let manager = integration.manager();
    let mut subscription = manager.subscribe_admin().unwrap();
    assert_eq!(
        subscription.take_snapshot().unwrap(),
        Vec::<CredentialRecord>::new()
    );

    let first = manager
        .create_admin(create("demo", "first", "stream-secret"))
        .unwrap();
    let event = subscription.recv().await.unwrap();
    assert_eq!(event.change, CredentialChangeType::Added);
    assert_eq!(event.item.id, first.id);

    for index in 0..(SUBSCRIPTION_CAPACITY + 2) {
        manager
            .create_admin(create("demo", &format!("app-{index}"), "stream-secret"))
            .unwrap();
    }
    assert!(matches!(
        subscription.recv().await,
        Err(SubscriptionError::Lagged(2))
    ));
    assert_eq!(
        subscription.recv().await.unwrap().change,
        CredentialChangeType::Added
    );
}

#[tokio::test]
async fn failed_persistence_does_not_change_memory_or_publish_events() {
    let store = Arc::new(FailingMetadataStore::default());
    let (_runtime, _entries, integration) = integration_with_store(store.clone());
    let _platform = integration
        .platform_registry()
        .register("demo", default_platform())
        .unwrap();
    let manager = integration.manager();
    manager
        .create_admin(create("demo", "saved", "saved-secret"))
        .unwrap();
    let mut subscription = manager.subscribe_admin().unwrap();
    assert_eq!(subscription.take_snapshot().unwrap().len(), 1);

    store.fail_writes.store(true, Ordering::SeqCst);
    assert!(matches!(
        manager.create_admin(create("demo", "failed", "failed-secret")),
        Err(ApplicationCredentialsError::StorageUnavailable)
    ));
    assert!(matches!(
        manager.delete_admin("demo_saved"),
        Err(ApplicationCredentialsError::StorageUnavailable)
    ));
    assert_eq!(manager.list_admin().unwrap().len(), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), subscription.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn sqlite_reopen_retains_exact_record_and_future_or_malformed_store_is_preserved() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("application-credentials.sqlite");
    {
        let store = Arc::new(SqliteStore::open(&path).unwrap());
        let runtime = Runtime::new(store.clone()).unwrap();
        drop(store);
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let integration = ApplicationCredentialsIntegration::new(runtime, entries).unwrap();
        let _platform = integration
            .platform_registry()
            .register("demo", default_platform())
            .unwrap();
        let record = integration
            .manager()
            .create_admin(CreateCredential {
                domain: "demo".to_owned(),
                client_id: "client".to_owned(),
                client_secret: "sqlite-secret".to_owned(),
                auth_domain: Some("legacy-alias".to_owned()),
                name: Some("Stored name".to_owned()),
            })
            .unwrap();
        assert_eq!(record.id, "demo_client");
        integration.shutdown().await;
    }
    {
        let store = Arc::new(SqliteStore::open(&path).unwrap());
        let runtime = Runtime::new(store.clone()).unwrap();
        drop(store);
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let integration = ApplicationCredentialsIntegration::new(runtime, entries).unwrap();
        let _platform = integration
            .platform_registry()
            .register("demo", default_platform())
            .unwrap();
        let record = integration.manager().list_admin().unwrap().remove(0);
        assert_eq!(record.id, "demo_client");
        assert_eq!(record.domain, "demo");
        assert_eq!(record.client_id, "client");
        assert_eq!(record.client_secret, "sqlite-secret");
        assert_eq!(record.auth_domain.as_deref(), Some("legacy-alias"));
        assert_eq!(record.name.as_deref(), Some("Stored name"));
    }

    for (raw, expected) in [
        (json!({"version": 8, "items": []}), 8),
        (
            json!({"version": 1, "items": "malformed", "future_secret": "private"}),
            1,
        ),
    ] {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store).unwrap();
        runtime.set_data(STORAGE_KEY, &raw).unwrap();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        let result = ApplicationCredentialsIntegration::new(runtime.clone(), entries);
        if expected == 8 {
            assert!(matches!(
                result,
                Err(ApplicationCredentialsError::UnsupportedStoreVersion(8))
            ));
        } else {
            assert!(matches!(
                result,
                Err(ApplicationCredentialsError::MalformedStore)
            ));
        }
        assert_eq!(runtime.get_data(STORAGE_KEY).unwrap(), Some(raw));
    }
}

#[tokio::test]
async fn shutdown_clears_callbacks_closes_subscriptions_and_joins_admitted_callbacks() {
    let (_runtime, _entries, integration) =
        integration_with_store(Arc::new(MemoryStore::default()));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let release_rx = Arc::new(std::sync::Mutex::new(Some(release_rx)));
    let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
    let platform =
        ApplicationCredentialsPlatform::new().with_authorization_server_factory(move |_| {
            let started_tx = Arc::clone(&started_tx);
            let release_rx = Arc::clone(&release_rx);
            Box::pin(async move {
                if let Some(tx) = started_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let rx = release_rx.lock().unwrap().take().unwrap();
                let _ = rx.await;
                Ok(AuthorizationServer {
                    authorize_url: "https://auth.example/authorize".to_owned(),
                    token_url: "https://auth.example/token".to_owned(),
                })
            })
        });
    let registration = integration
        .platform_registry()
        .register("demo", platform)
        .unwrap();
    let manager = integration.manager();
    manager
        .create_admin(create("demo", "app", "secret"))
        .unwrap();
    let mut subscription = manager.subscribe_admin().unwrap();
    let _ = subscription.take_snapshot();

    let provider_task = tokio::spawn(async move { manager.provide_implementations("demo").await });
    started_rx.await.unwrap();
    let shutdown_task = tokio::spawn(async move { integration.shutdown().await });
    tokio::task::yield_now().await;
    assert!(!shutdown_task.is_finished());
    assert!(matches!(
        subscription.recv().await,
        Err(SubscriptionError::Closed)
    ));
    release_tx.send(()).unwrap();
    shutdown_task.await.unwrap();
    assert!(provider_task.await.unwrap().is_ok());
    drop(registration);
}

#[derive(Default)]
struct FailingMetadataStore {
    inner: MemoryStore,
    fail_writes: AtomicBool,
}

impl Store for FailingMetadataStore {
    fn load_states(&self) -> hass_core::Result<Vec<State>> {
        self.inner.load_states()
    }

    fn put_state(&self, state: &State, event: Option<&Event>) -> hass_core::Result<()> {
        self.inner.put_state(state, event)
    }

    fn remove_state(&self, entity_id: &str, event: &Event) -> hass_core::Result<()> {
        self.inner.remove_state(entity_id, event)
    }

    fn append_event(&self, event: &Event) -> hass_core::Result<()> {
        self.inner.append_event(event)
    }

    fn get_data(&self, key: &str) -> hass_core::Result<Option<Value>> {
        self.inner.get_data(key)
    }

    fn set_data(&self, key: &str, value: &Value) -> hass_core::Result<()> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(CoreError::Storage("injected metadata failure".to_owned()));
        }
        self.inner.set_data(key, value)
    }

    fn delete_data(&self, key: &str) -> hass_core::Result<bool> {
        self.inner.delete_data(key)
    }

    fn history(
        &self,
        entity_id: &str,
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
    ) -> hass_core::Result<Vec<State>> {
        self.inner.history(entity_id, start, end)
    }

    fn history_query(&self, query: &HistoryQuery) -> hass_core::Result<Vec<HistorySeries>> {
        self.inner.history_query(query)
    }

    fn event_history_query(&self, query: &EventHistoryQuery) -> hass_core::Result<Vec<Event>> {
        self.inner.event_history_query(query)
    }

    fn purge_before(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> hass_core::Result<PurgeResult> {
        self.inner.purge_before(cutoff)
    }

    fn repack(&self) -> hass_core::Result<()> {
        self.inner.repack()
    }
}
