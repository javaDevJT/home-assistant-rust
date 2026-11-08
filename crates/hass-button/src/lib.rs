//! Typed descriptions, validation, and state projection for native `button`
//! entities.
//!
//! This is a source-pinned SDK layer, not a platform integration. A provider
//! supplies a [`ButtonPressHandler`] with its real action. The generic
//! [`execute_button_press`] wrapper records the press timestamp first and then
//! invokes that handler; it does not invent a successful no-op action.

use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

use chrono::{DateTime, Timelike, Utc};
use hass_core::{Context, ServiceCall};
use hass_entity::{EntityHandle, EntityPublication, EntitySnapshot, EntitySpec};
use serde_json::{Map, Value};

/// Home Assistant entity domain for native buttons.
pub const BUTTON_DOMAIN: &str = "button";
/// The standard button service.
pub const BUTTON_SERVICE_PRESS: &str = "press";
/// Upstream component scan interval in seconds. Button entities themselves do
/// not poll; the interval is component-level metadata only.
pub const BUTTON_SCAN_INTERVAL_SECONDS: u64 = 30;
/// Upstream minimum scan interval in seconds. Button entities themselves do
/// not poll; the interval is component-level metadata only.
pub const BUTTON_MIN_TIME_BETWEEN_SCANS_SECONDS: u64 = 10;
/// Upstream button entities set `should_poll` to false.
pub const BUTTON_SHOULD_POLL: bool = false;

/// Device classes exposed by the pinned Home Assistant button component.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ButtonDeviceClass {
    /// An identification action.
    Identify,
    /// A restart action.
    Restart,
    /// An update action.
    Update,
}

impl ButtonDeviceClass {
    /// All device classes in upstream enum order.
    pub const ALL: [Self; 3] = [Self::Identify, Self::Restart, Self::Update];

    /// Return the Home Assistant device-class string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Identify => "identify",
            Self::Restart => "restart",
            Self::Update => "update",
        }
    }
}

impl fmt::Display for ButtonDeviceClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ButtonDeviceClass {
    type Err = UnknownButtonDeviceClass;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|class| class.as_str() == normalized)
            .ok_or_else(|| UnknownButtonDeviceClass(value.to_owned()))
    }
}

impl TryFrom<&str> for ButtonDeviceClass {
    type Error = UnknownButtonDeviceClass;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// Parse failure for an unsupported button device class.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownButtonDeviceClass(String);

impl fmt::Display for UnknownButtonDeviceClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown button device class {:?}", self.0)
    }
}

impl StdError for UnknownButtonDeviceClass {}

/// Provider description for one `button` registry entity.
///
/// The class is typed here and projected into shared provider metadata. The
/// shared lifecycle still owns registry identity, user overrides, lifecycle
/// changes, and the actual state write.
#[derive(Clone, Debug)]
pub struct ButtonDescriptor {
    spec: EntitySpec,
    device_class: Option<ButtonDeviceClass>,
}

impl ButtonDescriptor {
    /// Create a button descriptor with stable provider identity.
    #[must_use]
    pub fn new(
        platform: impl Into<String>,
        provider_id: impl Into<String>,
        provider_key: impl Into<String>,
    ) -> Self {
        Self {
            spec: EntitySpec::new(BUTTON_DOMAIN, platform, provider_id, provider_key),
            device_class: None,
        }
    }

    /// Return the provider's currently described device class, if any.
    #[must_use]
    pub const fn device_class(&self) -> Option<ButtonDeviceClass> {
        self.device_class
    }

    /// Button entities are event-driven and do not poll.
    #[must_use]
    pub const fn should_poll(&self) -> bool {
        BUTTON_SHOULD_POLL
    }

    /// Home Assistant may use a button's device class for its default name.
    /// Localization and UI naming remain the responsibility of the host.
    #[must_use]
    pub const fn defaults_name_to_device_class(&self) -> bool {
        self.device_class.is_some()
    }

