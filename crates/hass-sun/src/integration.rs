//! Native Home Assistant Sun config-entry runtime and entities.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use chrono_tz::Tz;
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigEntryState, ConfigFlowRequest,
    ConfigFlowResult, CoreConfig, SetupOutcome,
};
use hass_core::{Context, Error, Runtime};
use hass_entity::{EntityHandle, EntityLifecycle, EntitySnapshot, EntitySpec};
use hass_registry::{DeviceRegistration, ProviderEntityInfo, Registries};
use serde_json::{Map, Value, json};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::{self, MissedTickBehavior};

use crate::{Observer, Position, SolarError, SolarEvent, next_event, position_at};

const DOMAIN: &str = "sun";
const DEVICE_NAME: &str = "Sun";
const MAIN_PROVIDER_ID: &str = "sun:main";
const MAIN_PROVIDER_KEY: &str = "sun";
const MAIN_ENTITY_ID: &str = "sun.sun";
const SUN_ATTRIBUTE_NAMES: [&str; 9] = [
    "next_dawn",
    "next_dusk",
    "next_midnight",
    "next_noon",
    "next_rising",
    "next_setting",
    "elevation",
    "azimuth",
    "rising",
];
const RETRY_POLL_INTERVAL: StdDuration = StdDuration::from_secs(1);
const RETRY_BASE_SECONDS: u64 = 5;
const RETRY_MAX_SECONDS: u64 = 300;

/// Solar phase used to choose the position polling interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SunPhase {
    Night,
    AstronomicalTwilight,
    NauticalTwilight,
    Twilight,
    SmallDay,
    Day,
}

impl SunPhase {
    fn position_interval(self) -> Duration {
        match self {
            Self::Night => Duration::minutes(20),
            Self::AstronomicalTwilight => Duration::minutes(8),
            Self::NauticalTwilight => Duration::minutes(4),
            Self::Twilight | Self::SmallDay => Duration::minutes(2),
            Self::Day => Duration::minutes(4),
        }
    }
}

/// Calculated Sun state, with Home Assistant's two-decimal entity positions.
#[derive(Clone, Debug, PartialEq)]
pub struct SunSnapshot {
    pub next_dawn: DateTime<Utc>,
    pub next_dusk: DateTime<Utc>,
    pub next_midnight: DateTime<Utc>,
    pub next_noon: DateTime<Utc>,
    pub next_rising: DateTime<Utc>,
    pub next_setting: DateTime<Utc>,
    pub solar_elevation: f64,
    pub solar_azimuth: f64,
    pub rising: bool,
    pub next_change: DateTime<Utc>,
    pub phase: SunPhase,
}

impl SunSnapshot {
    /// Calculate all exposed Sun values at a deterministic UTC instant.
    pub fn calculate(
        observer: Observer,
        now: DateTime<Utc>,
        timezone: Tz,
    ) -> Result<Self, SolarError> {
        calculate_snapshot(observer, now, timezone)
    }

    /// Return the fixed `sun.sun` state string.
    #[must_use]
    pub fn state(&self) -> &'static str {
        if self.solar_elevation > -0.833 {
            "above_horizon"
        } else {
            "below_horizon"
        }
    }

    /// Return the nine Home Assistant `sun.sun` attributes.
    #[must_use]
    pub fn attributes(&self) -> Map<String, Value> {
        Map::from_iter([
            ("next_dawn".into(), json!(format_utc(self.next_dawn))),
            ("next_dusk".into(), json!(format_utc(self.next_dusk))),
            (
                "next_midnight".into(),
                json!(format_utc(self.next_midnight)),
            ),
            ("next_noon".into(), json!(format_utc(self.next_noon))),
            ("next_rising".into(), json!(format_utc(self.next_rising))),
            ("next_setting".into(), json!(format_utc(self.next_setting))),
            ("elevation".into(), json!(self.solar_elevation)),
            ("azimuth".into(), json!(self.solar_azimuth)),
            ("rising".into(), json!(self.rising)),
        ])
    }
}

/// Deterministically calculate Home Assistant's current Sun state.
pub fn calculate_snapshot(
    observer: Observer,
    now: DateTime<Utc>,
    timezone: Tz,
) -> Result<SunSnapshot, SolarError> {
    let mut next_change = now
        .checked_add_signed(Duration::days(400))
        .ok_or(SolarError::DateOverflow)?;
    let mut phase = None;

    consider_event(
        observer,
        now,
        timezone,
        SolarEvent::Dawn { depression: 18.0 },
        Some(SunPhase::Night),
        &mut next_change,
        &mut phase,
    )?;
    consider_event(
        observer,
        now,
        timezone,
        SolarEvent::Dawn { depression: 12.0 },
        Some(SunPhase::AstronomicalTwilight),
        &mut next_change,
        &mut phase,
    )?;
    let next_dawn = consider_event(
        observer,
        now,
        timezone,
        SolarEvent::Dawn { depression: 6.0 },
        Some(SunPhase::NauticalTwilight),
        &mut next_change,
        &mut phase,
    )?;
    let next_rising = consider_event(
        observer,
        now,
        timezone,
        SolarEvent::Sunrise,
        Some(SunPhase::Twilight),
        &mut next_change,
        &mut phase,
    )?;
    consider_event(
        observer,
        now,
        timezone,
        SolarEvent::Dawn { depression: -10.0 },
        Some(SunPhase::SmallDay),
        &mut next_change,
        &mut phase,
    )?;
    let next_noon = consider_event(
        observer,
        now,
        timezone,
        SolarEvent::Noon,
        None,
        &mut next_change,
        &mut phase,
    )?;
    consider_event(
        observer,
        now,
        timezone,
        SolarEvent::Dusk { depression: -10.0 },
        Some(SunPhase::Day),
        &mut next_change,
        &mut phase,
    )?;
    let next_setting = consider_event(
        observer,
        now,
        timezone,
        SolarEvent::Sunset,
        Some(SunPhase::SmallDay),
        &mut next_change,
        &mut phase,
    )?;
    let next_dusk = consider_event(
        observer,
        now,
        timezone,
        SolarEvent::Dusk { depression: 6.0 },
        Some(SunPhase::Twilight),
        &mut next_change,
        &mut phase,
    )?;
    consider_event(
        observer,
        now,
        timezone,
        SolarEvent::Dusk { depression: 12.0 },
        Some(SunPhase::NauticalTwilight),
        &mut next_change,
        &mut phase,
    )?;
    consider_event(
        observer,
        now,
        timezone,
        SolarEvent::Dusk { depression: 18.0 },
        Some(SunPhase::AstronomicalTwilight),
        &mut next_change,
        &mut phase,
    )?;
    let next_midnight = consider_event(
        observer,
        now,
        timezone,
        SolarEvent::Midnight,
        None,
        &mut next_change,
        &mut phase,
    )?;

    let phase = match phase {
        Some(phase) => phase,
        None => phase_at(observer, next_change)?,
    };
    let position = rounded_position(position_at(observer, now)?);

    Ok(SunSnapshot {
        next_dawn,
        next_dusk,
        next_midnight,
        next_noon,
        next_rising,
        next_setting,
        solar_elevation: position.elevation,
        solar_azimuth: position.azimuth,
        rising: next_noon < next_midnight,
        next_change,
        phase,
    })
}

fn consider_event(
    observer: Observer,
    now: DateTime<Utc>,
    timezone: Tz,
    event: SolarEvent,
    event_phase: Option<SunPhase>,
    next_change: &mut DateTime<Utc>,
    phase: &mut Option<SunPhase>,
) -> Result<DateTime<Utc>, SolarError> {
    let instant = next_event(observer, now, timezone, event, Duration::zero())?;
    if instant < *next_change {
        *next_change = instant;
        *phase = event_phase;
    }
    Ok(instant)
}

