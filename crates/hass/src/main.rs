//! Independent native Rust implementation of a subset of Home Assistant Core.

mod config;
mod dhcp;
mod dhcp_providers;
mod discovery;
mod helpers;
mod homeassistant;
mod instance_id;
mod integration_drivers;
mod ip_policy;
mod logging;
mod media_player;
mod oauth2;
mod persons;
mod recorder;
mod registry_lifecycle;
mod repairs;
mod restart;
mod spotify;
mod spotify_projection;
mod ssdp;
mod system_health;
mod system_sensors;
mod usb;
mod usb_scheduler;
mod usb_usage;
mod yaml;
mod zeroconf;
mod zeroconf_network;
mod zeroconf_repairs;
mod zones;

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use config::Config;
use hass_core::{Context, Runtime};
use hass_storage::SqliteStore;
use serde_json::Map;
use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::{info, warn};
use tracing_subscriber::prelude::*;

const DHCP_CONFIGURATION_WARNING: &str = "The dhcp integration does not support configuration parameters; remove the parameters from configuration.";
const USB_CONFIGURATION_WARNING: &str = "The usb integration does not support configuration parameters; remove the parameters from configuration.";
const HARDWARE_CONFIGURATION_WARNING: &str = "The hardware integration does not support configuration parameters; remove the parameters from configuration.";
const APPLICATION_CREDENTIALS_CONFIGURATION_WARNING: &str = "The application_credentials integration does not support configuration parameters; remove the parameters from configuration.";

#[derive(Parser)]
#[command(
    name = "hass",
    version,
    about = "Native Rust Home Assistant Core rewrite (partial compatibility)"
)]
struct Args {
    /// Read the supported YAML configuration subset.
    #[arg(short, long, default_value = "configuration.yaml")]
    config: PathBuf,
    /// Validate configuration without opening a database or binding a listener.
    #[arg(long)]
    check: bool,
    /// Override the configured listening IP address.
    #[arg(long)]
    host: Option<IpAddr>,
    /// Override the configured port (0 selects an available port).
    #[arg(long)]
    port: Option<u16>,
    /// Override the database path (relative to the current directory).
    #[arg(long)]
    database: Option<PathBuf>,
    /// Read a Bearer token from this file; alternatively set HASS_TOKEN.
    #[arg(long, env = "HASS_TOKEN_FILE")]
    token_file: Option<PathBuf>,
}