    /// Set the typed button device class.
    #[must_use]
    pub fn with_device_class(mut self, device_class: ButtonDeviceClass) -> Self {
        self.spec.info.original_device_class = Some(device_class.as_str().to_owned());
        self.device_class = Some(device_class);
        self
    }

    /// Clear the provider's device-class metadata.
    #[must_use]
    pub fn without_device_class(mut self) -> Self {
        self.spec.info.original_device_class = None;
        self.device_class = None;
        self
    }

    /// Set the registry unique ID.
    #[must_use]
    pub fn with_unique_id(mut self, unique_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_unique_id(unique_id);
        self
    }

    /// Associate the descriptor with a config entry.
    #[must_use]
    pub fn with_config_entry_id(mut self, config_entry_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_config_entry_id(config_entry_id);
        self
    }

    /// Associate the descriptor with a config subentry.
    #[must_use]
    pub fn with_config_subentry_id(mut self, config_subentry_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_config_subentry_id(config_subentry_id);
        self
    }

    /// Associate the descriptor with a device registry entry.
    #[must_use]
    pub fn with_device_id(mut self, device_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_device_id(device_id);
        self
    }

    /// Associate the descriptor with an area registry entry.
    #[must_use]
    pub fn with_area_id(mut self, area_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_area_id(area_id);
        self
    }

    /// Set the suggested object ID for a new registry entity.
    #[must_use]
    pub fn with_suggested_object_id(mut self, object_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_suggested_object_id(object_id);
        self
    }

    /// Set a provider-owned entity ID receipt used during migration.
    #[must_use]
    pub fn with_prior_entity_id(mut self, entity_id: impl Into<String>) -> Self {
        self.spec = self.spec.with_prior_entity_id(entity_id);
        self
    }

    /// Set the provider's original entity name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.spec.info.original_name = Some(name.into());
        self
    }

    /// Set the provider's original icon.
    #[must_use]
    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.spec.info.original_icon = Some(icon.into());
        self
    }

    /// Set the entity category supplied by the provider.
    #[must_use]
    pub fn with_entity_category(mut self, category: impl Into<String>) -> Self {
        self.spec.info.entity_category = Some(category.into());
        self
    }

    /// Set whether the entity name is combined with its device name.
    #[must_use]
    pub fn with_has_entity_name(mut self, has_entity_name: bool) -> Self {
        self.spec.info.has_entity_name = has_entity_name;
        self
    }

    /// Set the provider translation key.
    #[must_use]
    pub fn with_translation_key(mut self, translation_key: impl Into<String>) -> Self {
        self.spec.info.translation_key = Some(translation_key.into());
        self
    }

    /// Set provider capability attributes.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Value) -> Self {
        self.spec.info.capabilities = Some(capabilities);
        self
    }

    /// Set provider-supported feature bits.
    #[must_use]
    pub fn with_supported_features(mut self, supported_features: u64) -> Self {
        self.spec.info.supported_features = supported_features;
        self
    }

    /// Set whether a newly created registry entry starts enabled.
    #[must_use]
    pub fn with_enabled_by_default(mut self, enabled: bool) -> Self {
        self.spec = self.spec.with_enabled_by_default(enabled);
        self
    }

    /// Borrow the shared entity specification used for registration.
    #[must_use]
    pub const fn entity_spec(&self) -> &EntitySpec {
        &self.spec
    }

    /// Consume the descriptor and return its shared entity specification.
    #[must_use]
    pub fn into_entity_spec(self) -> EntitySpec {
        self.spec
    }

    /// Project a button snapshot into the shared publication contract.
    ///
    /// Device class is a live Home Assistant state attribute, including while
    /// unavailable. Passing it through the shared publication's live-attribute
    /// channel preserves that behavior while leaving registry overrides in
    /// charge of precedence.
    #[must_use]
    pub fn publication(&self, snapshot: ButtonSnapshot) -> EntityPublication {
        EntityPublication::from(snapshot.into_entity_snapshot()).with_live_attribute(
            "device_class",
            self.device_class
                .map(|class| Value::String(class.as_str().to_owned())),
        )
    }
}