fn phase_at(observer: Observer, instant: DateTime<Utc>) -> Result<SunPhase, SolarError> {
    let elevation = position_at(observer, instant)?.elevation;
    Ok(if elevation >= 10.0 {
        SunPhase::Day
    } else if elevation >= 0.0 {
        SunPhase::SmallDay
    } else if elevation >= -6.0 {
        SunPhase::Twilight
    } else if elevation >= -12.0 {
        SunPhase::NauticalTwilight
    } else if elevation >= -18.0 {
        SunPhase::AstronomicalTwilight
    } else {
        SunPhase::Night
    })
}

fn rounded_position(position: Position) -> Position {
    Position {
        elevation: (position.elevation * 100.0).round() / 100.0,
        azimuth: (position.azimuth * 100.0).round() / 100.0,
    }
}

fn format_utc(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::AutoSi, false)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SunLocation {
    observer: Observer,
    timezone: Tz,
}

fn sun_location(config: CoreConfig) -> Result<SunLocation, Error> {
    let timezone = config
        .time_zone
        .parse::<Tz>()
        .map_err(|_| Error::InvalidInput("Home Assistant timezone is not configured".into()))?;
    Ok(SunLocation {
        observer: Observer {
            latitude: config.latitude,
            longitude: config.longitude,
            elevation_m: config.elevation,
        },
        timezone,
    })
}

struct SunHostState {
    runtime: Runtime,
    registries: Registries,
    lifecycle: EntityLifecycle,
    entries: Weak<ConfigEntries>,
    handle: Handle,
    location: watch::Sender<SunLocation>,
    operation: Mutex<()>,
    runtimes: Mutex<HashMap<String, SunEntryRuntime>>,
    closed: AtomicBool,
}