fn read_token(path: Option<&Path>) -> Result<String> {
    let value = if let Some(path) = path {
        std::fs::read_to_string(path).context("cannot read token file")?
    } else {
        match std::env::var("HASS_TOKEN") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return Ok(String::new()),
            Err(error) => return Err(error).context("HASS_TOKEN is not valid text"),
        }
    };
    let token = value.trim().to_owned();
    if token.len() < 32 || token.len() > 4096 || !token.bytes().all(|c| c.is_ascii_graphic()) {
        bail!("access token must contain 32 to 4096 non-whitespace printable bytes");
    }
    Ok(token)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = Config::load(&args.config)?;
    if args.check {
        if config.system_health_has_parameters() {
            eprintln!("{}", system_health::CONFIGURATION_WARNING);
        }
        if config.ssdp_has_parameters() {
            eprintln!("{}", ssdp::CONFIGURATION_WARNING);
        }
        if config.zeroconf_has_deprecated_parameters() {
            eprintln!("{}", zeroconf::CONFIGURATION_WARNING);
        }
        if config.dhcp_has_parameters() {
            eprintln!("{DHCP_CONFIGURATION_WARNING}");
        }
        if config.usb_has_parameters() {
            eprintln!("{USB_CONFIGURATION_WARNING}");
        }
        if config.hardware_has_parameters() {
            eprintln!("{HARDWARE_CONFIGURATION_WARNING}");
        }
        if config.application_credentials_has_parameters() {
            eprintln!("{APPLICATION_CREDENTIALS_CONFIGURATION_WARNING}");
        }
        println!("Configuration valid (supported Rust subset)");
        return Ok(());
    }
    let access_token = read_token(args.token_file.as_deref())?;
    let address = SocketAddr::new(
        args.host.unwrap_or(config.http.server_host),
        args.port.unwrap_or(config.http.server_port),
    );
    // Bind before mutating persistent data: an occupied port cannot partially
    // initialize an otherwise unused configuration directory.
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("cannot bind {address}"))?;
    let bound_address = listener.local_addr()?;
    let db_path = args.database.unwrap_or_else(|| {
        args.config
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&config.recorder.db_path)
    });
    let helper_config = config.helpers.clone();
    let mut api_config = config.api_config();
    if let Some(components) = api_config
        .get_mut("components")
        .and_then(serde_json::Value::as_array_mut)
        && !components
            .iter()
            .any(|component| component.as_str() == Some("media_player"))
    {
        components.push(serde_json::Value::String("media_player".into()));
    }
    let seed_config = serde_json::json!({
        "name":config.homeassistant.name,
        "latitude":config.homeassistant.latitude,
        "longitude":config.homeassistant.longitude,
        "elevation":config.homeassistant.elevation,
        "time_zone":config.homeassistant.time_zone,
        "unit_system":config.homeassistant.unit_system,
    });
    let network = hass_api::network::NetworkPolicy::new(
        config.http.use_x_forwarded_for,
        &config.http.trusted_proxies,
    )?;
    let (runtime, auth, registries, config_entries) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            if let Some(parent) = db_path.parent().filter(|path| !path.as_os_str().is_empty()) {
                let mut directory = std::fs::DirBuilder::new();
                directory.recursive(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    directory.mode(0o700);
                }
                directory
                    .create(parent)
                    .context("cannot create database directory")?;
            }
            let store = SqliteStore::open(&db_path)?;
            let runtime = Runtime::new(Arc::new(store))?;
            helpers::setup(&runtime, &helper_config)?;
            let auth = hass_auth::AuthManager::new(runtime.clone())?;
            let config_entries = hass_config::ConfigEntries::new(runtime.clone())?;
            let registry_entries = config_entries.clone();
            let registries = hass_registry::Registries::new_with_config_entry_domain_resolver(
                runtime.clone(),
                Arc::new(move |entry_id| {
                    registry_entries
                        .get_entry(entry_id)
                        .map(|entry| entry.domain)
                }),
            )?;
            config_entries.initialize_core_config(&seed_config)?;
            Ok((runtime, auth, registries, config_entries))
        })
        .await
        .context("runtime initialization failed")??;
    let repairs_runtime = runtime.clone();
    let repairs_entries = config_entries.clone();
    let repairs_registries = registries.clone();
    let repairs = tokio::task::spawn_blocking(move || {
        repairs::RepairsHost::start(repairs_runtime, repairs_entries, repairs_registries)
    })
    .await??;
    let repairs_manager = repairs.manager();
    let registry_lifecycle = registry_lifecycle::RegistryLifecycleHost::start(
        config_entries.clone(),
        registries.clone(),
    )?;
    let restart = restart::RestartHost::start(runtime.clone(), &auth)?;
    let system_health = system_health::start(runtime.clone())?;
    let system_health_manager = system_health.manager();
    let network_runtime = runtime.clone();
    let network_adapters = tokio::task::spawn_blocking(move || {
        hass_network::NetworkIntegration::start(
            network_runtime,
            Arc::new(hass_network::SystemAdapterEnumerator),
            Arc::new(hass_network::SystemRouteSourceResolver),
        )
    })
    .await??;
    let network_adapter_manager = network_adapters.manager();
    let logger_runtime = runtime.clone();
    let logger_auth = auth.clone();
    let logger_config = config.logger.clone().unwrap_or_default();
    let logger = tokio::task::spawn_blocking(move || {
        logging::start(logger_runtime, &logger_auth, logger_config)
    })
    .await??;
    let logger_manager = logger.manager();
    let system_log = hass_system_log::SystemLogIntegration::start(
        runtime.clone(),
        config.system_log.clone().unwrap_or_default(),
        Arc::new(logging::catalog),
    )?;
    let system_log_manager = system_log.manager();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(hass_logger::NormalizedEventFormat)
                .with_filter(logger.filter()),
        )
        .with(system_log.layer().with_filter(logger.filter()))
        .try_init()
        .context("cannot initialize native logging")?;
    if config.system_health_has_parameters() {
        tracing::error!(target: "hass::system_health", "{}", system_health::CONFIGURATION_WARNING);
    }
    if config.ssdp_has_parameters() {
        tracing::error!(target: "hass::ssdp", "{}", ssdp::CONFIGURATION_WARNING);
    }
    if config.zeroconf_has_deprecated_parameters() {
        tracing::warn!(target: "hass::zeroconf", "{}", zeroconf::CONFIGURATION_WARNING);
    }
    if config.dhcp_has_parameters() {
        tracing::error!(target: "hass::dhcp", "{DHCP_CONFIGURATION_WARNING}");
    }
    if config.usb_has_parameters() {
        tracing::error!(target: "hass::usb", "{USB_CONFIGURATION_WARNING}");
    }
    if config.hardware_has_parameters() {
        tracing::error!(target: "hass::hardware", "{HARDWARE_CONFIGURATION_WARNING}");
    }
    if config.application_credentials_has_parameters() {
        tracing::error!(target: "hass::application_credentials", "{APPLICATION_CREDENTIALS_CONFIGURATION_WARNING}");
    }
    let application_credentials = if config.application_credentials_enabled {
        Some(
            hass_application_credentials::ApplicationCredentialsIntegration::new(
                runtime.clone(),
                config_entries.clone(),
            )?,
        )
    } else {
        None
    };
    let application_credentials_manager = application_credentials
        .as_ref()
        .map(hass_application_credentials::ApplicationCredentialsIntegration::manager);
    let discovery_dispatcher = discovery::DiscoveryDispatcher::new(config_entries.clone());
    let mut dhcp = if config.dhcp_enabled {
        Some(dhcp::DhcpHost::start(
            runtime.clone(),
            config_entries.clone(),
            registries.clone(),
            discovery_dispatcher.handle(),
        )?)
    } else {
        None
    };
    let dhcp_manager = dhcp.as_ref().map(dhcp::DhcpHost::manager);
    let usb = if config.usb_enabled {
        Some(usb::UsbHost::start(discovery_dispatcher.handle()).await?)
    } else {
        None
    };
    let usb_manager = usb.as_ref().map(usb::UsbHost::manager);
    let usb_usage = usb.as_ref().map(|_| {
        Arc::new(usb_usage::UsbUsage::new(config_entries.clone()))
            as Arc<dyn hass_api::UsbUsageProvider>
    });
    let mut hardware = if config.hardware_enabled {
        Some(hass_hardware::HardwareIntegration::new(Arc::new(
            hass_hardware_metrics::NativeMetricsProvider::new(),
        ))?)
    } else {
        None
    };
    let hardware_manager = hardware
        .as_ref()
        .map(hass_hardware::HardwareIntegration::manager);
    let mut zeroconf = if config.zeroconf_enabled {
        Some(
            zeroconf::ZeroconfHost::start(
                runtime.clone(),
                config_entries.clone(),
                network_adapter_manager.clone(),
                repairs_manager.clone(),
                discovery_dispatcher.handle(),
                bound_address,
            )
            .await?,
        )
    } else {
        None
    };
    let zeroconf_manager = zeroconf.as_ref().map(zeroconf::ZeroconfHost::manager);
    let mut ssdp = if config.ssdp_enabled {
        Some(
            ssdp::SsdpHost::start(
                runtime.clone(),
                config_entries.clone(),
                network_adapter_manager.clone(),
                discovery_dispatcher.handle(),
            )
            .await?,
        )
    } else {
        None
    };
    let ssdp_manager = ssdp.as_ref().map(ssdp::SsdpHost::manager);
    let image_runtime = runtime.clone();
    let image_directory = args
        .config
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_owned();
    let images = tokio::task::spawn_blocking(move || {
        hass_image_upload::ImageIntegration::start(image_runtime, image_directory)
    })
    .await??;
    let image_manager = images.manager();
    let mut notifications = hass_notifications::NotificationIntegration::start(runtime.clone())?;
    let notification_manager = notifications.manager();
    let zones = zones::Zones::start(
        runtime.clone(),
        auth.clone(),
        config_entries.clone(),
        args.config.clone(),
        config.zone.clone(),
    )
    .await?;
    let zone_manager = zones.manager();
    let mut trackers = hass_device_tracker::DeviceTrackerIntegration::start(
        runtime.clone(),
        zone_manager.clone(),
        config.device_tracker.clone(),
    )
    .await?;
    let persons = persons::Persons::start(
        runtime.clone(),
        auth.clone(),
        registries.clone(),
        args.config.clone(),
        config.person.clone(),
    )
    .await?;
    let person_manager = persons.manager();
    let groups = hass_group::GroupIntegration::start_with_order(
        runtime.clone(),
        config.group.clone(),
        config.group_order.clone(),
    )
    .await?;
    let group_lookup = groups.lookup();
    let target_groups = group_lookup.clone();
    let target_registries = registries.clone();
    let _service_targets =
        runtime.install_service_target_resolver_if_absent(Arc::new(move |runtime, call| {
            match hass_registry::targets::resolve_entity_service_targets(
                runtime,
                Some(&target_registries),
                call,
            )? {
                hass_registry::targets::TargetResolution::NoMatches => Ok(false),
                hass_registry::targets::TargetResolution::Unspecified => Ok(true),
                hass_registry::targets::TargetResolution::Matched(ids) => {
                    let expanded = hass_group::expand_entity_ids(runtime, &target_groups, &ids)?;
                    if expanded.len() > hass_core::MAX_HISTORY_QUERY_FILTERS {
                        return Err(hass_core::Error::InvalidInput(
                            "Expanded group target exceeds the native entity limit".into(),
                        ));
                    }
                    if expanded.is_empty() {
                        return Ok(false);
                    }
                    call.data
                        .insert("entity_id".into(), serde_json::json!(expanded));
                    Ok(true)
                }
            }
        }));
    let homeassistant =
        homeassistant::HomeAssistantServices::start_with_groups(runtime.clone(), group_lookup)?;
    let recorder = recorder::RecorderMaintenance::start(
        runtime.clone(),
        auth.clone(),
        config.recorder.clone(),
    )?;
    let mut native_config = config.native_config();
    native_config.time_zone = runtime.time_zone();
    let native = hass_native::NativeIntegrations::start(runtime.clone(), native_config).await?;
    let scripts = hass_script::ScriptEngine::start(runtime.clone(), config.script, config.scene)?;
    let entity_lifecycle = hass_entity::EntityLifecycle::new(runtime.clone(), registries.clone())?;
    let media_players = hass_media_player::MediaPlayerDispatcher::new(entity_lifecycle.clone());
    let media_player_host =
        media_player::MediaPlayerHost::start(runtime.clone(), media_players.clone())?;
    let sun = hass_sun::SunIntegration::start(
        runtime.clone(),
        registries.clone(),
        entity_lifecycle.clone(),
        config_entries.clone(),
        config.sun,
    )
    .await?;
    let tags = hass_tag::TagIntegration::start(
        runtime.clone(),
        registries.clone(),
        entity_lifecycle.clone(),
    )?;
    let tag_manager = tags.manager();
    let system_sensors = system_sensors::SystemSensors::start(
        runtime.clone(),
        registries.clone(),
        entity_lifecycle.clone(),
        config_entries.clone(),
        config.sensor,
    )
    .await?;
    let template_entities = hass_template_entities::TemplateEntities::start_with_lifecycle(
        runtime.clone(),
        config.template,
        entity_lifecycle.clone(),
    )
    .await?;
    let mqtt_topics = hass_automation::mqtt_topics(&config.automation);
    let webhooks = hass_webhook::WebhookManager::new();
    let automations = hass_automation::start_with_features(
        runtime.clone(),
        config.automation,
        hass_automation::AutomationFeatures {
            time_zone: Some(runtime.time_zone()),
            webhooks: Some(webhooks.clone()),
            registries: Some(registries.clone()),
        },
    )?;
    let rest = hass_rest::RestIntegration::start(runtime.clone(), config.rest, config.rest_command)
        .await?;
    let drivers = integration_drivers::NativeDrivers::start_with_entity_lifecycle(
        runtime.clone(),
        config_entries.clone(),
        registries.clone(),
        mqtt_topics,
        config.mqtt,
        entity_lifecycle.clone(),
    )
    .await?;
    let templates = hass_template::TemplateEngine::new(runtime.clone());
    let mut oauth2_factory = None;
    let oauth2_host = if let Some(credentials) = &application_credentials {
        match oauth2::callback_url(&config_entries.core_config(), bound_address) {
            Ok(Some(callback_url)) => {
                let store = Arc::new(hass_oauth2_flow::ConfigEntryTokenStore::new(
                    config_entries.clone(),
                ));
                let factory = hass_oauth2_client::OAuthClientFactory::new(store)?;
                let host = hass_oauth2_flow::OAuthFlowHost::new(
                    config_entries.clone(),
                    credentials.manager(),
                    credentials.platform_registry(),
                    factory.clone(),
                    hass_oauth2_flow::OAuthFlowHostOptions::new(callback_url),
                )?;
                oauth2_factory = Some(factory);
                Some(host)
            }
            Ok(None) => {
                warn!(
                    "External OAuth flows require a configured HTTPS external/internal URL or a loopback listener"
                );
                None
            }
            Err(_) => {
                warn!(
                    "Configured OAuth callback origin is invalid; external OAuth flows are unavailable"
                );
                None
            }
        }
    } else {
        None
    };
    let spotify = if let Some(host) = &oauth2_host {
        let integration = spotify::SpotifyIntegration::start(
            runtime.clone(),
            config_entries.clone(),
            registries.clone(),
            host,
            media_players.clone(),
        )
        .await?;
        if let Some(components) = api_config
            .get_mut("components")
            .and_then(serde_json::Value::as_array_mut)
        {
            components.push(serde_json::Value::String("spotify".into()));
        }
        Some(integration)
    } else {
        None
    };
    let logger_catalog = logging::loaded_catalog(config_entries.clone(), api_config.clone());
    let starting_runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        starting_runtime.fire_event("homeassistant_start", Map::new(), Context::default())
    })
    .await??;
    persons.mark_started().await?;
    let started_runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        started_runtime.fire_event("homeassistant_started", Map::new(), Context::default())
    })
    .await??;
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    discovery_dispatcher.mark_started().await;
    if let Some(host) = &usb {
        host.mark_started().await?;
    }
    let dhcp_providers = if let Some(host) = &mut dhcp {
        let providers =
            dhcp_providers::DhcpProviders::start(host.manager(), network_adapter_manager.clone())
                .await?;
        host.observe_removed_keys()?;
        Some(providers)
    } else {
        None
    };
    if let Some(host) = &mut ssdp {
        host.mark_started(bound_address).await?;
    }
    if let Some(host) = &mut zeroconf {
        host.mark_started().await?;
    }
    let oauth2_manager = oauth2_host
        .as_ref()
        .map(hass_oauth2_flow::OAuthFlowHost::manager);
    let app = hass_api::router_with_features(
        hass_api::ApiState {
            runtime: runtime.clone(),
            access_token,
            config: api_config,
        },
        hass_api::ApiFeatures {
            network,
            auth: Some(auth),
            registries: Some(registries),
            templates: Some(templates),
            config_entries: Some(config_entries.clone()),
            notifications: Some(notification_manager),
            zones: Some(zone_manager),
            persons: Some(person_manager),
            images: Some(image_manager),
            webhooks: Some(webhooks),
            tags: Some(tag_manager),
            logger: Some(logger_manager),
            logger_catalog: Some(logger_catalog),
            system_log: Some(system_log_manager),
            repairs: Some(repairs_manager),
            system_health: Some(system_health_manager),
            ssdp: ssdp_manager,
            zeroconf: zeroconf_manager,
            dhcp: dhcp_manager,
            usb: usb_manager,
            usb_usage,
            hardware: hardware_manager,
            application_credentials: application_credentials_manager,
            oauth2_flows: oauth2_manager,
            media_players: Some(media_players),
            network_adapters: Some(network_adapter_manager),
        },
        Some(shutdown_rx),
    );
    info!(%bound_address, version = hass_core::VERSION, "Rust runtime listening");
    warn!(
        "Rust rewrite in progress; consult docs/upstream-parity.json for verified integration coverage"
    );
    let result = axum_serve(listener, app, shutdown_tx, restart.receiver()).await;
    let restart_requested = restart.requested();
    let flow_shutdown_result = config_entries.shutdown_flow_tasks_async().await;
    let hardware_result = if let Some(hardware) = &mut hardware {
        hardware.shutdown().await
    } else {
        Ok(())
    };
    let usb_result = if let Some(usb) = usb {
        usb.shutdown().await
    } else {
        Ok(())
    };
    let dhcp_providers_result = if let Some(providers) = dhcp_providers {
        providers.shutdown().await
    } else {
        Ok(())
    };
    let dhcp_result = if let Some(dhcp) = dhcp {
        dhcp.shutdown().await
    } else {
        Ok(())
    };
    let zeroconf_stop_result = if let Some(zeroconf) = &mut zeroconf {
        zeroconf.stop_discovery().await
    } else {
        Ok(())
    };
    let ssdp_result = if let Some(ssdp) = ssdp {
        ssdp.shutdown().await
    } else {
        Ok(())
    };
    discovery_dispatcher.shutdown().await;
    drop(system_health);
    let recorder_result = recorder.shutdown().await;
    let automation_result = automations.shutdown().await;
    let system_sensor_result = system_sensors.shutdown().await;
    let sun_result = sun.shutdown().await;
    let tag_result = tags.shutdown();
    let script_result = scripts.shutdown().await;
    let template_entity_result = template_entities.shutdown().await;
    let driver_result = drivers.shutdown().await;
    let spotify_result = if let Some(spotify) = spotify {
        spotify.shutdown().await
    } else {
        Ok(())
    };
    let media_player_result = media_player_host.shutdown();
    if let Some(oauth2_host) = &oauth2_host {
        oauth2_host.shutdown().await;
    }
    let oauth2_factory_result = if let Some(factory) = &oauth2_factory {
        factory.shutdown().await
    } else {
        Ok(())
    };
    if let Some(application_credentials) = &application_credentials {
        application_credentials.shutdown().await;
    }
    let rest_result = rest.shutdown().await;
    let native_result = native.shutdown().await;
    let tracker_result = trackers.shutdown().await;
    let homeassistant_result = homeassistant.shutdown();
    let person_result = persons.shutdown().await;
    let image_result = images.shutdown();
    let zone_result = zones.shutdown().await;
    let group_result = groups.shutdown().await;
    let notification_result = notifications.shutdown();
    let zeroconf_result = if let Some(zeroconf) = zeroconf {
        zeroconf.shutdown().await
    } else {
        Ok(())
    };
    let entity_lifecycle_result = entity_lifecycle.shutdown().await;
    let service_tasks_result = runtime.shutdown_service_tasks_async().await;
    drop(registry_lifecycle);
    drop(repairs);
    network_adapters.shutdown();
    drop(_service_targets);
    let restart_result = restart.shutdown();
    let system_log_result = system_log.shutdown().await;
    let logger_result = logger.shutdown();
    let stop_result = tokio::task::spawn_blocking(move || {
        runtime.fire_event("homeassistant_stop", Map::new(), Context::default())
    })
    .await?;
    result?;
    flow_shutdown_result?;
    service_tasks_result?;
    oauth2_factory_result?;
    ssdp_result?;
    zeroconf_result?;
    zeroconf_stop_result?;
    dhcp_result?;
    dhcp_providers_result?;
    usb_result?;
    hardware_result?;
    recorder_result?;
    automation_result?;
    system_sensor_result?;
    sun_result?;
    tag_result?;
    entity_lifecycle_result?;
    template_entity_result?;
    driver_result?;
    spotify_result?;
    media_player_result?;
    script_result?;
    rest_result?;
    native_result?;
    tracker_result?;
    homeassistant_result?;
    person_result?;
    image_result?;
    zone_result?;
    group_result?;
    notification_result?;
    logger_result?;
    system_log_result?;
    stop_result?;
    restart_result?;
    if restart_requested {
        match restart::exec_current_process().context("cannot restart native application")? {
            restart::RestartOutcome::Spawned(child) => {
                info!(pid = child.id(), "Restarted native application");
            }
        }
    }
    Ok(())
}

async fn axum_serve(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    shutdown: tokio::sync::watch::Sender<bool>,
    mut restart: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            _ = shutdown_signal() => {},
            _ = async {
                if restart.wait_for(|requested| *requested).await.is_err() {
                    std::future::pending::<()>().await;
                }
            } => {},
        }
        let _ = shutdown.send(true);
    })
    .await
    .context("HTTP server failed")
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = term.recv() => {},
                }
            }
            Err(error) => {
                warn!(%error, "cannot listen for SIGTERM; using Ctrl-C");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    info!("Stopping Rust runtime");
}