/// UTC timestamp carried as a button's state after a press.
///
/// Home Assistant's Python `datetime.isoformat()` output has six fractional
/// digits when microseconds are nonzero, omits the fraction when zero, and
/// uses `+00:00`. New values are truncated to microsecond precision to match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ButtonTimestamp(DateTime<Utc>);

impl ButtonTimestamp {
    /// Create a timestamp using the pinned Python timestamp precision.
    #[must_use]
    pub fn from_datetime(value: DateTime<Utc>) -> Self {
        let microseconds = value.timestamp_subsec_micros();
        let normalized = value
            .clone()
            .with_nanosecond(microseconds * 1_000)
            .unwrap_or(value);
        Self(normalized)
    }

    /// Create a timestamp for the current UTC instant.
    #[must_use]
    pub fn now() -> Self {
        Self::from_datetime(Utc::now())
    }

    /// Borrow the normalized UTC timestamp.
    #[must_use]
    pub const fn datetime(&self) -> &DateTime<Utc> {
        &self.0
    }

    /// Return the Home Assistant state string for this timestamp.
    #[must_use]
    pub fn as_state_string(&self) -> String {
        let mut value = self.0.format("%Y-%m-%dT%H:%M:%S").to_string();
        let microseconds = self.0.timestamp_subsec_micros();
        if microseconds != 0 {
            use fmt::Write as _;
            let _ = write!(value, ".{microseconds:06}");
        }
        value.push_str("+00:00");
        value
    }
}

impl From<DateTime<Utc>> for ButtonTimestamp {
    fn from(value: DateTime<Utc>) -> Self {
        Self::from_datetime(value)
    }
}

/// Provider state for one button.
///
/// A press timestamp is represented as the entity state itself, not as an
/// extra `last_pressed` attribute. Restored state is retained as text because
/// Home Assistant's restore hook does not parse or validate it.
#[derive(Clone, Debug, PartialEq)]
pub struct ButtonSnapshot {
    state: Option<String>,
    available: bool,
    context: Context,
}

impl ButtonSnapshot {
    /// Create an available button state with no prior press (`unknown`).
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            state: None,
            available: true,
            context: Context::default(),
        }
    }

    /// Create an available state from a newly generated press timestamp.
    #[must_use]
    pub fn pressed_at(timestamp: ButtonTimestamp) -> Self {
        Self {
            state: Some(timestamp.as_state_string()),
            ..Self::unknown()
        }
    }

    /// Restore a prior state exactly as supplied by the shared lifecycle.
    ///
    /// Home Assistant ignores missing and `unavailable` restored states. Other
    /// values are retained verbatim, without requiring them to parse as dates.
    #[must_use]
    pub fn from_restored_state(state: Option<&str>) -> Option<Self> {
        match state {
            None | Some("unavailable") => None,
            Some(state) => Some(Self {
                state: Some(state.to_owned()),
                ..Self::unknown()
            }),
        }
    }

    /// Create an unavailable state, suppressing ordinary state attributes.
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            state: None,
            available: false,
            context: Context::default(),
        }
    }

    /// Attach the service or provider context to the next publication.
    #[must_use]
    pub fn with_context(mut self, context: Context) -> Self {
        self.context = context;
        self
    }

    /// Return the projected state string, if one is present.
    #[must_use]
    pub fn state(&self) -> Option<&str> {
        self.state.as_deref()
    }

    /// Return whether the button is currently available.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.available
    }

    /// Convert this domain snapshot to the shared lifecycle snapshot.
    #[must_use]
    pub fn into_entity_snapshot(self) -> EntitySnapshot {
        EntitySnapshot {
            state: self.state,
            available: self.available,
            attributes: Map::new(),
            context: self.context,
        }
    }
}

/// Standard button service vocabulary at the pinned Core revision.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ButtonService {
    /// Invoke the provider's button action.
    Press,
}

impl ButtonService {
    /// Return the canonical service name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Press => BUTTON_SERVICE_PRESS,
        }
    }
}