impl SunHostState {
    fn lock_operation(&self) -> MutexGuard<'_, ()> {
        self.operation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_runtimes(&self) -> MutexGuard<'_, HashMap<String, SunEntryRuntime>> {
        self.runtimes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ensure_open(&self) -> Result<(), Error> {
        if self.closed.load(Ordering::Acquire) {
            Err(Error::Service("Sun integration is stopped".into()))
        } else {
            Ok(())
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn set_location(&self, location: SunLocation) {
        self.location.send_replace(location);
    }

    fn start_entry(self: &Arc<Self>, entry: &ConfigEntry) -> Result<SetupOutcome, Error> {
        self.ensure_open()?;
        if self.lock_runtimes().contains_key(&entry.entry_id) {
            return Ok(SetupOutcome::Ready);
        }

        let location = *self.location.borrow();
        let now = Utc::now();
        let snapshot = calculate_snapshot(location.observer, now, location.timezone)
            .map_err(|error| Error::InvalidInput(format!("Sun calculation failed: {error}")))?;

        remove_legacy_rising_entity(&self.runtime, &self.registries, &entry.entry_id)?;

        let device_preexisting = has_sun_device(&self.registries, &entry.entry_id);
        let device = match self.registries.get_or_create_device(DeviceRegistration {
            config_entry_id: Some(entry.entry_id.clone()),
            identifiers: BTreeSet::from([(DOMAIN.to_owned(), entry.entry_id.clone())]),
            name: Some(DEVICE_NAME.to_owned()),
            entry_type: Some("service".to_owned()),
            ..DeviceRegistration::default()
        }) {
            Ok(device) => device,
            Err(Error::InvalidInput(message)) => return Err(Error::InvalidInput(message)),
            Err(error) => {
                return Ok(SetupOutcome::RetryAfter {
                    seconds: retry_delay(entry.retry_attempts),
                    reason: Some(error.to_string()),
                });
            }
        };

        let specs = entity_specs(&entry.entry_id, &device.id);
        let handles = match self.lifecycle.register_batch(specs) {
            Ok(handles) => handles,
            Err(Error::InvalidInput(message)) => {
                return match remove_new_device(&self.registries, &device.id, device_preexisting) {
                    Ok(()) => Err(Error::InvalidInput(message)),
                    Err(cleanup) => Err(Error::Service(format!(
                        "Sun entity registration failed: {message}; device rollback failed: {cleanup}"
                    ))),
                };
            }
            Err(error) => {
                if let Err(cleanup) =
                    remove_new_device(&self.registries, &device.id, device_preexisting)
                {
                    return Err(Error::Service(format!(
                        "Sun entity registration failed: {error}; device rollback failed: {cleanup}"
                    )));
                }
                return Ok(SetupOutcome::RetryAfter {
                    seconds: retry_delay(entry.retry_attempts),
                    reason: Some(error.to_string()),
                });
            }
        };
        let entities = match SunEntities::from_handles(handles) {
            Ok(entities) => entities,
            Err(error) => {
                if let Err(cleanup) =
                    remove_new_device(&self.registries, &device.id, device_preexisting)
                {
                    return Err(Error::Service(format!(
                        "failed to create Sun entities: {error}; device rollback failed: {cleanup}"
                    )));
                }
                return Err(error);
            }
        };
        if entities.main.entity_id().as_deref() != Some(MAIN_ENTITY_ID) {
            let mut cleanup_errors = Vec::new();
            if let Err(error) = entities.release() {
                cleanup_errors.push(error.to_string());
            }
            if let Err(error) = remove_new_device(&self.registries, &device.id, device_preexisting)
            {
                cleanup_errors.push(error.to_string());
            }
            if !cleanup_errors.is_empty() {
                return Err(Error::Service(format!(
                    "fixed Sun entity ID {MAIN_ENTITY_ID} is unavailable; cleanup failed: {}",
                    cleanup_errors.join("; ")
                )));
            }
            return Err(Error::InvalidInput(format!(
                "fixed Sun entity ID {MAIN_ENTITY_ID} is already owned by another provider"
            )));
        }

        if let Err(error) = entities.publish_all(&snapshot) {
            let mut cleanup_errors = Vec::new();
            if let Err(cleanup) = entities.release() {
                cleanup_errors.push(cleanup.to_string());
            }
            if let Err(cleanup) =
                remove_new_device(&self.registries, &device.id, device_preexisting)
            {
                cleanup_errors.push(cleanup.to_string());
            }
            if !cleanup_errors.is_empty() {
                return Err(Error::Service(format!(
                    "Sun state publication failed: {error}; cleanup failed: {}",
                    cleanup_errors.join("; ")
                )));
            }
            return Ok(SetupOutcome::RetryAfter {
                seconds: retry_delay(entry.retry_attempts),
                reason: Some(error.to_string()),
            });
        }

        let (cancel, cancel_rx) = watch::channel(false);
        let location_rx = self.location.subscribe();
        let entities_for_task = entities.clone();
        let snapshot_store = Arc::new(Mutex::new(snapshot.clone()));
        let snapshot_store_for_task = Arc::clone(&snapshot_store);
        let task = self.handle.spawn(async move {
            sun_worker(
                location_rx,
                cancel_rx,
                entities_for_task,
                snapshot_store_for_task,
                location,
                snapshot,
            )
            .await
        });
        self.lock_runtimes().insert(
            entry.entry_id.clone(),
            SunEntryRuntime {
                entities,
                cancel,
                task: Some(task),
                snapshot: snapshot_store,
            },
        );
        Ok(SetupOutcome::Ready)
    }

    async fn stop_entry(&self, entry_id: &str) -> Result<(), Error> {
        let runtime = { self.lock_runtimes().remove(entry_id) };
        if let Some(runtime) = runtime {
            runtime.shutdown().await
        } else {
            Ok(())
        }
    }

    async fn shutdown_remaining(&self, errors: &mut Vec<String>) {
        let runtimes = std::mem::take(&mut *self.lock_runtimes());
        for (entry_id, runtime) in runtimes {
            if let Err(error) = runtime.shutdown().await {
                errors.push(format!("{entry_id}: {error}"));
            }
        }
    }
}

/// Weak access to the active native Sun runtime.
#[derive(Clone)]
pub struct SunManager {
    state: Weak<SunHostState>,
}

impl SunManager {
    /// Return the latest published calculation while the integration is active.
    #[must_use]
    pub fn snapshot(&self) -> Option<SunSnapshot> {
        let state = self.state.upgrade()?;
        let runtimes = state.lock_runtimes();
        let runtime = runtimes.values().next()?;
        Some(
            runtime
                .snapshot
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
    }

    /// Whether the owning integration host still accepts work.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.state
            .upgrade()
            .is_some_and(|state| !state.closed.load(Ordering::Acquire))
    }
}

/// Runtime host for the singleton Home Assistant Sun integration.
pub struct SunIntegration {
    entries: Arc<ConfigEntries>,
    state: Arc<SunHostState>,
    driver: Option<Arc<dyn ConfigEntryDriver>>,
    location_cancel: Option<watch::Sender<bool>>,
    location_task: Option<JoinHandle<Result<(), Error>>>,
    retry_cancel: Option<watch::Sender<bool>>,
    retry_task: Option<JoinHandle<()>>,
}

impl SunIntegration {
    /// Register the Sun config-entry driver, import YAML when requested, and
    /// restore its single persisted entry.
    pub async fn start(
        runtime: Runtime,
        registries: Registries,
        lifecycle: EntityLifecycle,
        entries: ConfigEntries,
        import_yaml: bool,
    ) -> Result<Self, Error> {
        let entries = Arc::new(entries);
        let mut events = runtime.subscribe();
        let location = sun_location(entries.core_config())?;
        let (location_tx, _location_rx) = watch::channel(location);
        let state = Arc::new(SunHostState {
            runtime: runtime.clone(),
            registries: registries.clone(),
            lifecycle,
            entries: Arc::downgrade(&entries),
            handle: Handle::current(),
            location: location_tx,
            operation: Mutex::new(()),
            runtimes: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        });
        let driver: Arc<dyn ConfigEntryDriver> = Arc::new(SunConfigEntryDriver {
            state: Arc::downgrade(&state),
            registries,
        });
        entries.register_driver(Arc::clone(&driver))?;

        let (location_cancel, mut location_rx) = watch::channel(false);
        let weak_state = Arc::downgrade(&state);
        let listener_entries = Arc::clone(&entries);
        let location_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = location_rx.changed() => {
                        if changed.is_err() || *location_rx.borrow() {
                            break;
                        }
                    }
                    event = events.recv() => match event {
                        Ok(event) if event.event_type == "core_config_updated" => {
                            let location = sun_location(listener_entries.core_config())?;
                            let Some(state) = weak_state.upgrade() else { break };
                            state.set_location(location);
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            let location = sun_location(listener_entries.core_config())?;
                            let Some(state) = weak_state.upgrade() else { break };
                            state.set_location(location);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
            Ok(())
        });

        let (retry_cancel, retry_rx) = watch::channel(false);
        let retry_entries = Arc::clone(&entries);
        let retry_task =
            tokio::spawn(async move { retry_scheduler(retry_entries, retry_rx).await });

        let integration = Self {
            entries,
            state,
            driver: Some(driver),
            location_cancel: Some(location_cancel),
            location_task: Some(location_task),
            retry_cancel: Some(retry_cancel),
            retry_task: Some(retry_task),
        };
        let restore_entries = Arc::clone(&integration.entries);
        let restore_state = Arc::clone(&integration.state);
        let restoration = tokio::task::spawn_blocking(move || {
            Self::restore_or_import(&restore_entries, &restore_state, import_yaml)
        })
        .await
        .unwrap_or_else(|error| Err(Error::Service(format!("Sun restore task failed: {error}"))));
        if let Err(error) = restoration {
            let cleanup_error = integration.shutdown().await.err();
            return Err(match cleanup_error {
                Some(cleanup) => Error::Service(format!(
                    "Sun startup failed: {error}; cleanup failed: {cleanup}"
                )),
                None => error,
            });
        }
        Ok(integration)
    }

    /// Return a weak manager handle for the active Sun snapshot.
    #[must_use]
    pub fn manager(&self) -> SunManager {
        SunManager {
            state: Arc::downgrade(&self.state),
        }
    }

    /// Stop listeners/retries, unload the entry, join workers, and unregister
    /// this host's exact config-entry driver.
    pub async fn shutdown(mut self) -> Result<(), Error> {
        self.state.close();
        if let Some(cancel) = self.location_cancel.take() {
            cancel.send_replace(true);
        }
        if let Some(cancel) = self.retry_cancel.take() {
            cancel.send_replace(true);
        }

        let mut errors = Vec::new();
        if let Some(task) = self.location_task.take() {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(format!("Sun location listener: {error}")),
                Err(error) => errors.push(format!("join Sun location listener: {error}")),
            }
        }
        if let Some(task) = self.retry_task.take()
            && let Err(error) = task.await
        {
            errors.push(format!("join Sun config retry task: {error}"));
        }

        let state = Arc::clone(&self.state);
        if let Err(error) = tokio::task::spawn_blocking(move || drop(state.lock_operation())).await
        {
            errors.push(format!("join Sun config-entry operations: {error}"));
        }

        let entries = Arc::clone(&self.entries);
        match tokio::task::spawn_blocking(move || {
            let mut unload_errors = Vec::new();
            for entry in entries.list_entries(Some(DOMAIN)) {
                if entry.state == ConfigEntryState::Loaded
                    && let Err(error) = entries.unload_entry(&entry.entry_id)
                {
                    unload_errors.push(format!("{}: {error}", entry.entry_id));
                }
            }
            unload_errors
        })
        .await
        {
            Ok(unload_errors) => errors.extend(unload_errors),
            Err(error) => errors.push(format!("join Sun entry unload: {error}")),
        }
        self.state.shutdown_remaining(&mut errors).await;
        if let Some(driver) = self.driver.take() {
            self.entries.unregister_driver(&driver);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::Service(format!(
                "Sun integration shutdown failed: {}",
                errors.join("; ")
            )))
        }
    }

    fn restore_or_import(
        entries: &ConfigEntries,
        state: &SunHostState,
        import_yaml: bool,
    ) -> Result<(), Error> {
        let existing = entries.list_entries(Some(DOMAIN));
        if existing.len() > 1 {
            return Err(Error::InvalidInput(
                "Sun integration supports one config entry".into(),
            ));
        }
        if existing.is_empty() {
            if !import_yaml {
                return Ok(());
            }
            let result = entries.init_config_flow_with_input(
                DOMAIN,
                "import",
                Map::new(),
                Some(Map::new()),
            )?;
            if result.get("type").and_then(Value::as_str) == Some("abort") {
                // A concurrent user flow may have created the singleton while
                // the import flow was being initialized. Accept it only if the
                // persisted entry now exists.
                if entries.list_entries(Some(DOMAIN)).is_empty() {
                    return Err(Error::InvalidInput(
                        "Sun YAML import was aborted without creating an entry".into(),
                    ));
                }
            }
        }

        for entry in entries.list_entries(Some(DOMAIN)) {
            if entry.disabled_by.is_some() {
                continue;
            }
            if entry.state == ConfigEntryState::Loaded {
                if !state.lock_runtimes().contains_key(&entry.entry_id) {
                    // A prior best-effort Drop may leave a loaded marker in
                    // the shared config manager after closing its provider.
                    entries.reload_entry(&entry.entry_id)?;
                }
                continue;
            }
            if entry.state == ConfigEntryState::SetupRetry {
                if entry
                    .retry_at
                    .is_some_and(|retry_at| retry_at <= Utc::now())
                {
                    entries.retry_entry(&entry.entry_id)?;
                }
                continue;
            }
            entries.setup_entry(&entry.entry_id)?;
        }
        Ok(())
    }
}

impl Drop for SunIntegration {
    fn drop(&mut self) {
        self.state.close();
        let runtimes: Vec<_> = self
            .state
            .lock_runtimes()
            .drain()
            .map(|(_, runtime)| runtime)
            .collect();
        for runtime in runtimes {
            runtime.cancel.send_replace(true);
            if let Some(task) = &runtime.task {
                task.abort();
            }
            let _ = runtime.entities.release();
        }
        if let Some(cancel) = self.location_cancel.take() {
            cancel.send_replace(true);
        }
        if let Some(cancel) = self.retry_cancel.take() {
            cancel.send_replace(true);
        }
        if let Some(task) = self.location_task.take() {
            task.abort();
        }
        if let Some(task) = self.retry_task.take() {
            task.abort();
        }
        if let Some(driver) = self.driver.take() {
            self.entries.unregister_driver(&driver);
        }
    }
}

struct SunConfigEntryDriver {
    state: Weak<SunHostState>,
    registries: Registries,
}

impl ConfigEntryDriver for SunConfigEntryDriver {
    fn domain(&self) -> &str {
        DOMAIN
    }

    fn integration_type(&self) -> &str {
        "service"
    }

    fn version(&self) -> (u32, u32) {
        (1, 1)
    }

    fn setup(&self, _runtime: &Runtime, entry: &ConfigEntry) -> Result<SetupOutcome, Error> {
        let Some(state) = self.state.upgrade() else {
            return Err(Error::Service(
                "Sun integration host is no longer active".into(),
            ));
        };
        let _operation = state.lock_operation();
        state.ensure_open()?;
        state.start_entry(entry)
    }

    fn unload(&self, _runtime: &Runtime, entry: &ConfigEntry) -> Result<(), Error> {
        let Some(state) = self.state.upgrade() else {
            return Ok(());
        };
        let _operation = state.lock_operation();
        state
            .handle
            .block_on(state.stop_entry(&entry.entry_id))
            .map_err(|error| {
                Error::Service(format!(
                    "failed to stop Sun runtime for config entry {}: {error}",
                    entry.entry_id
                ))
            })
    }

    fn config_flow(&self, request: &ConfigFlowRequest) -> Result<ConfigFlowResult, Error> {
        let Some(state) = self.state.upgrade() else {
            return Err(Error::Service(
                "Sun integration host is no longer active".into(),
            ));
        };
        state.ensure_open()?;
        let entries = state
            .entries
            .upgrade()
            .ok_or_else(|| Error::Service("Sun config-entry manager is no longer active".into()))?;
        if !entries.list_entries(Some(DOMAIN)).is_empty() {
            return Ok(ConfigFlowResult::Abort {
                reason: "single_instance_allowed".into(),
            });
        }
        if request.source == "import" || request.user_input.is_some() {
            return Ok(ConfigFlowResult::CreateEntry {
                title: DEVICE_NAME.into(),
                data: Map::new(),
                options: Map::new(),
                unique_id: None,
                version: 1,
                minor_version: 1,
            });
        }
        Ok(ConfigFlowResult::form("user", None))
    }

    fn remove(&self, runtime: &Runtime, entry: &ConfigEntry) -> Result<(), Error> {
        remove_entry_resources(runtime, &self.registries, entry)
    }
}

#[derive(Clone)]
struct SunEntities {
    main: EntityHandle,
    events: Vec<EntityHandle>,
    positions: Vec<EntityHandle>,
    rising: EntityHandle,
}

impl SunEntities {
    fn from_handles(handles: Vec<EntityHandle>) -> Result<Self, Error> {
        if handles.len() != 10 {
            for handle in &handles {
                let _ = handle.release();
            }
            return Err(Error::Service(format!(
                "Sun entity batch returned {} handles, expected 10",
                handles.len()
            )));
        }
        let mut handles = handles.into_iter();
        let main = handles.next().expect("validated main entity handle");
        let events = (0..6)
            .map(|_| handles.next().expect("validated event sensor handle"))
            .collect();
        let positions = (0..2)
            .map(|_| handles.next().expect("validated position sensor handle"))
            .collect();
        let rising = handles
            .next()
            .expect("validated rising binary sensor handle");
        Ok(Self {
            main,
            events,
            positions,
            rising,
        })
    }

    fn publish_all(&self, snapshot: &SunSnapshot) -> Result<(), Error> {
        self.publish_main(snapshot)?;
        self.publish_events(snapshot)?;
        self.publish_positions(snapshot)?;
        self.publish_rising(snapshot)
    }

    fn publish_main_and_positions(&self, snapshot: &SunSnapshot) -> Result<(), Error> {
        self.publish_main(snapshot)?;
        self.publish_positions(snapshot)
    }

    fn publish_main(&self, snapshot: &SunSnapshot) -> Result<(), Error> {
        let _ = self.main.publish(EntitySnapshot {
            state: Some(snapshot.state().into()),
            available: true,
            attributes: snapshot.attributes(),
            context: Context::default(),
        })?;
        Ok(())
    }

    fn publish_events(&self, snapshot: &SunSnapshot) -> Result<(), Error> {
        let values = [
            snapshot.next_dawn,
            snapshot.next_dusk,
            snapshot.next_midnight,
            snapshot.next_noon,
            snapshot.next_rising,
            snapshot.next_setting,
        ];
        for (handle, value) in self.events.iter().zip(values) {
            let _ = handle.publish(EntitySnapshot {
                state: Some(format_utc(value)),
                available: true,
                attributes: Map::new(),
                context: Context::default(),
            })?;
        }
        Ok(())
    }

    fn publish_positions(&self, snapshot: &SunSnapshot) -> Result<(), Error> {
        for (handle, value) in self
            .positions
            .iter()
            .zip([snapshot.solar_elevation, snapshot.solar_azimuth])
        {
            let _ = handle.publish(EntitySnapshot {
                state: Some(json!(value).to_string()),
                available: true,
                attributes: Map::new(),
                context: Context::default(),
            })?;
        }
        Ok(())
    }

    fn publish_rising(&self, snapshot: &SunSnapshot) -> Result<(), Error> {
        let _ = self.rising.publish(EntitySnapshot {
            state: Some(if snapshot.rising { "on" } else { "off" }.into()),
            available: true,
            attributes: Map::new(),
            context: Context::default(),
        })?;
        Ok(())
    }

    fn release(&self) -> Result<(), Error> {
        let mut errors = Vec::new();
        for handle in self
            .events
            .iter()
            .chain(self.positions.iter())
            .chain(std::iter::once(&self.rising))
            .chain(std::iter::once(&self.main))
        {
            if let Err(error) = handle.release() {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::Service(errors.join("; ")))
        }
    }
}

struct SunEntryRuntime {
    entities: SunEntities,
    cancel: watch::Sender<bool>,
    task: Option<JoinHandle<Result<(), Error>>>,
    snapshot: Arc<Mutex<SunSnapshot>>,
}

impl SunEntryRuntime {
    async fn shutdown(mut self) -> Result<(), Error> {
        self.cancel.send_replace(true);
        let mut errors = Vec::new();
        if let Some(task) = self.task.take() {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error.to_string()),
                Err(error) => errors.push(format!("join Sun entity worker: {error}")),
            }
        }
        if let Err(error) = self.entities.release() {
            errors.push(error.to_string());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::Service(errors.join("; ")))
        }
    }
}