impl FromStr for ButtonService {
    type Err = UnknownButtonService;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            BUTTON_SERVICE_PRESS => Ok(Self::Press),
            _ => Err(UnknownButtonService(value.to_owned())),
        }
    }
}

impl fmt::Display for ButtonService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Parse failure for a service outside the button platform vocabulary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownButtonService(String);

impl fmt::Display for UnknownButtonService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown button service {:?}", self.0)
    }
}

impl StdError for UnknownButtonService {}

/// A validated `button.press` service request.
///
/// The upstream entity-service schema is `None`, so arbitrary service data is
/// preserved rather than rejected. Target selector interpretation remains the
/// job of the shared service-target resolver.
#[derive(Clone, Debug, PartialEq)]
pub struct ButtonPressRequest {
    data: Map<String, Value>,
    target: Map<String, Value>,
    context: Context,
}

impl ButtonPressRequest {
    /// Borrow any pass-through service data accepted by the upstream schema.
    #[must_use]
    pub const fn data(&self) -> &Map<String, Value> {
        &self.data
    }

    /// Borrow selectors for the shared target resolver.
    #[must_use]
    pub const fn target(&self) -> &Map<String, Value> {
        &self.target
    }

    /// Borrow the causal context attached to the request.
    #[must_use]
    pub const fn context(&self) -> &Context {
        &self.context
    }
}

impl TryFrom<ServiceCall> for ButtonPressRequest {
    type Error = ButtonServiceValidationError;

    fn try_from(call: ServiceCall) -> Result<Self, Self::Error> {
        if call.domain != BUTTON_DOMAIN {
            return Err(ButtonServiceValidationError::WrongDomain(call.domain));
        }
        if call.service != BUTTON_SERVICE_PRESS {
            return Err(ButtonServiceValidationError::UnknownService(call.service));
        }
        Ok(Self {
            data: call.data,
            target: call.target,
            context: call.context,
        })
    }
}

/// Failure to validate a native button service request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ButtonServiceValidationError {
    /// The service call belongs to a different entity domain.
    WrongDomain(String),
    /// The service name is not supported by the button platform.
    UnknownService(String),
}

impl fmt::Display for ButtonServiceValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongDomain(domain) => {
                write!(formatter, "button service request has domain {domain:?}")
            }
            Self::UnknownService(service) => {
                write!(formatter, "unknown button service {service:?}")
            }
        }
    }
}

impl StdError for ButtonServiceValidationError {}

/// Future returned by a provider's required button action implementation.
pub type ButtonPressFuture<'a> = Pin<Box<dyn Future<Output = hass_core::Result<()>> + Send + 'a>>;

/// Provider-owned button action corresponding to Home Assistant's
/// `ButtonEntity.async_press` extension point.
///
/// Implementations must perform the real provider action. There is no default
/// or no-op implementation, and this SDK never invokes the trait except from
/// [`execute_button_press`] after publishing the new timestamp.
pub trait ButtonPressHandler: Send + Sync {
    /// Perform the provider-specific action.
    fn async_press(&self) -> ButtonPressFuture<'_>;
}

/// Result of applying a validated press request to the shared entity lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonPressOutcome {
    /// The entity lease was not active, so the provider action was skipped.
    NotPublished,
    /// The entity lease was active and the provider action completed. The
    /// lifecycle may suppress an unchanged timestamp/context write.
    Invoked,
}

/// Apply the generic button-service ordering using the current UTC time.
///
/// The shared lifecycle publication happens before the provider action. If the
/// provider lease is inactive after the publication attempt, the action is
/// skipped. A `None` publication result does not by itself cancel the action:
/// an active lifecycle may suppress an identical timestamp/context write. If
/// the provider action returns an error, that error is propagated and the
/// already-published timestamp remains current and recorded; this wrapper does
/// not roll it back. The service host must apply its own target and
/// provider-availability policy before calling this function; an active
/// [`EntityHandle`] lease does not mean the provider reports the button as
/// available.
pub async fn execute_button_press(
    descriptor: &ButtonDescriptor,
    handle: &EntityHandle,
    request: &ButtonPressRequest,
    handler: &dyn ButtonPressHandler,
) -> hass_core::Result<ButtonPressOutcome> {
    execute_button_press_at(descriptor, handle, request, ButtonTimestamp::now(), handler).await
}