impl Drop for SunEntryRuntime {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn next_entity_enabled(
    receivers: &mut [watch::Receiver<hass_entity::EntityLifecycleChange>],
    was_active: &mut [bool],
) -> Option<bool> {
    let futures: Vec<_> = receivers
        .iter_mut()
        .map(|receiver| Box::pin(receiver.changed()))
        .collect();
    let (changed, index, pending) = futures_util::future::select_all(futures).await;
    drop(pending);
    changed.ok()?;
    // A later registry update can coalesce away the Enabled reason.
    let active = matches!(
        &receivers[index].borrow_and_update().status,
        hass_entity::EntityStatus::Active { .. }
    );
    let enabled = active && !was_active[index];
    was_active[index] = active;
    Some(enabled)
}

async fn sun_worker(
    mut location: watch::Receiver<SunLocation>,
    mut cancel: watch::Receiver<bool>,
    entities: SunEntities,
    snapshot_store: Arc<Mutex<SunSnapshot>>,
    initial_location: SunLocation,
    initial: SunSnapshot,
) -> Result<(), Error> {
    let mut snapshot = initial;
    let mut lifecycle_changes: Vec<_> = std::iter::once(&entities.main)
        .chain(entities.events.iter())
        .chain(entities.positions.iter())
        .chain(std::iter::once(&entities.rising))
        .map(EntityHandle::changes)
        .collect();
    let mut was_active: Vec<bool> = lifecycle_changes
        .iter()
        .map(|receiver| {
            matches!(
                &receiver.borrow().status,
                hass_entity::EntityStatus::Active { .. }
            )
        })
        .collect();
    let mut current_location = initial_location;
    let latest_location = *location.borrow();
    if latest_location != current_location {
        current_location = latest_location;
        snapshot = calculate_snapshot(
            current_location.observer,
            Utc::now(),
            current_location.timezone,
        )
        .map_err(|error| Error::Service(format!("Sun location recalculation failed: {error}")))?;
        entities.publish_all(&snapshot)?;
        *snapshot_store
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = snapshot.clone();
    }
    // Close the startup window when enabling predates receiver installation.
    entities.publish_all(&snapshot)?;
    let mut position_at = next_position_deadline(&snapshot, Utc::now());
    loop {
        if *cancel.borrow() {
            break;
        }

        let now = Utc::now();
        let wake_at = position_at.map_or(snapshot.next_change, |position| {
            position.min(snapshot.next_change)
        });
        let wait = wake_at
            .signed_duration_since(now)
            .to_std()
            .unwrap_or(StdDuration::ZERO);
        let enabled = tokio::select! {
            enabled = next_entity_enabled(&mut lifecycle_changes, &mut was_active) => {
                let Some(enabled) = enabled else { break; };
                enabled
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    break;
                }
                false
            }
            changed = location.changed() => {
                if changed.is_err() {
                    break;
                }
                let new_location = *location.borrow_and_update();
                if new_location != current_location {
                    current_location = new_location;
                    snapshot = calculate_snapshot(new_location.observer, Utc::now(), new_location.timezone)
                        .map_err(|error| Error::Service(format!("Sun location recalculation failed: {error}")))?;
                    entities.publish_all(&snapshot)?;
                    *snapshot_store.lock().unwrap_or_else(PoisonError::into_inner) = snapshot.clone();
                    position_at = next_position_deadline(&snapshot, Utc::now());
                }
                false
            }
            _ = time::sleep(wait) => {
                let now = Utc::now();
                if now >= snapshot.next_change {
                    snapshot = calculate_snapshot(current_location.observer, now, current_location.timezone)
                        .map_err(|error| Error::Service(format!("Sun event recalculation failed: {error}")))?;
                    entities.publish_all(&snapshot)?;
                    *snapshot_store.lock().unwrap_or_else(PoisonError::into_inner) = snapshot.clone();
                    position_at = next_position_deadline(&snapshot, now);
                } else if position_at.is_some_and(|deadline| now >= deadline) {
                    let position = rounded_position(position_at_fn(current_location.observer, now)?);
                    snapshot.solar_elevation = position.elevation;
                    snapshot.solar_azimuth = position.azimuth;
                    entities.publish_main_and_positions(&snapshot)?;
                    *snapshot_store.lock().unwrap_or_else(PoisonError::into_inner) = snapshot.clone();
                    position_at = next_position_deadline(&snapshot, now);
                }
                false
            }
        };
        if enabled {
            let now = Utc::now();
            snapshot =
                calculate_snapshot(current_location.observer, now, current_location.timezone)
                    .map_err(|error| {
                        Error::Service(format!("Sun enable recalculation failed: {error}"))
                    })?;
            entities.publish_all(&snapshot)?;
            *snapshot_store
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = snapshot.clone();
            position_at = next_position_deadline(&snapshot, now);
        }
    }
    Ok(())
}

fn position_at_fn(observer: Observer, at: DateTime<Utc>) -> Result<Position, Error> {
    position_at(observer, at)
        .map_err(|error| Error::Service(format!("Sun position calculation failed: {error}")))
}

fn next_position_deadline(snapshot: &SunSnapshot, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let interval = snapshot.phase.position_interval();
    let next = now.checked_add_signed(interval)?;
    let suppression_window = Duration::seconds(interval.num_seconds() * 5 / 4);
    let last_safe_update = now.checked_add_signed(suppression_window)?;
    (last_safe_update <= snapshot.next_change).then_some(next)
}

fn entity_specs(entry_id: &str, device_id: &str) -> Vec<EntitySpec> {
    let provider_id = format!("sun:{entry_id}");
    let main = EntitySpec::new(DOMAIN, DOMAIN, MAIN_PROVIDER_ID, MAIN_PROVIDER_KEY)
        .with_suggested_object_id("sun")
        .with_info(ProviderEntityInfo {
            original_name: Some("Sun".into()),
            ..ProviderEntityInfo::default()
        })
        .with_unrecorded_attributes(SUN_ATTRIBUTE_NAMES);

    let mut specs = vec![main];
    for description in EVENT_SENSOR_DESCRIPTIONS {
        specs.push(sun_child_spec(
            entry_id,
            device_id,
            &provider_id,
            description,
        ));
    }
    for description in [
        SunChildDescription {
            domain: "sensor",
            key: "solar_elevation",
            suggested_object_id: "sun_solar_elevation",
            device_class: None,
            state_class: Some("measurement"),
            unit_of_measurement: Some("°"),
            enabled_by_default: false,
        },
        SunChildDescription {
            domain: "sensor",
            key: "solar_azimuth",
            suggested_object_id: "sun_solar_azimuth",
            device_class: None,
            state_class: Some("measurement"),
            unit_of_measurement: Some("°"),
            enabled_by_default: false,
        },
        SunChildDescription {
            domain: "binary_sensor",
            key: "solar_rising",
            suggested_object_id: "sun_solar_rising",
            device_class: None,
            state_class: None,
            unit_of_measurement: None,
            enabled_by_default: false,
        },
    ] {
        specs.push(sun_child_spec(
            entry_id,
            device_id,
            &provider_id,
            description,
        ));
    }
    specs
}

#[derive(Clone, Copy)]
struct SunChildDescription {
    domain: &'static str,
    key: &'static str,
    suggested_object_id: &'static str,
    device_class: Option<&'static str>,
    state_class: Option<&'static str>,
    unit_of_measurement: Option<&'static str>,
    enabled_by_default: bool,
}

const EVENT_SENSOR_DESCRIPTIONS: [SunChildDescription; 6] = [
    SunChildDescription {
        domain: "sensor",
        key: "next_dawn",
        suggested_object_id: "sun_next_dawn",
        device_class: Some("timestamp"),
        state_class: None,
        unit_of_measurement: None,
        enabled_by_default: true,
    },
    SunChildDescription {
        domain: "sensor",
        key: "next_dusk",
        suggested_object_id: "sun_next_dusk",
        device_class: Some("timestamp"),
        state_class: None,
        unit_of_measurement: None,
        enabled_by_default: true,
    },
    SunChildDescription {
        domain: "sensor",
        key: "next_midnight",
        suggested_object_id: "sun_next_midnight",
        device_class: Some("timestamp"),
        state_class: None,
        unit_of_measurement: None,
        enabled_by_default: true,
    },
    SunChildDescription {
        domain: "sensor",
        key: "next_noon",
        suggested_object_id: "sun_next_noon",
        device_class: Some("timestamp"),
        state_class: None,
        unit_of_measurement: None,
        enabled_by_default: true,
    },
    SunChildDescription {
        domain: "sensor",
        key: "next_rising",
        suggested_object_id: "sun_next_rising",
        device_class: Some("timestamp"),
        state_class: None,
        unit_of_measurement: None,
        enabled_by_default: true,
    },
    SunChildDescription {
        domain: "sensor",
        key: "next_setting",
        suggested_object_id: "sun_next_setting",
        device_class: Some("timestamp"),
        state_class: None,
        unit_of_measurement: None,
        enabled_by_default: true,
    },
];

fn sun_child_spec(
    entry_id: &str,
    device_id: &str,
    provider_id: &str,
    description: SunChildDescription,
) -> EntitySpec {
    let entity_name = match description.key {
        "next_dawn" => "Next dawn",
        "next_dusk" => "Next dusk",
        "next_midnight" => "Next midnight",
        "next_noon" => "Next noon",
        "next_rising" => "Next rising",
        "next_setting" => "Next setting",
        "solar_elevation" => "Solar elevation",
        "solar_azimuth" => "Solar azimuth",
        "solar_rising" => "Solar rising",
        _ => description.key,
    };
    EntitySpec::new(description.domain, DOMAIN, provider_id, description.key)
        .with_unique_id(format!("{entry_id}-{}", description.key))
        .with_config_entry_id(entry_id)
        .with_device_id(device_id)
        .with_suggested_object_id(description.suggested_object_id)
        .with_info(ProviderEntityInfo {
            original_name: Some(entity_name.into()),
            original_device_class: description.device_class.map(str::to_owned),
            entity_category: Some("diagnostic".into()),
            has_entity_name: true,
            unit_of_measurement: description.unit_of_measurement.map(str::to_owned),
            translation_key: Some(description.key.into()),
            capabilities: description
                .state_class
                .map(|state_class| json!({"state_class": state_class})),
            ..ProviderEntityInfo::default()
        })
        .with_enabled_by_default(description.enabled_by_default)
}

fn has_sun_device(registries: &Registries, entry_id: &str) -> bool {
    registries.devices().iter().any(|device| {
        device
            .identifiers
            .contains(&(DOMAIN.to_owned(), entry_id.to_owned()))
    })
}

fn remove_new_device(
    registries: &Registries,
    device_id: &str,
    preexisting: bool,
) -> Result<(), Error> {
    if !preexisting {
        registries.remove_device(device_id)?;
    }
    Ok(())
}

fn remove_legacy_rising_entity(
    runtime: &Runtime,
    registries: &Registries,
    entry_id: &str,
) -> Result<(), Error> {
    let unique_id = format!("{entry_id}-solar_rising");
    for entity in registries.entities().into_iter().filter(|entity| {
        entity.domain == "sensor" && entity.platform == DOMAIN && entity.unique_id == unique_id
    }) {
        let observed = runtime.get_state(&entity.entity_id);
        registries.remove_entity(&entity.entity_id)?;
        if let Some(observed) = observed {
            runtime.remove_state_if_unchanged(&observed, observed.context.clone())?;
        }
    }
    Ok(())
}

fn remove_entry_resources(
    runtime: &Runtime,
    registries: &Registries,
    entry: &ConfigEntry,
) -> Result<(), Error> {
    let mut errors = Vec::new();
    for entity in registries.entities().into_iter().filter(|entity| {
        entity.config_entry_id.as_deref() == Some(entry.entry_id.as_str())
            || (entity.entity_id == MAIN_ENTITY_ID && entity.platform == DOMAIN)
    }) {
        let observed = runtime.get_state(&entity.entity_id);
        if let Err(error) = registries.remove_entity(&entity.entity_id) {
            errors.push(format!("remove entity {}: {error}", entity.entity_id));
            continue;
        }
        if let Some(observed) = observed
            && let Err(error) =
                runtime.remove_state_if_unchanged(&observed, observed.context.clone())
        {
            errors.push(format!("remove state {}: {error}", entity.entity_id));
        }
    }
    for device in registries.devices().into_iter().filter(|device| {
        device.config_entry_id.as_deref() == Some(entry.entry_id.as_str())
            || device.config_entries.iter().any(|id| id == &entry.entry_id)
    }) {
        if device
            .config_entries
            .iter()
            .all(|config_entry_id| config_entry_id == &entry.entry_id)
            && let Err(error) = registries.remove_device(&device.id)
        {
            errors.push(format!("remove device {}: {error}", device.id));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Service(errors.join("; ")))
    }
}

fn retry_delay(attempts: u32) -> u64 {
    RETRY_BASE_SECONDS
        .saturating_mul(1_u64 << attempts.min(6))
        .min(RETRY_MAX_SECONDS)
}