/// Apply the generic button-service ordering at an explicit timestamp.
///
/// This variant permits deterministic clock injection. It has the same
/// publication-before-action and no-rollback behavior as
/// [`execute_button_press`].
pub async fn execute_button_press_at(
    descriptor: &ButtonDescriptor,
    handle: &EntityHandle,
    request: &ButtonPressRequest,
    timestamp: ButtonTimestamp,
    handler: &dyn ButtonPressHandler,
) -> hass_core::Result<ButtonPressOutcome> {
    let snapshot = ButtonSnapshot::pressed_at(timestamp).with_context(request.context.clone());
    let _publication = handle.publish_publication(descriptor.publication(snapshot))?;
    if !handle.status().is_active() {
        return Ok(ButtonPressOutcome::NotPublished);
    }
    handler.async_press().await?;
    Ok(ButtonPressOutcome::Invoked)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use hass_core::{Error, Runtime};
    use hass_registry::EntityUpdate;
    use hass_registry::Registries;
    use hass_storage::SqliteStore;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn service_call(domain: &str, service: &str) -> ServiceCall {
        ServiceCall {
            domain: domain.into(),
            service: service.into(),
            data: Map::new(),
            target: Map::new(),
            context: Context::default(),
        }
    }

    fn timestamp(value: &str) -> ButtonTimestamp {
        ButtonTimestamp::from_datetime(
            DateTime::parse_from_rfc3339(value)
                .unwrap()
                .with_timezone(&Utc),
        )
    }

    #[test]
    fn device_classes_match_the_pinned_button_enum() {
        assert_eq!(
            ButtonDeviceClass::ALL.map(ButtonDeviceClass::as_str),
            ["identify", "restart", "update"]
        );
        assert_eq!("RESTART".parse(), Ok(ButtonDeviceClass::Restart));
        assert!("factory_reset".parse::<ButtonDeviceClass>().is_err());
    }

    #[test]
    fn descriptor_projects_class_and_generic_registry_metadata() {
        let descriptor = ButtonDescriptor::new("fixture", "button-provider", "device/restart")
            .with_unique_id("restart-17")
            .with_suggested_object_id("restart_device")
            .with_name("Restart")
            .with_device_class(ButtonDeviceClass::Restart);

        assert_eq!(descriptor.device_class(), Some(ButtonDeviceClass::Restart));
        assert!(!descriptor.should_poll());
        assert!(descriptor.defaults_name_to_device_class());
        let spec = descriptor.entity_spec();
        assert_eq!(spec.domain, BUTTON_DOMAIN);
        assert_eq!(spec.unique_id.as_deref(), Some("restart-17"));
        assert_eq!(spec.info.original_name.as_deref(), Some("Restart"));
        assert_eq!(spec.info.original_device_class.as_deref(), Some("restart"));

        let unavailable = descriptor.publication(ButtonSnapshot::unavailable());
        assert!(!unavailable.snapshot.available);
    }

    #[test]
    fn timestamp_projection_matches_python_isoformat_precision_and_restore_rules() {
        let timestamp_value = timestamp("2026-11-07T12:34:56.123456789+00:00");
        assert_eq!(
            timestamp_value.as_state_string(),
            "2026-11-07T12:34:56.123456+00:00"
        );
        assert_eq!(
            timestamp("2026-11-07T12:34:56+00:00").as_state_string(),
            "2026-11-07T12:34:56+00:00"
        );

        assert_eq!(ButtonSnapshot::unknown().state(), None);
        assert_eq!(
            ButtonSnapshot::pressed_at(timestamp_value).state(),
            Some("2026-11-07T12:34:56.123456+00:00")
        );
        assert!(ButtonSnapshot::from_restored_state(None).is_none());
        assert!(ButtonSnapshot::from_restored_state(Some("unavailable")).is_none());
        assert_eq!(
            ButtonSnapshot::from_restored_state(Some("provider legacy state"))
                .unwrap()
                .state(),
            Some("provider legacy state")
        );
    }

    #[test]
    fn service_validation_accepts_schema_free_data_and_rejects_wrong_identity() {
        let mut call = service_call(BUTTON_DOMAIN, BUTTON_SERVICE_PRESS);
        call.data
            .insert("provider_hint".into(), json!("passed through"));
        call.target
            .insert("entity_id".into(), json!("button.restart_device"));
        let request = ButtonPressRequest::try_from(call).unwrap();
        assert_eq!(request.data()["provider_hint"], "passed through");
        assert_eq!(request.target()["entity_id"], "button.restart_device");

        assert!(matches!(
            ButtonPressRequest::try_from(service_call("switch", BUTTON_SERVICE_PRESS)),
            Err(ButtonServiceValidationError::WrongDomain(domain)) if domain == "switch"
        ));
        assert!(matches!(
            ButtonPressRequest::try_from(service_call(BUTTON_DOMAIN, "toggle")),
            Err(ButtonServiceValidationError::UnknownService(service)) if service == "toggle"
        ));
    }

    struct ObserveThenFail {
        runtime: Runtime,
        entity_id: String,
        expected_state: String,
    }

    impl ButtonPressHandler for ObserveThenFail {
        fn async_press(&self) -> ButtonPressFuture<'_> {
            Box::pin(async {
                let state = self
                    .runtime
                    .get_state(&self.entity_id)
                    .expect("timestamp publication precedes provider action");
                assert_eq!(state.state, self.expected_state);
                Err(Error::Service("synthetic provider failure".into()))
            })
        }
    }

    struct CountPresses(Arc<AtomicUsize>);

    impl ButtonPressHandler for CountPresses {
        fn async_press(&self) -> ButtonPressFuture<'_> {
            let calls = self.0.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn equal_timestamp_and_context_still_invoke_provider_twice() {
        let runtime = Runtime::new(Arc::new(SqliteStore::in_memory().unwrap())).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = hass_entity::EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let descriptor = ButtonDescriptor::new("fixture", "button-provider", "device/restart")
            .with_unique_id("restart-17")
            .with_suggested_object_id("restart_device");
        let handle = lifecycle
            .register_batch(vec![descriptor.entity_spec().clone()])
            .unwrap()
            .remove(0);
        let request =
            ButtonPressRequest::try_from(service_call(BUTTON_DOMAIN, BUTTON_SERVICE_PRESS))
                .unwrap();
        let timestamp = timestamp("2026-11-07T12:34:56.123456+00:00");
        let calls = Arc::new(AtomicUsize::new(0));
        let handler = CountPresses(calls.clone());

        for _ in 0..2 {
            assert_eq!(
                execute_button_press_at(
                    &descriptor,
                    &handle,
                    &request,
                    timestamp.clone(),
                    &handler,
                )
                .await
                .unwrap(),
                ButtonPressOutcome::Invoked
            );
        }

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn inactive_registry_lease_skips_provider_action() {
        let runtime = Runtime::new(Arc::new(SqliteStore::in_memory().unwrap())).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle =
            hass_entity::EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let descriptor = ButtonDescriptor::new("fixture", "button-provider", "device/restart")
            .with_unique_id("restart-17")
            .with_suggested_object_id("restart_device");
        let handle = lifecycle
            .register_batch(vec![descriptor.entity_spec().clone()])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        registries
            .update_entity(
                &entity_id,
                EntityUpdate {
                    disabled_by: Some(Some("user".into())),
                    ..EntityUpdate::default()
                },
            )
            .unwrap();
        let request =
            ButtonPressRequest::try_from(service_call(BUTTON_DOMAIN, BUTTON_SERVICE_PRESS))
                .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let handler = CountPresses(calls.clone());

        assert_eq!(
            execute_button_press_at(
                &descriptor,
                &handle,
                &request,
                timestamp("2026-11-07T12:34:56.123456+00:00"),
                &handler,
            )
            .await
            .unwrap(),
            ButtonPressOutcome::NotPublished
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn clearing_device_class_tombstones_stale_provider_fallback() {
        let runtime = Runtime::new(Arc::new(SqliteStore::in_memory().unwrap())).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = hass_entity::EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let descriptor = ButtonDescriptor::new("fixture", "button-provider", "device/restart")
            .with_unique_id("restart-17")
            .with_suggested_object_id("restart_device")
            .with_device_class(ButtonDeviceClass::Restart);
        let handle = lifecycle
            .register_batch(vec![descriptor.entity_spec().clone()])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();

        handle
            .publish_publication(descriptor.publication(ButtonSnapshot::unknown()))
            .unwrap()
            .unwrap();
        assert_eq!(
            runtime
                .get_state(&entity_id)
                .unwrap()
                .attributes
                .get("device_class"),
            Some(&json!("restart"))
        );

        let classless = descriptor.clone().without_device_class();
        handle
            .publish_publication(classless.publication(ButtonSnapshot::unknown()))
            .unwrap()
            .unwrap();
        assert!(
            !runtime
                .get_state(&entity_id)
                .unwrap()
                .attributes
                .contains_key("device_class")
        );

        lifecycle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn lifecycle_records_timestamp_before_provider_failure_without_rollback() {
        let store = Arc::new(SqliteStore::in_memory().unwrap());
        let runtime = Runtime::new(store).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = hass_entity::EntityLifecycle::new(runtime.clone(), registries).unwrap();
        let descriptor = ButtonDescriptor::new("fixture", "button-provider", "device/restart")
            .with_unique_id("restart-17")
            .with_suggested_object_id("restart_device")
            .with_device_class(ButtonDeviceClass::Restart);
        let handle = lifecycle
            .register_batch(vec![descriptor.entity_spec().clone()])
            .unwrap()
            .remove(0);
        let entity_id = handle.current_entity_id().unwrap();
        let mut service_call = service_call(BUTTON_DOMAIN, BUTTON_SERVICE_PRESS);
        service_call.context = Context::default();
        let request = ButtonPressRequest::try_from(service_call).unwrap();
        let timestamp = timestamp("2026-11-07T12:34:56.123456+00:00");
        let expected_state = timestamp.as_state_string();
        let handler = ObserveThenFail {
            runtime: runtime.clone(),
            entity_id: entity_id.clone(),
            expected_state: expected_state.clone(),
        };

        let result =
            execute_button_press_at(&descriptor, &handle, &request, timestamp, &handler).await;
        assert!(
            matches!(result, Err(Error::Service(message)) if message == "synthetic provider failure")
        );

        let state = runtime.get_state(&entity_id).unwrap();
        assert_eq!(state.state, expected_state);
        assert_eq!(
            state.attributes.get("device_class"),
            Some(&json!("restart"))
        );
        let history = runtime
            .history(
                &entity_id,
                state.last_updated - chrono::Duration::seconds(1),
                state.last_updated + chrono::Duration::seconds(1),
            )
            .unwrap();
        assert!(history.iter().any(|row| row.state == expected_state));

        lifecycle.shutdown().await.unwrap();
    }

    #[test]
    fn source_interval_metadata_remains_separate_from_polling() {
        assert_eq!(BUTTON_SCAN_INTERVAL_SECONDS, 30);
        assert_eq!(BUTTON_MIN_TIME_BETWEEN_SCANS_SECONDS, 10);
        assert!(!ButtonDescriptor::new("fixture", "provider", "button").should_poll());
        let epoch = Utc.with_ymd_and_hms(2026, 11, 7, 0, 0, 0).unwrap();
        assert_eq!(
            ButtonTimestamp::from_datetime(epoch).as_state_string(),
            "2026-11-07T00:00:00+00:00"
        );
    }
}