async fn retry_scheduler(entries: Arc<ConfigEntries>, mut stop: watch::Receiver<bool>) {
    let mut interval = time::interval(RETRY_POLL_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
            _ = interval.tick() => {
                let entries = Arc::clone(&entries);
                let _ = tokio::task::spawn_blocking(move || {
                    for entry in entries.list_entries(Some(DOMAIN)) {
                        if entry.state == ConfigEntryState::SetupRetry
                            && entry.retry_at.is_some_and(|retry_at| retry_at <= Utc::now())
                        {
                            let _ = entries.retry_entry(&entry.entry_id);
                        }
                    }
                }).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration as StdDuration;

    use chrono::{DateTime, Duration, TimeZone, Utc};
    use hass_config::{
        ConfigEntryDriver, ConfigEntryState, ConfigFlowRequest, ConfigFlowResult, NewConfigEntry,
        SetupOutcome,
    };
    use hass_core::{MemoryStore, Runtime};
    use hass_entity::EntityLifecycle;
    use hass_registry::{EntityRegistration, EntityUpdate, Registries};
    use serde_json::json;

    use super::*;

    fn managers() -> (Runtime, Registries, EntityLifecycle, ConfigEntries) {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        (runtime, registries, lifecycle, entries)
    }

    async fn start(
        runtime: Runtime,
        registries: Registries,
        lifecycle: EntityLifecycle,
        entries: ConfigEntries,
        import_yaml: bool,
    ) -> Result<SunIntegration, Error> {
        SunIntegration::start(runtime, registries, lifecycle, entries, import_yaml).await
    }

    #[tokio::test]
    async fn imports_singleton_entities_and_excludes_all_main_attributes_from_history() {
        let (runtime, registries, lifecycle, entries) = managers();
        let host = start(
            runtime.clone(),
            registries.clone(),
            lifecycle,
            entries.clone(),
            true,
        )
        .await
        .unwrap();
        let manager = host.manager();

        let config_entries = entries.list_entries(Some(DOMAIN));
        assert_eq!(config_entries.len(), 1);
        assert_eq!(config_entries[0].unique_id, None);
        assert_eq!(config_entries[0].integration_type, "service");
        assert_eq!(config_entries[0].state, ConfigEntryState::Loaded);

        let registered_entities = registries.entities();
        let entity_ids: Vec<_> = registered_entities
            .iter()
            .map(|entity| entity.entity_id.as_str())
            .collect();
        assert_eq!(entity_ids.len(), 9);
        for entity_id in [
            "sensor.sun_next_dawn",
            "sensor.sun_next_dusk",
            "sensor.sun_next_midnight",
            "sensor.sun_next_noon",
            "sensor.sun_next_rising",
            "sensor.sun_next_setting",
            "sensor.sun_solar_elevation",
            "sensor.sun_solar_azimuth",
            "binary_sensor.sun_solar_rising",
        ] {
            assert!(entity_ids.contains(&entity_id), "missing {entity_id}");
        }

        let main_state = runtime.get_state(MAIN_ENTITY_ID).unwrap();
        assert_eq!(main_state.attributes.len(), SUN_ATTRIBUTE_NAMES.len() + 1);
        assert_eq!(main_state.attributes["friendly_name"], "Sun");
        for attribute in SUN_ATTRIBUTE_NAMES {
            assert!(main_state.attributes.contains_key(attribute));
        }
        let history = runtime
            .history(
                MAIN_ENTITY_ID,
                DateTime::<Utc>::MIN_UTC,
                DateTime::<Utc>::MAX_UTC,
            )
            .unwrap();
        assert_eq!(history.len(), 1);
        for attribute in SUN_ATTRIBUTE_NAMES {
            assert!(!history[0].attributes.contains_key(attribute));
        }

        let entry_id = &config_entries[0].entry_id;
        let device = registries.devices().pop().unwrap();
        assert_eq!(device.name.as_deref(), Some("Sun"));
        assert_eq!(device.entry_type.as_deref(), Some("service"));
        assert!(
            device
                .identifiers
                .contains(&(DOMAIN.to_owned(), entry_id.clone()))
        );
        for entity_id in entity_ids
            .iter()
            .copied()
            .filter(|entity_id| *entity_id != MAIN_ENTITY_ID)
        {
            let entity = registries.entity(entity_id).unwrap();
            assert_eq!(entity.config_entry_id.as_deref(), Some(entry_id.as_str()));
            assert_eq!(entity.device_id.as_deref(), Some(device.id.as_str()));
            assert_eq!(entity.platform, DOMAIN);
        }
        for entity_id in [
            "sensor.sun_solar_elevation",
            "sensor.sun_solar_azimuth",
            "binary_sensor.sun_solar_rising",
        ] {
            assert_eq!(
                registries.entity(entity_id).unwrap().disabled_by.as_deref(),
                Some("integration")
            );
        }
        for entity_id in ["sensor.sun_solar_elevation", "sensor.sun_solar_azimuth"] {
            let entity = registries.entity(entity_id).unwrap();
            assert_eq!(entity.unit_of_measurement.as_deref(), Some("°"));
            assert_eq!(
                entity.capabilities,
                Some(json!({"state_class": "measurement"}))
            );
            let handle = {
                let runtimes = host.state.lock_runtimes();
                let runtime = runtimes.values().next().unwrap();
                let index = usize::from(entity_id.ends_with("azimuth"));
                runtime.entities.positions[index].clone()
            };
            let snapshot = manager.snapshot().unwrap();
            let value = if entity_id.ends_with("azimuth") {
                snapshot.solar_azimuth
            } else {
                snapshot.solar_elevation
            };
            registries
                .update_entity(
                    entity_id,
                    EntityUpdate {
                        disabled_by: Some(None),
                        ..EntityUpdate::default()
                    },
                )
                .unwrap();
            handle
                .publish(EntitySnapshot::new(json!(value).to_string()))
                .unwrap()
                .expect("enabled Sun position sensor should accept a state publication");
            assert_eq!(
                runtime.get_state(entity_id).unwrap().attributes["state_class"],
                "measurement"
            );
        }
        assert!(manager.snapshot().is_some());

        host.shutdown().await.unwrap();
        assert!(!manager.is_running());
        assert!(runtime.get_state(MAIN_ENTITY_ID).is_none());
        assert!(entries.list_entries(Some(DOMAIN))[0].state != ConfigEntryState::Loaded);
    }

    #[test]
    fn snapshot_matches_pinned_event_times_phase_and_horizon_threshold() {
        let observer = Observer {
            latitude: 32.87336,
            longitude: -117.22743,
            elevation_m: 0.0,
        };
        let now = Utc.with_ymd_and_hms(2015, 9, 15, 1, 0, 0).single().unwrap();
        let mut snapshot =
            SunSnapshot::calculate(observer, now, chrono_tz::America::Los_Angeles).unwrap();
        let parse_utc = |value: &str| DateTime::parse_from_rfc3339(value).unwrap().to_utc();

        assert_eq!(snapshot.next_dawn, parse_utc("2015-09-15T13:07:26.084892Z"));
        assert_eq!(snapshot.next_dusk, parse_utc("2015-09-15T02:21:39.056429Z"));
        assert!(snapshot.next_change > now);
        assert_eq!(snapshot.phase, SunPhase::Day);
        assert_eq!(snapshot.attributes().len(), SUN_ATTRIBUTE_NAMES.len());
        assert_eq!(snapshot.state(), "above_horizon");

        snapshot.solar_elevation = -0.833;
        assert_eq!(snapshot.state(), "below_horizon");
        snapshot.solar_elevation = -0.832;
        assert_eq!(snapshot.state(), "above_horizon");
    }

    #[test]
    fn position_polling_intervals_and_event_suppression_follow_phase() {
        let now = Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).single().unwrap();
        assert_eq!(SunPhase::Night.position_interval(), Duration::minutes(20));
        assert_eq!(
            SunPhase::AstronomicalTwilight.position_interval(),
            Duration::minutes(8)
        );
        assert_eq!(
            SunPhase::NauticalTwilight.position_interval(),
            Duration::minutes(4)
        );
        assert_eq!(SunPhase::Twilight.position_interval(), Duration::minutes(2));
        assert_eq!(SunPhase::SmallDay.position_interval(), Duration::minutes(2));
        assert_eq!(SunPhase::Day.position_interval(), Duration::minutes(4));

        let mut snapshot = SunSnapshot {
            next_dawn: now,
            next_dusk: now,
            next_midnight: now,
            next_noon: now,
            next_rising: now,
            next_setting: now,
            solar_elevation: 0.0,
            solar_azimuth: 0.0,
            rising: false,
            next_change: now + Duration::minutes(24),
            phase: SunPhase::Night,
        };
        assert_eq!(next_position_deadline(&snapshot, now), None);
        snapshot.next_change = now + Duration::minutes(25);
        assert_eq!(
            next_position_deadline(&snapshot, now),
            Some(now + Duration::minutes(20))
        );
    }

    #[tokio::test]
    async fn no_import_keeps_the_host_live_without_creating_an_entry() {
        let (runtime, registries, lifecycle, entries) = managers();
        let host = SunIntegration::start(
            runtime,
            registries.clone(),
            lifecycle,
            entries.clone(),
            false,
        )
        .await
        .unwrap();
        let manager = host.manager();
        assert!(manager.is_running());
        assert!(manager.snapshot().is_none());
        assert!(entries.list_entries(Some(DOMAIN)).is_empty());
        assert!(registries.entities().is_empty());

        let started = entries
            .init_config_flow(DOMAIN, "user", Map::new())
            .unwrap();
        assert_eq!(started["type"], "form");
        assert_eq!(started["step_id"], "user");
        assert!(started["data_schema"].is_null());
        let completed = entries
            .submit_flow(started["flow_id"].as_str().unwrap(), Map::new())
            .unwrap();
        assert_eq!(completed["type"], "create_entry");
        let created = entries.list_entries(Some(DOMAIN));
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].title, "Sun");
        assert_eq!(created[0].unique_id, None);
        assert!(created[0].data.is_empty());
        assert!(created[0].options.is_empty());
        assert!(manager.snapshot().is_some());
        let duplicate = entries
            .init_config_flow(DOMAIN, "user", Map::new())
            .unwrap();
        assert_eq!(duplicate["type"], "abort");
        assert_eq!(duplicate["reason"], "single_instance_allowed");

        host.shutdown().await.unwrap();
        assert!(!manager.is_running());
    }

    #[tokio::test]
    async fn setup_removes_the_deprecated_solar_rising_sensor_identity() {
        let (runtime, registries, lifecycle, entries) = managers();
        let entry = entries
            .create_entry(NewConfigEntry::new(DOMAIN, "Sun", serde_json::Map::new()))
            .unwrap();
        let mut registration =
            EntityRegistration::new("sensor", DOMAIN, format!("{}-solar_rising", entry.entry_id));
        registration.suggested_object_id = Some("sun_solar_rising".into());
        registration.config_entry_id = Some(entry.entry_id.clone());
        let legacy = registries.get_or_create_entity(registration).unwrap();

        let host = SunIntegration::start(runtime, registries.clone(), lifecycle, entries, false)
            .await
            .unwrap();
        assert!(registries.entity(&legacy.entity_id).is_none());
        assert!(
            registries
                .entity("binary_sensor.sun_solar_rising")
                .is_some()
        );
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn core_location_and_timezone_changes_refresh_active_sun_entities() {
        let (runtime, registries, lifecycle, entries) = managers();
        let host = SunIntegration::start(
            runtime.clone(),
            registries.clone(),
            lifecycle,
            entries.clone(),
            true,
        )
        .await
        .unwrap();
        let manager = host.manager();
        let initial = manager.snapshot().unwrap();
        let mut config = entries.core_config();
        config.latitude = 69.6;
        config.longitude = 18.8;
        config.time_zone = "Europe/Oslo".into();
        entries.update_core_config(config).unwrap();

        let updated = tokio::time::timeout(StdDuration::from_secs(2), async {
            loop {
                let snapshot = manager.snapshot().unwrap();
                if snapshot.solar_elevation != initial.solar_elevation {
                    break snapshot;
                }
                tokio::time::sleep(StdDuration::from_millis(5)).await;
            }
        })
        .await
        .expect("Sun worker should apply core location updates");
        assert_ne!(updated.next_rising, initial.next_rising);
        assert_eq!(
            runtime.get_state(MAIN_ENTITY_ID).unwrap().attributes["elevation"],
            json!(updated.solar_elevation)
        );
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn enabling_fresh_default_disabled_entities_publishes_without_waiting_for_a_solar_tick() {
        let (runtime, registries, lifecycle, entries) = managers();
        let host = SunIntegration::start(
            runtime.clone(),
            registries.clone(),
            lifecycle.clone(),
            entries,
            true,
        )
        .await
        .unwrap();
        for entity_id in [
            "sensor.sun_solar_elevation",
            "sensor.sun_solar_azimuth",
            "binary_sensor.sun_solar_rising",
        ] {
            assert!(runtime.get_state(entity_id).is_none());
            registries
                .update_entity(
                    entity_id,
                    EntityUpdate {
                        disabled_by: Some(None),
                        ..EntityUpdate::default()
                    },
                )
                .unwrap();
            let state = tokio::time::timeout(StdDuration::from_secs(2), async {
                loop {
                    if let Some(state) = runtime.get_state(entity_id) {
                        break state;
                    }
                    tokio::time::sleep(StdDuration::from_millis(5)).await;
                }
            })
            .await
            .expect(
                "enable must publish immediately rather than waiting minutes for the phase timer",
            );
            assert!(!matches!(state.state.as_str(), "unknown" | "unavailable"));
            if entity_id.starts_with("sensor.") {
                assert_eq!(state.attributes["state_class"], "measurement");
            }
        }
        host.shutdown().await.unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dropped_loaded_host_can_restart_with_the_same_shared_managers() {
        let (runtime, registries, lifecycle, entries) = managers();
        let first = SunIntegration::start(
            runtime.clone(),
            registries.clone(),
            lifecycle.clone(),
            entries.clone(),
            true,
        )
        .await
        .unwrap();
        let entry_id = entries.list_entries(Some(DOMAIN))[0].entry_id.clone();
        let manager = first.manager();
        drop(first);
        assert!(!manager.is_running());
        let second = SunIntegration::start(
            runtime.clone(),
            registries,
            lifecycle.clone(),
            entries.clone(),
            false,
        )
        .await
        .unwrap();
        assert!(second.manager().snapshot().is_some());
        assert_eq!(
            entries.get_entry(&entry_id).unwrap().state,
            ConfigEntryState::Loaded
        );
        assert!(runtime.get_state(MAIN_ENTITY_ID).is_some());
        second.shutdown().await.unwrap();
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restart_preserves_user_enabled_position_entities() {
        let (runtime, registries, lifecycle, entries) = managers();
        let first = SunIntegration::start(
            runtime.clone(),
            registries.clone(),
            lifecycle.clone(),
            entries.clone(),
            true,
        )
        .await
        .unwrap();
        let entity_id = "sensor.sun_solar_elevation";
        let prior_id = registries.entity(entity_id).unwrap().entity_id;
        registries
            .update_entity(
                entity_id,
                EntityUpdate {
                    disabled_by: Some(None),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        first.shutdown().await.unwrap();

        let second = SunIntegration::start(runtime, registries.clone(), lifecycle, entries, false)
            .await
            .unwrap();
        let restored = registries.entity(entity_id).unwrap();
        assert_eq!(restored.entity_id, prior_id);
        assert_eq!(restored.disabled_by, None);
        assert!(second.manager().snapshot().is_some());
        second.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn removing_entry_releases_sun_entities_and_service_device() {
        let (runtime, registries, lifecycle, entries) = managers();
        let host = SunIntegration::start(
            runtime.clone(),
            registries.clone(),
            lifecycle,
            entries.clone(),
            true,
        )
        .await
        .unwrap();
        let entry_id = entries.list_entries(Some(DOMAIN))[0].entry_id.clone();
        let entries_for_remove = entries.clone();
        tokio::task::spawn_blocking(move || entries_for_remove.remove_entry(&entry_id))
            .await
            .unwrap()
            .unwrap();

        assert!(entries.list_entries(Some(DOMAIN)).is_empty());
        assert!(registries.devices().is_empty());
        assert!(
            registries.entities().iter().all(
                |entity| entity.entity_id != MAIN_ENTITY_ID && entity.config_entry_id.is_none()
            )
        );
        for entity_id in [
            MAIN_ENTITY_ID,
            "sensor.sun_next_dawn",
            "sensor.sun_solar_elevation",
            "binary_sensor.sun_solar_rising",
        ] {
            assert!(runtime.get_state(entity_id).is_none());
        }
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_singleton_restore_unregisters_its_driver() {
        let (runtime, registries, lifecycle, entries) = managers();
        for _ in 0..2 {
            entries
                .create_entry(NewConfigEntry::new(DOMAIN, "Sun", serde_json::Map::new()))
                .unwrap();
        }

        let result =
            SunIntegration::start(runtime, registries, lifecycle, entries.clone(), false).await;
        assert!(result.is_err());
        assert!(
            entries
                .list_entries(Some(DOMAIN))
                .iter()
                .all(|entry| { entry.state != ConfigEntryState::Loaded })
        );
        assert!(entries.register_driver(Arc::new(ProbeSunDriver)).is_ok());
    }

    struct ProbeSunDriver;

    impl ConfigEntryDriver for ProbeSunDriver {
        fn domain(&self) -> &str {
            DOMAIN
        }

        fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<SetupOutcome, Error> {
            Ok(SetupOutcome::Ready)
        }

        fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<(), Error> {
            Ok(())
        }

        fn config_flow(&self, _request: &ConfigFlowRequest) -> Result<ConfigFlowResult, Error> {
            Ok(ConfigFlowResult::Abort {
                reason: "test".into(),
            })
        }
    }
}
