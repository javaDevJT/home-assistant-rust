//! Native RESTful sensors and commands.
//!
//! Resource polls share one request across their configured sensors. HTTP
//! failures, invalid templates, and absent values leave sensors unavailable;
//! this integration never invents a zero reading.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use encoding_rs::Encoding;
use futures_util::StreamExt;
use hass_core::{Context, Error, Result, Runtime, ServiceCall, ServiceResult};
use hass_template::TemplateEngine;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, WWW_AUTHENTICATE};
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, Semaphore, watch};
use tokio::task::JoinHandle;
use url::Url;
use uuid::Uuid;

const DEFAULT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_SCAN_INTERVAL_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 300;
const MAX_SCAN_INTERVAL_SECS: u64 = 86_400;
const MAX_RESOURCES: usize = 64;
const MAX_SENSORS: usize = 256;
const MAX_COMMANDS: usize = 128;
const MAX_BODY_BYTES: usize = 1_048_576;
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 8;

fn default_method_get() -> String {
    "GET".into()
}

fn default_method_command() -> String {
    "GET".into()
}

fn default_authentication() -> String {
    "basic".into()
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

fn default_scan_interval() -> u64 {
    DEFAULT_SCAN_INTERVAL_SECS
}

fn default_ssl_cipher_list() -> String {
    "python_default".into()
}

fn default_false() -> bool {
    false
}

fn default_verify_ssl() -> bool {
    true
}

fn default_name() -> String {
    "REST Sensor".into()
}

fn deserialize_scan_interval<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct ScanIntervalVisitor;

    impl<'de> Visitor<'de> for ScanIntervalVisitor {
        type Value = u64;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("positive seconds or a HH:MM:SS duration")
        }

        fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
            Ok(value)
        }

        fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            u64::try_from(value).map_err(|_| E::custom("scan_interval must be positive"))
        }

        fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            if !value.is_finite() || value <= 0.0 || value > MAX_SCAN_INTERVAL_SECS as f64 {
                return Err(E::custom("scan_interval is outside the supported range"));
            }
            Ok(value.ceil() as u64)
        }

        fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_scan_interval(value).ok_or_else(|| E::custom("scan_interval duration is invalid"))
        }

        fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&value)
        }
    }

    deserializer.deserialize_any(ScanIntervalVisitor)
}

fn parse_scan_interval(value: &str) -> Option<u64> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    let pieces: Vec<_> = value.split(':').collect();
    let (hours, minutes, seconds) = match pieces.as_slice() {
        [minutes, seconds] => (
            0,
            minutes.parse::<u64>().ok()?,
            seconds.parse::<u64>().ok()?,
        ),
        [hours, minutes, seconds] => (
            hours.parse::<u64>().ok()?,
            minutes.parse::<u64>().ok()?,
            seconds.parse::<u64>().ok()?,
        ),
        _ => return None,
    };
    if minutes >= 60 || seconds >= 60 {
        return None;
    }
    hours
        .checked_mul(3600)?
        .checked_add(minutes.checked_mul(60)?)?
        .checked_add(seconds)
}

fn deserialize_json_attributes<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct JsonAttributesVisitor;

    impl<'de> Visitor<'de> for JsonAttributesVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a list or comma-separated string of JSON attribute names")
        }

        fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
            Ok(value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect())
        }

        fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&value)
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut attributes = Vec::new();
            while let Some(value) = sequence.next_element::<String>()? {
                attributes.push(value);
            }
            Ok(attributes)
        }
    }

    deserializer.deserialize_any(JsonAttributesVisitor)
}

/// One HTTP resource and the sensors extracted from its response.
///
/// `resource` and `resource_template` are mutually exclusive. Credentials,
/// headers, payloads, and URLs are deliberately omitted from `Debug` output.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestConfig {
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub resource_template: Option<String>,
    #[serde(default = "default_method_get")]
    pub method: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    #[serde(default)]
    pub payload: Option<String>,
    #[serde(default)]
    pub payload_template: Option<String>,
    #[serde(default = "default_authentication")]
    pub authentication: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(
        default = "default_scan_interval",
        deserialize_with = "deserialize_scan_interval"
    )]
    pub scan_interval: u64,
    #[serde(default = "default_verify_ssl")]
    pub verify_ssl: bool,
    #[serde(default = "default_encoding")]
    pub encoding: String,
    #[serde(default = "default_ssl_cipher_list")]
    pub ssl_cipher_list: String,
    pub sensor: Vec<RestSensorConfig>,
}

impl fmt::Debug for RestConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestConfig")
            .field("resource", &self.resource.as_ref().map(|_| "<redacted>"))
            .field(
                "resource_template",
                &self.resource_template.as_ref().map(|_| "<redacted>"),
            )
            .field("method", &self.method)
            .field("headers", &"<redacted>")
            .field("params", &"<redacted>")
            .field("payload", &self.payload.as_ref().map(|_| "<redacted>"))
            .field(
                "payload_template",
                &self.payload_template.as_ref().map(|_| "<redacted>"),
            )
            .field("authentication", &self.authentication)
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("timeout", &self.timeout)
            .field("scan_interval", &self.scan_interval)
            .field("verify_ssl", &self.verify_ssl)
            .field("encoding", &self.encoding)
            .field("ssl_cipher_list", &self.ssl_cipher_list)
            .field("sensor_count", &self.sensor.len())
            .finish()
    }
}

/// A sensor definition attached to a [`RestConfig`] resource.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestSensorConfig {
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default)]
    pub value_template: Option<String>,
    #[serde(default)]
    pub availability: Option<String>,
    #[serde(default, deserialize_with = "deserialize_json_attributes")]
    pub json_attributes: Vec<String>,
    #[serde(default)]
    pub json_attributes_path: Option<String>,
    #[serde(default)]
    pub device_class: Option<String>,
    #[serde(default)]
    pub state_class: Option<String>,
    #[serde(default)]
    pub unit_of_measurement: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub picture: Option<String>,
    #[serde(default)]
    pub unique_id: Option<String>,
    #[serde(default)]
    pub force_update: bool,
}

impl fmt::Debug for RestSensorConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestSensorConfig")
            .field("name", &self.name)
            .field(
                "value_template",
                &self.value_template.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "availability",
                &self.availability.as_ref().map(|_| "<redacted>"),
            )
            .field("json_attributes", &self.json_attributes)
            .field("json_attributes_path", &self.json_attributes_path)
            .field("device_class", &self.device_class)
            .field("state_class", &self.state_class)
            .field("unit_of_measurement", &self.unit_of_measurement)
            .field("icon", &self.icon.as_ref().map(|_| "<redacted>"))
            .field("picture", &self.picture.as_ref().map(|_| "<redacted>"))
            .field("unique_id", &self.unique_id.as_ref().map(|_| "<redacted>"))
            .field("force_update", &self.force_update)
            .finish()
    }
}

/// One named `rest_command` service.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestCommandConfig {
    pub url: String,
    #[serde(default = "default_method_command")]
    pub method: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub payload: Option<String>,
    #[serde(default = "default_authentication")]
    pub authentication: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default = "default_verify_ssl")]
    pub verify_ssl: bool,
    #[serde(default = "default_false")]
    pub insecure_cipher: bool,
    #[serde(default = "default_false")]
    pub skip_url_encoding: bool,
}

impl fmt::Debug for RestCommandConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestCommandConfig")
            .field("url", &"<redacted>")
            .field("method", &self.method)
            .field("headers", &"<redacted>")
            .field("payload", &self.payload.as_ref().map(|_| "<redacted>"))
            .field("authentication", &self.authentication)
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("timeout", &self.timeout)
            .field("content_type", &self.content_type)
            .field("verify_ssl", &self.verify_ssl)
            .field("insecure_cipher", &self.insecure_cipher)
            .field("skip_url_encoding", &self.skip_url_encoding)
            .finish()
    }
}

fn default_encoding() -> String {
    "UTF-8".into()
}

/// Validate REST resources and command definitions without network or runtime
/// side effects.
pub fn validate_config(
    resources: &[RestConfig],
    commands: &BTreeMap<String, RestCommandConfig>,
) -> Result<()> {
    if resources.len() > MAX_RESOURCES {
        return Err(invalid_config("too many REST resources"));
    }
    if commands.len() > MAX_COMMANDS {
        return Err(invalid_config("too many REST commands"));
    }

    let mut sensor_count = 0usize;
    let mut entity_ids = BTreeSet::new();
    let mut unique_ids = BTreeSet::new();
    for (resource_index, resource) in resources.iter().enumerate() {
        match (&resource.resource, &resource.resource_template) {
            (Some(url), None) => validate_static_url(url)?,
            (None, Some(template)) if !template.trim().is_empty() => {
                validate_template_size(template)?;
            }
            _ => {
                return Err(invalid_config(
                    "a REST resource must define exactly one of resource or resource_template",
                ));
            }
        }
        if !matches!(
            resource.method.to_ascii_uppercase().as_str(),
            "GET" | "POST"
        ) {
            return Err(invalid_config("REST sensor method must be GET or POST"));
        }
        validate_authentication(
            &resource.authentication,
            resource.username.as_deref(),
            resource.password.as_deref(),
        )?;
        if !matches!(
            resource.ssl_cipher_list.as_str(),
            "python_default" | "intermediate" | "modern" | "insecure"
        ) {
            return Err(invalid_config("ssl_cipher_list is not supported"));
        }
        if resource.ssl_cipher_list != "python_default" {
            return Err(invalid_config(
                "the reqwest rustls client supports only ssl_cipher_list python_default",
            ));
        }
        validate_timeout(resource.timeout)?;
        if !(1..=MAX_SCAN_INTERVAL_SECS).contains(&resource.scan_interval) {
            return Err(invalid_config(
                "scan_interval must be between 1 and 86400 seconds",
            ));
        }
        validate_headers(&resource.headers)?;
        validate_string_map(&resource.params)?;
        if resource.payload.is_some() && resource.payload_template.is_some() {
            return Err(invalid_config(
                "payload and payload_template are mutually exclusive",
            ));
        }
        if let Some(payload) = &resource.payload {
            validate_template_size(payload)?;
        }
        if let Some(payload) = &resource.payload_template {
            validate_template_size(payload)?;
        }
        if Encoding::for_label(resource.encoding.trim().as_bytes()).is_none() {
            return Err(invalid_config("encoding is not a supported character set"));
        }
        if resource.sensor.is_empty() {
            return Err(invalid_config(
                "a REST resource must define at least one sensor",
            ));
        }

        for (sensor_index, sensor) in resource.sensor.iter().enumerate() {
            sensor_count += 1;
            if sensor_count > MAX_SENSORS {
                return Err(invalid_config("too many REST sensors"));
            }
            if sensor.name.trim().is_empty() {
                return Err(invalid_config("REST sensor name cannot be empty"));
            }
            for template in [
                sensor.value_template.as_deref(),
                sensor.availability.as_deref(),
                sensor.icon.as_deref(),
                sensor.picture.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                validate_template_size(template)?;
            }
            if sensor
                .json_attributes
                .iter()
                .any(|attribute| attribute.trim().is_empty())
            {
                return Err(invalid_config("JSON attribute names cannot be empty"));
            }
            if let Some(path) = &sensor.json_attributes_path {
                parse_json_path(path)?;
            }
            if let Some(unique_id) = &sensor.unique_id {
                if unique_id.trim().is_empty() || unique_id.len() > 255 {
                    return Err(invalid_config(
                        "REST sensor unique_id must be non-empty and bounded",
                    ));
                }
                if !unique_ids.insert(unique_id) {
                    return Err(invalid_config(
                        "REST sensor unique_id values must be unique",
                    ));
                }
            }
            let entity_id = configured_entity_id(sensor, resource_index, sensor_index);
            if entity_id.len() > 255 {
                return Err(invalid_config(
                    "REST sensor name produces an entity ID that is too long",
                ));
            }
            if !entity_ids.insert(entity_id) {
                return Err(invalid_config(
                    "REST sensor names produce duplicate entity IDs",
                ));
            }
        }
    }

    for (name, config) in commands {
        if !valid_component(name) {
            return Err(invalid_config(
                "REST command names must be lowercase service names",
            ));
        }
        if name == "reload" {
            return Err(invalid_config("REST command name reload is reserved"));
        }
        validate_template_size(&config.url)?;
        if config.url.trim().is_empty() {
            return Err(invalid_config("REST command URL cannot be empty"));
        }
        if !matches!(
            config.method.to_ascii_lowercase().as_str(),
            "get" | "patch" | "post" | "put" | "delete"
        ) {
            return Err(invalid_config("REST command method is not supported"));
        }
        validate_authentication(
            &config.authentication,
            config.username.as_deref(),
            config.password.as_deref(),
        )?;
        if config.insecure_cipher {
            return Err(invalid_config(
                "insecure_cipher is not supported by the reqwest rustls client",
            ));
        }
        if config.skip_url_encoding {
            return Err(invalid_config(
                "skip_url_encoding is not supported by the reqwest URL parser",
            ));
        }
        validate_timeout(config.timeout)?;
        validate_headers(&config.headers)?;
        if let Some(payload) = &config.payload {
            validate_template_size(payload)?;
        }
        if let Some(content_type) = &config.content_type {
            HeaderValue::from_str(content_type)
                .map_err(|_| invalid_config("content_type is not a valid HTTP header value"))?;
        }
        if !templateish(&config.url) {
            validate_static_url(&config.url)?;
        }
    }

    Ok(())
}

fn invalid_config(message: &str) -> Error {
    Error::InvalidInput(message.into())
}

fn validate_timeout(timeout: u64) -> Result<()> {
    if !(1..=MAX_TIMEOUT_SECS).contains(&timeout) {
        return Err(invalid_config("timeout must be between 1 and 300 seconds"));
    }
    Ok(())
}

fn validate_authentication(
    authentication: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<()> {
    if !matches!(
        authentication.to_ascii_lowercase().as_str(),
        "basic" | "digest"
    ) {
        return Err(invalid_config("authentication must be basic or digest"));
    }
    if username.is_some() != password.is_some() {
        return Err(invalid_config(
            "username and password must be configured together",
        ));
    }
    if username.is_some_and(|username| username.contains(':')) {
        return Err(invalid_config("username cannot contain a colon"));
    }
    Ok(())
}

fn validate_headers(headers: &BTreeMap<String, String>) -> Result<()> {
    if headers.len() > 128 {
        return Err(invalid_config("too many HTTP headers"));
    }
    for (name, value) in headers {
        HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| invalid_config("HTTP header name is invalid"))?;
        validate_template_size(value)?;
    }
    Ok(())
}

fn validate_string_map(values: &BTreeMap<String, String>) -> Result<()> {
    if values.len() > 128 {
        return Err(invalid_config("too many REST query parameters"));
    }
    for value in values.values() {
        validate_template_size(value)?;
    }
    Ok(())
}

fn validate_template_size(value: &str) -> Result<()> {
    if value.len() > 64 * 1024 {
        return Err(invalid_config(
            "REST template or value exceeds the size limit",
        ));
    }
    Ok(())
}

fn validate_static_url(value: &str) -> Result<()> {
    let parsed = Url::parse(value.trim()).map_err(|_| invalid_config("REST URL is invalid"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(invalid_config("REST URL must use HTTP or HTTPS"));
    }
    Ok(())
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn templateish(value: &str) -> bool {
    value.contains("{{") || value.contains("{%")
}

/// Running REST resources and registered `rest_command` services.
pub struct RestIntegration {
    runtime: Runtime,
    owner: String,
    cancel: watch::Sender<bool>,
    commands: Arc<CommandLifecycle>,
    tasks: Vec<JoinHandle<()>>,
}

struct CommandLifecycle {
    active: AtomicBool,
    in_flight: AtomicUsize,
    finished: Notify,
    cancel: watch::Sender<bool>,
}

struct CommandCallGuard(Arc<CommandLifecycle>);

impl CommandLifecycle {
    fn begin(self: &Arc<Self>) -> Result<CommandCallGuard> {
        if !self.active.load(Ordering::Acquire) {
            return Err(Error::Service("REST integration is shut down".into()));
        }
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        if !self.active.load(Ordering::Acquire) {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            self.finished.notify_one();
            return Err(Error::Service("REST integration is shut down".into()));
        }
        Ok(CommandCallGuard(Arc::clone(self)))
    }

    async fn wait_for_commands(&self) {
        loop {
            let finished = self.finished.notified();
            if self.in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            finished.await;
        }
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
        let _ = self.cancel.send(true);
    }
}

impl Drop for CommandCallGuard {
    fn drop(&mut self) {
        if self.0.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.finished.notify_one();
        }
    }
}

impl Drop for RestIntegration {
    fn drop(&mut self) {
        self.commands.deactivate();
        let _ = self.cancel.send(true);
        if !self.owner.is_empty() && self.runtime.unregister_owned_services(&self.owner).is_ok() {
            self.owner.clear();
        }
    }
}

impl RestIntegration {
    /// Validate configuration, initialize resources, register commands, and start polling.
    pub async fn start(
        runtime: Runtime,
        resources: Vec<RestConfig>,
        commands: BTreeMap<String, RestCommandConfig>,
    ) -> Result<Self> {
        validate_config(&resources, &commands)?;
        let tokio_handle = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::Service("REST integration requires a Tokio runtime".into()))?;

        // Complete every fallible client build and state initialization before
        // publishing any service handler or starting a poll task.
        let engine = TemplateEngine::new(runtime.clone());
        let mut command_clients = BTreeMap::new();
        for (service, config) in &commands {
            command_clients.insert(service.clone(), make_client(config.verify_ssl)?);
        }

        let mut prepared_resources = Vec::with_capacity(resources.len());
        for config in resources {
            let client = make_client(config.verify_ssl)?;
            prepared_resources.push((client, config));
        }

        let mut pollers = Vec::with_capacity(prepared_resources.len());
        for (resource_index, (client, config)) in prepared_resources.into_iter().enumerate() {
            let mut entity_ids = Vec::with_capacity(config.sensor.len());
            let mut sensor_names = Vec::with_capacity(config.sensor.len());
            for (sensor_index, sensor) in config.sensor.iter().enumerate() {
                let entity_id = configured_entity_id(sensor, resource_index, sensor_index);
                let name = initial_name(sensor, &engine, resource_index, sensor_index);
                let mut attributes = base_attributes(sensor, &name);
                attributes.insert("rest_error".into(), Value::String("not_polled".into()));
                runtime.set_state(
                    &entity_id,
                    "unavailable",
                    attributes,
                    Context::default(),
                    false,
                )?;
                entity_ids.push(entity_id);
                sensor_names.push(name);
            }

            pollers.push((client, config, entity_ids, sensor_names));
        }

        let owner = Uuid::new_v4().to_string();
        let (command_cancel, _) = watch::channel(false);
        let command_lifecycle = Arc::new(CommandLifecycle {
            active: AtomicBool::new(true),
            in_flight: AtomicUsize::new(0),
            finished: Notify::new(),
            cancel: command_cancel,
        });
        let command_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));

        for (service, config) in &commands {
            let client = command_clients
                .get(service)
                .expect("every validated command has a prepared HTTP client")
                .clone();
            let service_lifecycle = Arc::clone(&command_lifecycle);
            let service_semaphore = Arc::clone(&command_semaphore);
            let service_name = service.clone();
            let service_config = config.clone();
            let registration = runtime.register_owned_service(
                &owner,
                "rest_command",
                service,
                json!({"name": service, "fields": {}}),
                Arc::new(move |callback_runtime, call| {
                    let _guard = service_lifecycle.begin()?;
                    let permit = service_semaphore.clone().try_acquire_owned().map_err(|_| {
                        Error::Service("REST command concurrency limit reached".into())
                    })?;
                    // Construct the template engine from the invocation's
                    // runtime. Capturing one here would create a Runtime ->
                    // service handler -> TemplateEngine -> Runtime cycle.
                    run_command_sync(
                        client.clone(),
                        TemplateEngine::new(callback_runtime.clone()),
                        service_config.clone(),
                        call.clone(),
                        permit,
                        service_name.clone(),
                        service_lifecycle.cancel.subscribe(),
                    )
                }),
            );
            if let Err(error) = registration {
                command_lifecycle.deactivate();
                let cleanup = runtime.unregister_owned_services(&owner);
                command_lifecycle.wait_for_commands().await;
                return match cleanup {
                    Ok(_) => Err(error),
                    Err(cleanup_error) => Err(Error::Service(format!(
                        "{error}; failed to remove partially registered REST commands: {cleanup_error}"
                    ))),
                };
            }
        }

        let (cancel, receiver) = watch::channel(false);
        let poll_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));
        let mut tasks = Vec::with_capacity(pollers.len());
        for (client, config, entity_ids, sensor_names) in pollers {
            let task_runtime = runtime.clone();
            let task_engine = engine.clone();
            let task_semaphore = Arc::clone(&poll_semaphore);
            let mut task_cancel = receiver.clone();
            let poll_interval = Duration::from_secs(config.scan_interval);
            tasks.push(tokio_handle.spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        changed = task_cancel.changed() => {
                            if changed.is_err() || *task_cancel.borrow() {
                                break;
                            }
                        }
                        _ = poll_resource(
                            &task_runtime,
                            &task_engine,
                            &client,
                            &config,
                            &entity_ids,
                            &sensor_names,
                            Arc::clone(&task_semaphore),
                        ) => {}
                    }
                    tokio::select! {
                        biased;
                        changed = task_cancel.changed() => {
                            if changed.is_err() || *task_cancel.borrow() {
                                break;
                            }
                        }
                        _ = tokio::time::sleep(poll_interval) => {}
                    }
                }
            }));
        }

        Ok(Self {
            runtime,
            owner,
            cancel,
            commands: command_lifecycle,
            tasks,
        })
    }

    /// Stop polling, cancel in-flight requests, and wait for every task.
    pub async fn shutdown(mut self) -> Result<()> {
        self.commands.deactivate();
        let _ = self.cancel.send(true);
        let unregister = self.runtime.unregister_owned_services(&self.owner);
        if unregister.is_ok() {
            self.owner.clear();
        }
        let tasks = std::mem::take(&mut self.tasks);
        let mut task_failed = false;
        for task in tasks {
            task_failed |= task.await.is_err();
        }
        self.commands.wait_for_commands().await;
        unregister?;
        if task_failed {
            return Err(Error::Service("REST polling task failed".into()));
        }
        Ok(())
    }
}

fn initial_name(
    sensor: &RestSensorConfig,
    engine: &TemplateEngine,
    resource_index: usize,
    sensor_index: usize,
) -> String {
    if !templateish(&sensor.name) {
        return sensor.name.clone();
    }
    engine
        .render(&sensor.name, &Map::new())
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("REST Sensor {} {}", resource_index + 1, sensor_index + 1))
}

fn configured_entity_id(
    sensor: &RestSensorConfig,
    resource_index: usize,
    sensor_index: usize,
) -> String {
    let base = if templateish(&sensor.name) {
        format!("rest_sensor_{}_{}", resource_index + 1, sensor_index + 1)
    } else {
        slugify(&sensor.name)
    };
    format!("sensor.{base}")
}

fn slugify(value: &str) -> String {
    let mut slug = String::with_capacity(value.len());
    let mut previous_separator = true;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            previous_separator = false;
        } else if !previous_separator {
            slug.push('_');
            previous_separator = true;
        }
    }
    while slug.ends_with('_') {
        slug.pop();
    }
    if slug.is_empty() {
        "rest_sensor".into()
    } else {
        slug
    }
}

fn base_attributes(sensor: &RestSensorConfig, display_name: &str) -> Map<String, Value> {
    let mut attributes = Map::new();
    attributes.insert("friendly_name".into(), Value::String(display_name.into()));
    insert_option(
        &mut attributes,
        "unit_of_measurement",
        &sensor.unit_of_measurement,
    );
    insert_option(&mut attributes, "device_class", &sensor.device_class);
    insert_option(&mut attributes, "state_class", &sensor.state_class);
    attributes
}

fn insert_option(attributes: &mut Map<String, Value>, key: &str, value: &Option<String>) {
    if let Some(value) = value {
        attributes.insert(key.into(), Value::String(value.clone()));
    }
}

#[derive(Clone, Copy)]
enum Failure {
    Request,
    Timeout,
    HttpStatus,
    BodyTooLarge,
    InvalidEncoding,
    InvalidResponse,
    MissingValue,
    Template,
    Unavailable,
    UnsupportedAuthentication,
}

async fn poll_resource(
    runtime: &Runtime,
    engine: &TemplateEngine,
    client: &Client,
    config: &RestConfig,
    entity_ids: &[String],
    sensor_names: &[String],
    semaphore: Arc<Semaphore>,
) {
    let request_result = async {
        let _permit = semaphore
            .acquire_owned()
            .await
            .map_err(|_| Failure::Request)?;
        fetch_resource(client, engine, config).await
    }
    .await;

    match request_result {
        Err(failure) => {
            trace_failure("resource", failure);
            for ((sensor, entity_id), name) in
                config.sensor.iter().zip(entity_ids).zip(sensor_names)
            {
                mark_unavailable(runtime, entity_id, sensor, name, failure);
            }
        }
        Ok((body, json_value, sensitive_values)) => {
            for ((sensor, entity_id), name) in
                config.sensor.iter().zip(entity_ids).zip(sensor_names)
            {
                let result = sensor_state(
                    engine,
                    sensor,
                    runtime,
                    entity_id,
                    &body,
                    json_value.as_ref(),
                    &sensitive_values,
                );
                match result {
                    Ok((state, attributes)) => {
                        if runtime
                            .set_state(
                                entity_id,
                                &state,
                                attributes,
                                Context::default(),
                                sensor.force_update,
                            )
                            .is_err()
                        {
                            trace_failure("state", Failure::InvalidResponse);
                        }
                    }
                    Err(failure) => {
                        trace_failure("sensor", failure);
                        mark_unavailable(runtime, entity_id, sensor, name, failure);
                    }
                }
            }
        }
    }
}

fn trace_failure(scope: &str, failure: Failure) {
    // Never include an error string, URL, request body, or header value here.
    tracing::warn!(target: "hass_rest", scope, reason = failure_label(failure), "REST update failed");
}

fn failure_label(failure: Failure) -> &'static str {
    match failure {
        Failure::Request => "request_failed",
        Failure::Timeout => "timeout",
        Failure::HttpStatus => "http_status",
        Failure::BodyTooLarge => "body_too_large",
        Failure::InvalidEncoding => "invalid_encoding",
        Failure::InvalidResponse => "invalid_response",
        Failure::MissingValue => "missing_value",
        Failure::Template => "template_error",
        Failure::Unavailable => "availability_template_false",
        Failure::UnsupportedAuthentication => "unsupported_authentication",
    }
}

fn mark_unavailable(
    runtime: &Runtime,
    entity_id: &str,
    sensor: &RestSensorConfig,
    name: &str,
    failure: Failure,
) {
    let mut attributes = base_attributes(sensor, name);
    attributes.insert(
        "rest_error".into(),
        Value::String(failure_label(failure).into()),
    );
    let _ = runtime.set_state(
        entity_id,
        "unavailable",
        attributes,
        Context::default(),
        false,
    );
}

async fn fetch_resource(
    client: &Client,
    engine: &TemplateEngine,
    config: &RestConfig,
) -> std::result::Result<(String, Option<Value>, Vec<String>), Failure> {
    let variables = Map::new();
    let raw_url = match (&config.resource, &config.resource_template) {
        (Some(url), None) => url.clone(),
        (None, Some(template)) => engine
            .render(template, &variables)
            .map_err(|_| Failure::Template)?,
        _ => return Err(Failure::Request),
    };
    let headers = render_headers(engine, &config.headers, &variables)?;
    let params = render_string_map(engine, &config.params, &variables)?;
    let payload = match config.payload_template.as_deref() {
        Some(template) => Some(
            engine
                .render(template, &variables)
                .map_err(|_| Failure::Template)?,
        ),
        None => config.payload.clone(),
    };
    let sensitive_values = request_sensitive_values(
        &raw_url,
        &headers,
        &params,
        payload.as_deref(),
        config.username.as_deref(),
        config.password.as_deref(),
    );
    let response = send_request(
        client,
        RequestSpec {
            method: config.method.clone(),
            url: raw_url,
            headers,
            params,
            payload,
            username: config.username.clone(),
            password: config.password.clone(),
            authentication: config.authentication.clone(),
            timeout_seconds: config.timeout,
        },
    )
    .await?;
    if !StatusCode::from_u16(response.status)
        .map(|status| status.is_success())
        .unwrap_or(false)
    {
        return Err(Failure::HttpStatus);
    }
    let encoding = response_charset(&response.headers).unwrap_or(&config.encoding);
    let decoded = decode_body(&response.body, encoding)?;
    let json_value = serde_json::from_str(&decoded).ok();
    Ok((decoded, json_value, sensitive_values))
}

fn decode_body(body: &[u8], encoding: &str) -> std::result::Result<String, Failure> {
    let charset =
        Encoding::for_label(encoding.trim().as_bytes()).ok_or(Failure::InvalidEncoding)?;
    let (decoded, _, had_errors) = charset.decode(body);
    if had_errors {
        return Err(Failure::InvalidEncoding);
    }
    Ok(decoded.into_owned())
}

fn response_charset(headers: &BTreeMap<String, Vec<String>>) -> Option<&str> {
    let content_type = headers.get("content-type")?.first()?;
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.trim().split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches(['\'', '"']))
    })
}

fn response_content_type(headers: &BTreeMap<String, Vec<String>>) -> Option<&str> {
    headers
        .get("content-type")
        .and_then(|values| values.first())
        .map(String::as_str)
}

fn render_headers(
    engine: &TemplateEngine,
    headers: &BTreeMap<String, String>,
    variables: &Map<String, Value>,
) -> std::result::Result<BTreeMap<String, String>, Failure> {
    headers
        .iter()
        .map(|(key, value)| {
            engine
                .render(value, variables)
                .map(|value| (key.clone(), value))
                .map_err(|_| Failure::Template)
        })
        .collect()
}

fn render_string_map(
    engine: &TemplateEngine,
    values: &BTreeMap<String, String>,
    variables: &Map<String, Value>,
) -> std::result::Result<BTreeMap<String, String>, Failure> {
    values
        .iter()
        .map(|(key, value)| {
            engine
                .render(value, variables)
                .map(|value| (key.clone(), value))
                .map_err(|_| Failure::Template)
        })
        .collect()
}

struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, Vec<String>>,
    body: Vec<u8>,
}

struct RequestSpec {
    method: String,
    url: String,
    headers: BTreeMap<String, String>,
    params: BTreeMap<String, String>,
    payload: Option<String>,
    username: Option<String>,
    password: Option<String>,
    authentication: String,
    timeout_seconds: u64,
}

async fn send_request(
    client: &Client,
    spec: RequestSpec,
) -> std::result::Result<HttpResponse, Failure> {
    let mut url = Url::parse(spec.url.trim()).map_err(|_| Failure::Request)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Failure::Request);
    }
    if !spec.params.is_empty() {
        let mut query = url.query_pairs_mut();
        for (key, value) in spec.params {
            query.append_pair(&key, &value);
        }
    }
    let method = Method::from_bytes(spec.method.to_ascii_uppercase().as_bytes())
        .map_err(|_| Failure::Request)?;
    let mut header_map = HeaderMap::new();
    for (name, value) in spec.headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| Failure::Request)?;
        let value = HeaderValue::from_str(&value).map_err(|_| Failure::Request)?;
        header_map.insert(name, value);
    }
    if let Some(payload) = spec.payload.as_ref()
        && payload.len() > MAX_REQUEST_BODY_BYTES
    {
        return Err(Failure::BodyTooLarge);
    }
    let auth_kind = spec.authentication.to_ascii_lowercase();
    let response = match auth_kind.as_str() {
        "basic" => {
            let mut request = build_request(
                client,
                &method,
                &url,
                &header_map,
                spec.payload.as_deref(),
                spec.timeout_seconds,
            );
            if !header_map.contains_key(AUTHORIZATION)
                && let (Some(username), Some(password)) =
                    (spec.username.as_deref(), spec.password.as_deref())
            {
                request = request.basic_auth(username, Some(password));
            }
            request.send().await.map_err(map_request_error)?
        }
        "digest" => match spec.username.as_deref().zip(spec.password.as_deref()) {
            None => build_request(
                client,
                &method,
                &url,
                &header_map,
                spec.payload.as_deref(),
                spec.timeout_seconds,
            )
            .send()
            .await
            .map_err(map_request_error)?,
            Some((username, password)) => {
                let challenge_response = build_request(
                    client,
                    &method,
                    &url,
                    &header_map,
                    spec.payload.as_deref(),
                    spec.timeout_seconds,
                )
                .send()
                .await
                .map_err(map_request_error)?;
                if challenge_response.status().as_u16() != 401 {
                    challenge_response
                } else {
                    let challenge = challenge_response
                        .headers()
                        .get(WWW_AUTHENTICATE)
                        .and_then(|value| value.to_str().ok())
                        .ok_or(Failure::UnsupportedAuthentication)?
                        .to_owned();
                    drop(challenge_response);
                    let mut prompt = digest_auth::parse(&challenge)
                        .map_err(|_| Failure::UnsupportedAuthentication)?;
                    let mut digest_uri = url.path().to_owned();
                    if let Some(query) = url.query() {
                        digest_uri.push('?');
                        digest_uri.push_str(query);
                    }
                    let auth_context = digest_auth::AuthContext::new_with_method(
                        username,
                        password,
                        digest_uri,
                        spec.payload.as_ref().map(|body| body.as_bytes()),
                        digest_auth::HttpMethod::from(method.as_str()),
                    );
                    let authorization = prompt
                        .respond(&auth_context)
                        .map_err(|_| Failure::UnsupportedAuthentication)?
                        .to_string();
                    let mut authenticated_headers = header_map.clone();
                    authenticated_headers.insert(
                        AUTHORIZATION,
                        HeaderValue::from_str(&authorization).map_err(|_| Failure::Request)?,
                    );
                    build_request(
                        client,
                        &method,
                        &url,
                        &authenticated_headers,
                        spec.payload.as_deref(),
                        spec.timeout_seconds,
                    )
                    .send()
                    .await
                    .map_err(map_request_error)?
                }
            }
        },
        _ => return Err(Failure::UnsupportedAuthentication),
    };
    let status = response.status().as_u16();
    let mut response_headers = BTreeMap::new();
    for name in response.headers().keys() {
        let values = response
            .headers()
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_owned))
            .collect::<Vec<_>>();
        if !values.is_empty() {
            response_headers.insert(name.as_str().to_owned(), values);
        }
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            if error.is_timeout() {
                Failure::Timeout
            } else {
                Failure::Request
            }
        })?;
        if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err(Failure::BodyTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(HttpResponse {
        status,
        headers: response_headers,
        body,
    })
}

fn build_request(
    client: &Client,
    method: &Method,
    url: &Url,
    headers: &HeaderMap,
    payload: Option<&str>,
    timeout_seconds: u64,
) -> reqwest::RequestBuilder {
    let mut request = client
        .request(method.clone(), url.clone())
        .timeout(Duration::from_secs(timeout_seconds))
        .headers(headers.clone());
    if let Some(payload) = payload {
        request = request.body(payload.to_owned());
    }
    request
}

fn map_request_error(error: reqwest::Error) -> Failure {
    if error.is_timeout() {
        Failure::Timeout
    } else {
        Failure::Request
    }
}

fn sensor_state(
    engine: &TemplateEngine,
    sensor: &RestSensorConfig,
    runtime: &Runtime,
    entity_id: &str,
    body: &str,
    json_value: Option<&Value>,
    sensitive_values: &[String],
) -> std::result::Result<(String, Map<String, Value>), Failure> {
    let previous = runtime.get_state(entity_id);
    let mut variables = Map::new();
    variables.insert("value".into(), Value::String(body.into()));
    if let Some(value) = json_value {
        variables.insert("value_json".into(), value.clone());
    }
    if let Some(previous) = previous {
        variables.insert(
            "this".into(),
            json!({
                "entity_id": previous.entity_id,
                "state": previous.state,
                "attributes": previous.attributes,
            }),
        );
    }

    if let Some(availability) = &sensor.availability {
        let value = render_template_value(engine, availability, &variables)?;
        if !truthy(&value) {
            return Err(Failure::Unavailable);
        }
    }

    let state = match &sensor.value_template {
        Some(template) => {
            let value = render_template_value(engine, template, &variables)?;
            value_to_state(&value).ok_or(Failure::MissingValue)?
        }
        None => {
            let value = body.trim();
            if value.is_empty() {
                return Err(Failure::MissingValue);
            }
            value.to_owned()
        }
    };
    if state.chars().count() > 255 {
        return Err(Failure::InvalidResponse);
    }
    if contains_sensitive_text(&state, sensitive_values) {
        return Err(Failure::MissingValue);
    }

    let name = engine
        .render(&sensor.name, &variables)
        .map_err(|_| Failure::Template)?;
    let name = if name.trim().is_empty() {
        sensor.name.clone()
    } else {
        name
    };
    if contains_sensitive_text(&name, sensitive_values) {
        return Err(Failure::MissingValue);
    }
    let mut attributes = base_attributes(sensor, &name);
    if let Some(json) = json_value {
        if !sensor.json_attributes.is_empty() {
            let selected = match &sensor.json_attributes_path {
                Some(path) => json_path_get(json, path).ok_or(Failure::InvalidResponse)?,
                None => json,
            };
            let object = selected.as_object().ok_or(Failure::InvalidResponse)?;
            for key in &sensor.json_attributes {
                if is_sensitive_key(key) {
                    continue;
                }
                if let Some(value) = object.get(key) {
                    if value_contains_sensitive_data(value, sensitive_values) {
                        continue;
                    }
                    attributes.insert(key.clone(), value.clone());
                }
            }
        }
    } else if !sensor.json_attributes.is_empty() {
        return Err(Failure::InvalidResponse);
    }
    if let Some(icon) = &sensor.icon {
        let value = engine
            .render(icon, &variables)
            .map_err(|_| Failure::Template)?;
        if !value.trim().is_empty() && !contains_sensitive_text(&value, sensitive_values) {
            attributes.insert("icon".into(), Value::String(value));
        }
    }
    if let Some(picture) = &sensor.picture {
        let value = engine
            .render(picture, &variables)
            .map_err(|_| Failure::Template)?;
        if !value.trim().is_empty() && !contains_sensitive_text(&value, sensitive_values) {
            attributes.insert("entity_picture".into(), Value::String(value));
        }
    }
    Ok((state, attributes))
}

fn render_template_value(
    engine: &TemplateEngine,
    template: &str,
    variables: &Map<String, Value>,
) -> std::result::Result<Value, Failure> {
    if let Ok(value) = engine.render_value(template, variables) {
        return Ok(value);
    }
    let rendered = engine
        .render(template, variables)
        .map_err(|_| Failure::Template)?;
    let trimmed = rendered.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null);
    }
    if let Ok(value) = serde_json::from_str(trimmed) {
        Ok(value)
    } else {
        Ok(Value::String(rendered))
    }
}

fn value_to_state(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(value) => {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_owned())
        }
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(value).ok(),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => {
            let value = value.trim();
            !value.is_empty()
                && !matches!(
                    value.to_ascii_lowercase().as_str(),
                    "false" | "off" | "no" | "0"
                )
        }
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "password",
        "token",
        "secret",
        "authorization",
        "api_key",
        "apikey",
        "cookie",
        "rest_error",
    ]
    .iter()
    .any(|part| key.contains(part))
}

fn request_sensitive_values(
    raw_url: &str,
    headers: &BTreeMap<String, String>,
    params: &BTreeMap<String, String>,
    payload: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
) -> Vec<String> {
    let mut sensitive = BTreeSet::new();
    sensitive.extend(headers.values().filter(|value| !value.is_empty()).cloned());
    if let Some(username) = username.filter(|value| !value.is_empty()) {
        sensitive.insert(username.to_owned());
    }
    if let Some(password) = password.filter(|value| !value.is_empty()) {
        sensitive.insert(password.to_owned());
    }
    if let Some(payload) = payload.filter(|value| !value.is_empty()) {
        sensitive.insert(payload.to_owned());
    }
    for (key, value) in params {
        if is_sensitive_key(key) && !value.is_empty() {
            sensitive.insert(value.clone());
        }
    }
    if let Ok(url) = Url::parse(raw_url) {
        for (key, value) in url.query_pairs() {
            if is_sensitive_key(&key) && !value.is_empty() {
                sensitive.insert(value.into_owned());
            }
        }
    }
    sensitive.into_iter().collect()
}

fn contains_sensitive_text(value: &str, sensitive_values: &[String]) -> bool {
    sensitive_values
        .iter()
        .any(|secret| value == secret || (secret.len() >= 8 && value.contains(secret.as_str())))
}

fn value_contains_sensitive_data(value: &Value, sensitive_values: &[String]) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => contains_sensitive_text(&value.to_string(), sensitive_values),
        Value::Number(value) => contains_sensitive_text(&value.to_string(), sensitive_values),
        Value::String(value) => contains_sensitive_text(value, sensitive_values),
        Value::Array(values) => values
            .iter()
            .any(|value| value_contains_sensitive_data(value, sensitive_values)),
        Value::Object(values) => values
            .values()
            .any(|value| value_contains_sensitive_data(value, sensitive_values)),
    }
}

fn parse_json_path(path: &str) -> Result<Vec<JsonPathSegment>> {
    let path = path.trim();
    if path.len() > 4096 || !path.starts_with('$') {
        return Err(invalid_config(
            "json_attributes_path must be a bounded JSONPath",
        ));
    }
    let bytes = path.as_bytes();
    let mut segments = Vec::new();
    let mut index = 1usize;
    while index < bytes.len() {
        match bytes[index] {
            b'.' => {
                index += 1;
                if bytes.get(index) == Some(&b'[') {
                    continue;
                }
                let start = index;
                while index < bytes.len() && bytes[index] != b'.' && bytes[index] != b'[' {
                    index += 1;
                }
                if start == index || &path[start..index] == "*" {
                    return Err(invalid_config(
                        "json_attributes_path uses an unsupported selector",
                    ));
                }
                segments.push(JsonPathSegment::Key(path[start..index].to_owned()));
            }
            b'[' => {
                index += 1;
                if index >= bytes.len() {
                    return Err(invalid_config("json_attributes_path is malformed"));
                }
                let quote = if matches!(bytes[index], b'\'' | b'"') {
                    let quote = bytes[index];
                    index += 1;
                    Some(quote)
                } else {
                    None
                };
                let start = index;
                while index < bytes.len()
                    && match quote {
                        Some(quote) => bytes[index] != quote,
                        None => bytes[index].is_ascii_digit(),
                    }
                {
                    index += 1;
                }
                if start == index {
                    return Err(invalid_config("json_attributes_path is malformed"));
                }
                let value = &path[start..index];
                if let Some(quote) = quote {
                    if bytes.get(index) != Some(&quote) {
                        return Err(invalid_config("json_attributes_path is malformed"));
                    }
                    index += 1;
                    segments.push(JsonPathSegment::Key(value.to_owned()));
                } else {
                    let position = value
                        .parse::<usize>()
                        .map_err(|_| invalid_config("json_attributes_path index is invalid"))?;
                    segments.push(JsonPathSegment::Index(position));
                }
                if bytes.get(index) != Some(&b']') {
                    return Err(invalid_config("json_attributes_path is malformed"));
                }
                index += 1;
            }
            _ => return Err(invalid_config("json_attributes_path is malformed")),
        }
    }
    Ok(segments)
}

#[derive(Clone)]
enum JsonPathSegment {
    Key(String),
    Index(usize),
}

fn json_path_get<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let segments = parse_json_path(path).ok()?;
    let mut current = value;
    for segment in segments {
        current = match segment {
            JsonPathSegment::Key(key) => current.get(&key)?,
            JsonPathSegment::Index(index) => current.get(index)?,
        };
    }
    Some(current)
}

fn make_client(verify_ssl: bool) -> Result<Client> {
    Client::builder()
        .danger_accept_invalid_certs(!verify_ssl)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|_| Error::Service("REST HTTP client could not be created".into()))
}

fn run_command_sync(
    client: Client,
    engine: TemplateEngine,
    config: RestCommandConfig,
    call: ServiceCall,
    permit: tokio::sync::OwnedSemaphorePermit,
    service_name: String,
    mut cancel: watch::Receiver<bool>,
) -> Result<ServiceResult> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("hass-rest-command".into())
        .spawn(move || {
            let _permit = permit;
            let context = call.context.clone();
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| Failure::Request)
                .and_then(|runtime| {
                    runtime.block_on(async move {
                        tokio::select! {
                            biased;
                            changed = cancel.changed() => {
                                let _ = changed;
                                Err(Failure::Request)
                            }
                            result = run_command(client, engine, config, call, service_name) => result,
                        }
                    })
                });
            let _ = sender.send(result.map(|response| (response, context)));
        })
        .map_err(|_| Error::Service("REST command worker could not start".into()))?;
    let received = receiver.recv();
    let joined = worker.join();
    if joined.is_err() {
        return Err(Error::Service("REST command worker failed".into()));
    }
    let (response, context) = received
        .map_err(|_| Error::Service("REST command worker stopped unexpectedly".into()))?
        .map_err(|failure| {
            Error::Service(format!("REST command failed: {}", failure_label(failure)))
        })?;
    Ok(ServiceResult {
        changed_states: Vec::new(),
        response: Some(response),
        context,
    })
}

async fn run_command(
    client: Client,
    engine: TemplateEngine,
    config: RestCommandConfig,
    call: ServiceCall,
    _service_name: String,
) -> std::result::Result<Value, Failure> {
    let mut variables = call.data.clone();
    variables.insert("data".into(), Value::Object(call.data.clone()));
    variables
        .entry("target")
        .or_insert_with(|| Value::Object(call.target.clone()));
    let url = engine
        .render(&config.url, &variables)
        .map_err(|_| Failure::Template)?;
    let mut headers = render_headers(&engine, &config.headers, &variables)?;
    if let Some(content_type) = config.content_type {
        headers.retain(|header, _| !header.eq_ignore_ascii_case("content-type"));
        headers.insert("Content-Type".into(), content_type);
    }
    let payload = config
        .payload
        .as_deref()
        .map(|template| {
            engine
                .render(template, &variables)
                .map_err(|_| Failure::Template)
        })
        .transpose()?;
    let response = send_request(
        &client,
        RequestSpec {
            method: config.method,
            url,
            headers,
            params: BTreeMap::new(),
            payload,
            username: config.username,
            password: config.password,
            authentication: config.authentication,
            timeout_seconds: config.timeout,
        },
    )
    .await?;
    let encoding = response_charset(&response.headers).unwrap_or("UTF-8");
    let body = decode_body(&response.body, encoding)?;
    let content = if response_content_type(&response.headers)
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        serde_json::from_str::<Value>(&body).map_err(|_| Failure::InvalidResponse)?
    } else {
        Value::String(body)
    };
    let mut response_headers = Map::new();
    for (name, value) in response.headers {
        response_headers.insert(
            name,
            if value.len() == 1 {
                Value::String(value.into_iter().next().expect("one header value"))
            } else {
                Value::Array(value.into_iter().map(Value::String).collect())
            },
        );
    }
    Ok(json!({
        "status": response.status,
        "content": content,
        "headers": response_headers,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::extract::State as AxumState;
    use axum::http::{HeaderMap as AxumHeaders, StatusCode as AxumStatus};
    use axum::response::{IntoResponse, Response as AxumResponse};
    use axum::routing::{any, get};
    use axum::{Json, Router};
    use hass_storage::SqliteStore;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    #[derive(Clone)]
    struct FixtureState {
        mode: Arc<AtomicUsize>,
        requests: Arc<AtomicUsize>,
        network_requests: Arc<AtomicUsize>,
        slow_requests: Arc<AtomicUsize>,
        sensor_authorization: Arc<std::sync::Mutex<Option<String>>>,
        sensor_body: Arc<std::sync::Mutex<Vec<u8>>>,
        command_body: Arc<std::sync::Mutex<Vec<u8>>>,
        command_headers: Arc<std::sync::Mutex<Option<String>>>,
        command_content_type: Arc<std::sync::Mutex<Option<String>>>,
        digest_authorization: Arc<std::sync::Mutex<Option<String>>>,
    }

    async fn fixture() -> (String, FixtureState, JoinHandle<()>) {
        let state = FixtureState {
            mode: Arc::new(AtomicUsize::new(1)),
            requests: Arc::new(AtomicUsize::new(0)),
            network_requests: Arc::new(AtomicUsize::new(0)),
            slow_requests: Arc::new(AtomicUsize::new(0)),
            sensor_authorization: Arc::new(std::sync::Mutex::new(None)),
            sensor_body: Arc::new(std::sync::Mutex::new(Vec::new())),
            command_body: Arc::new(std::sync::Mutex::new(Vec::new())),
            command_headers: Arc::new(std::sync::Mutex::new(None)),
            command_content_type: Arc::new(std::sync::Mutex::new(None)),
            digest_authorization: Arc::new(std::sync::Mutex::new(None)),
        };
        let app = Router::new()
            .route("/sensor", any(sensor_response))
            .route("/missing", get(missing_response))
            .route("/slow", get(slow_response))
            .route("/command", any(command_response))
            .route("/digest", get(digest_response))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}"), state, server)
    }

    async fn sensor_response(
        AxumState(state): AxumState<FixtureState>,
        headers: AxumHeaders,
        body: Bytes,
    ) -> (AxumStatus, Json<Value>) {
        state.requests.fetch_add(1, Ordering::SeqCst);
        state.network_requests.fetch_add(1, Ordering::SeqCst);
        *state.sensor_body.lock().unwrap() = body.to_vec();
        *state.sensor_authorization.lock().unwrap() = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if state.mode.load(Ordering::SeqCst) == 0 {
            return (AxumStatus::SERVICE_UNAVAILABLE, Json(json!({"state": 99})));
        }
        (
            AxumStatus::OK,
            Json(
                json!({"state": 23, "meta": {"unit": "C", "token": "must-not-persist", "zero": 0, "echo": "Bearer loopback-secret-token"}}),
            ),
        )
    }

    async fn missing_response(AxumState(state): AxumState<FixtureState>) -> Json<Value> {
        state.requests.fetch_add(1, Ordering::SeqCst);
        state.network_requests.fetch_add(1, Ordering::SeqCst);
        Json(json!({"meta": {"unit": "C"}}))
    }

    async fn slow_response(AxumState(state): AxumState<FixtureState>) -> Json<Value> {
        state.slow_requests.fetch_add(1, Ordering::SeqCst);
        state.network_requests.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        Json(json!({"state": 7}))
    }

    async fn command_response(
        AxumState(state): AxumState<FixtureState>,
        headers: AxumHeaders,
        body: Bytes,
    ) -> AxumResponse {
        state.network_requests.fetch_add(1, Ordering::SeqCst);
        *state.command_body.lock().unwrap() = body.to_vec();
        *state.command_headers.lock().unwrap() = headers
            .get("x-fixture")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        *state.command_content_type.lock().unwrap() = headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut response = Json(json!({"accepted": true})).into_response();
        response.headers_mut().insert(
            "content-type",
            "application/json; charset=utf-8".parse().unwrap(),
        );
        response
    }

    async fn digest_response(
        AxumState(state): AxumState<FixtureState>,
        headers: AxumHeaders,
    ) -> AxumResponse {
        state.network_requests.fetch_add(1, Ordering::SeqCst);
        if let Some(authorization) = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
        {
            *state.digest_authorization.lock().unwrap() = Some(authorization.to_owned());
            return Json(json!({"authenticated": authorization.starts_with("Digest ")}))
                .into_response();
        }
        let mut response = AxumStatus::UNAUTHORIZED.into_response();
        response.headers_mut().insert(
            "www-authenticate",
            "Digest realm=\"fixture\", nonce=\"fixture-nonce\", qop=\"auth\", algorithm=MD5"
                .parse()
                .unwrap(),
        );
        response
    }

    fn runtime() -> Runtime {
        Runtime::new(Arc::new(SqliteStore::in_memory().unwrap())).unwrap()
    }

    fn sensor_config(url: String) -> RestConfig {
        RestConfig {
            resource: Some(url),
            resource_template: None,
            method: "GET".into(),
            headers: BTreeMap::from([(
                "Authorization".into(),
                "Bearer loopback-secret-token".into(),
            )]),
            params: BTreeMap::new(),
            payload: None,
            payload_template: None,
            authentication: "basic".into(),
            username: None,
            password: None,
            timeout: 1,
            scan_interval: 1,
            verify_ssl: true,
            encoding: "UTF-8".into(),
            ssl_cipher_list: "python_default".into(),
            sensor: vec![RestSensorConfig {
                name: "Fixture Temperature".into(),
                value_template: Some("{{ value_json.state }}".into()),
                availability: Some("{{ value_json.state > 0 }}".into()),
                json_attributes: vec!["unit".into(), "zero".into(), "token".into(), "echo".into()],
                json_attributes_path: Some("$.meta".into()),
                device_class: Some("temperature".into()),
                state_class: Some("measurement".into()),
                unit_of_measurement: Some("°C".into()),
                icon: None,
                picture: None,
                unique_id: Some("fixture_temperature".into()),
                force_update: false,
            }],
        }
    }

    fn command_config(url: String) -> RestCommandConfig {
        RestCommandConfig {
            url,
            method: "GET".into(),
            headers: BTreeMap::new(),
            payload: None,
            authentication: "basic".into(),
            username: None,
            password: None,
            timeout: 2,
            content_type: None,
            verify_ssl: true,
            insecure_cipher: false,
            skip_url_encoding: false,
        }
    }

    fn wait_state(runtime: &Runtime, entity_id: &str, expected: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline {
            if runtime
                .get_state(entity_id)
                .is_some_and(|state| state.state == expected)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("sensor {entity_id} did not reach state {expected:?}");
    }

    fn wait_error(runtime: &Runtime, entity_id: &str, expected: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline {
            if runtime
                .get_state(entity_id)
                .is_some_and(|state| state.attributes.get("rest_error") == Some(&json!(expected)))
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("sensor {entity_id} did not report REST error {expected:?}");
    }

    fn wait_counter(counter: &AtomicUsize, minimum: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline {
            if counter.load(Ordering::SeqCst) >= minimum {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("fixture request count did not reach {minimum}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn polls_real_http_and_command_sends_rendered_payload() {
        let (base, fixture, server) = fixture().await;
        let runtime = runtime();
        let mut config = sensor_config(format!("{base}/sensor"));
        config.method = "POST".into();
        config.payload_template = Some("{\"probe\": {{ 17 }}}".into());
        config.encoding = "windows-1252".into();
        let command = RestCommandConfig {
            url: format!("{base}/command?name={{{{ name }}}}"),
            method: "POST".into(),
            headers: BTreeMap::from([
                ("X-Fixture".into(), "{{ marker }}".into()),
                ("content-type".into(), "text/plain".into()),
            ]),
            payload: Some("{\"target\": {{ target }}}".into()),
            authentication: "basic".into(),
            username: None,
            password: None,
            timeout: 2,
            content_type: Some("application/json".into()),
            verify_ssl: true,
            insecure_cipher: false,
            skip_url_encoding: false,
        };
        let restart_command = command.clone();
        let digest_command = RestCommandConfig {
            url: format!("{base}/digest"),
            method: "GET".into(),
            headers: BTreeMap::new(),
            payload: None,
            authentication: "digest".into(),
            username: Some("fixture-user".into()),
            password: Some("fixture-password".into()),
            timeout: 2,
            content_type: None,
            verify_ssl: true,
            insecure_cipher: false,
            skip_url_encoding: false,
        };
        let integration = RestIntegration::start(
            runtime.clone(),
            vec![config],
            BTreeMap::from([
                ("send_fixture".into(), command),
                ("digest_fixture".into(), digest_command),
            ]),
        )
        .await
        .unwrap();
        wait_state(&runtime, "sensor.fixture_temperature", "23");
        let state = runtime.get_state("sensor.fixture_temperature").unwrap();
        assert_eq!(
            state.attributes.get("unit"),
            Some(&Value::String("C".into()))
        );
        assert_eq!(state.attributes.get("zero"), Some(&json!(0)));
        assert!(!state.attributes.contains_key("token"));
        assert!(!state.attributes.contains_key("echo"));
        assert_eq!(
            fixture.sensor_authorization.lock().unwrap().as_deref(),
            Some("Bearer loopback-secret-token")
        );
        assert_eq!(
            String::from_utf8(fixture.sensor_body.lock().unwrap().clone()).unwrap(),
            r#"{"probe": 17}"#
        );

        let call = ServiceCall {
            domain: "rest_command".into(),
            service: "send_fixture".into(),
            data: Map::from_iter([
                ("name".into(), json!("living room")),
                ("marker".into(), json!("present")),
                ("target".into(), json!(42)),
            ]),
            target: Map::new(),
            context: Context::default(),
        };
        let result = tokio::task::spawn_blocking({
            let runtime = runtime.clone();
            move || runtime.call_service(call)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.response.as_ref().unwrap()["status"], 200);
        assert_eq!(
            result.response.as_ref().unwrap()["content"]["accepted"],
            true
        );
        assert_eq!(
            String::from_utf8(fixture.command_body.lock().unwrap().clone()).unwrap(),
            r#"{"target": 42}"#
        );
        assert_eq!(
            fixture.command_headers.lock().unwrap().as_deref(),
            Some("present")
        );
        assert_eq!(
            fixture.command_content_type.lock().unwrap().as_deref(),
            Some("application/json")
        );

        let digest_call = ServiceCall {
            domain: "rest_command".into(),
            service: "digest_fixture".into(),
            data: Map::new(),
            target: Map::new(),
            context: Context::default(),
        };
        let digest_result = tokio::task::spawn_blocking({
            let runtime = runtime.clone();
            move || runtime.call_service(digest_call)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(digest_result.response.as_ref().unwrap()["status"], 200);
        let authorization = fixture
            .digest_authorization
            .lock()
            .unwrap()
            .clone()
            .expect("Digest header was sent");
        assert!(authorization.starts_with("Digest username=\"fixture-user\""));
        assert!(!authorization.contains("fixture-password"));

        integration.shutdown().await.unwrap();
        let previous_requests = fixture.requests.load(Ordering::SeqCst);
        let restarted = RestIntegration::start(
            runtime.clone(),
            vec![sensor_config(format!("{base}/sensor"))],
            BTreeMap::from([("send_fixture".into(), restart_command)]),
        )
        .await
        .unwrap();
        wait_counter(&fixture.requests, previous_requests + 1);
        wait_state(&runtime, "sensor.fixture_temperature", "23");
        let restart_call = ServiceCall {
            domain: "rest_command".into(),
            service: "send_fixture".into(),
            data: Map::from_iter([("target".into(), json!(7))]),
            target: Map::new(),
            context: Context::default(),
        };
        let restart_result = tokio::task::spawn_blocking({
            let runtime = runtime.clone();
            move || runtime.call_service(restart_call)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(restart_result.response.as_ref().unwrap()["status"], 200);
        assert_eq!(
            String::from_utf8(fixture.command_body.lock().unwrap().clone()).unwrap(),
            r#"{"target": 7}"#
        );
        restarted.shutdown().await.unwrap();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_failure_becomes_unavailable_and_recovers() {
        let (base, fixture, server) = fixture().await;
        fixture.mode.store(0, Ordering::SeqCst);
        let runtime = runtime();
        let integration = RestIntegration::start(
            runtime.clone(),
            vec![sensor_config(format!("{base}/sensor"))],
            BTreeMap::new(),
        )
        .await
        .unwrap();
        wait_error(&runtime, "sensor.fixture_temperature", "http_status");
        assert!(fixture.requests.load(Ordering::SeqCst) > 0);
        fixture.mode.store(1, Ordering::SeqCst);
        wait_state(&runtime, "sensor.fixture_temperature", "23");
        assert!(
            !runtime
                .get_state("sensor.fixture_temperature")
                .unwrap()
                .attributes
                .contains_key("rest_error")
        );
        integration.shutdown().await.unwrap();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_marks_sensor_unavailable() {
        let (base, fixture, server) = fixture().await;
        let runtime = runtime();
        let mut config = sensor_config(format!("{base}/slow"));
        config.timeout = 1;
        let integration = RestIntegration::start(runtime.clone(), vec![config], BTreeMap::new())
            .await
            .unwrap();
        wait_error(&runtime, "sensor.fixture_temperature", "timeout");
        assert!(fixture.slow_requests.load(Ordering::SeqCst) > 0);
        integration.shutdown().await.unwrap();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_template_reading_stays_unavailable_instead_of_becoming_zero() {
        let (base, fixture, server) = fixture().await;
        let runtime = runtime();
        let mut config = sensor_config(format!("{base}/missing"));
        config.sensor[0].value_template = Some("{{ value_json.missing }}".into());
        config.sensor[0].availability = None;
        let integration = RestIntegration::start(runtime.clone(), vec![config], BTreeMap::new())
            .await
            .unwrap();
        wait_error(&runtime, "sensor.fixture_temperature", "missing_value");
        assert!(fixture.requests.load(Ordering::SeqCst) > 0);
        assert_eq!(
            runtime
                .get_state("sensor.fixture_temperature")
                .unwrap()
                .state,
            "unavailable"
        );
        integration.shutdown().await.unwrap();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_cancels_inflight_poll() {
        let (base, fixture, server) = fixture().await;
        let runtime = runtime();
        let mut config = sensor_config(format!("{base}/slow"));
        config.timeout = 10;
        let integration = RestIntegration::start(runtime, vec![config], BTreeMap::new())
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while fixture.slow_requests.load(Ordering::SeqCst) == 0
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(fixture.slow_requests.load(Ordering::SeqCst) > 0);
        let before = tokio::time::Instant::now();
        integration.shutdown().await.unwrap();
        assert!(before.elapsed() < Duration::from_secs(1));
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_cancels_and_joins_inflight_command() {
        let (base, fixture, server) = fixture().await;
        let runtime = runtime();
        let command = RestCommandConfig {
            url: format!("{base}/slow"),
            method: "GET".into(),
            headers: BTreeMap::new(),
            payload: None,
            authentication: "basic".into(),
            username: None,
            password: None,
            timeout: 10,
            content_type: None,
            verify_ssl: true,
            insecure_cipher: false,
            skip_url_encoding: false,
        };
        let integration = RestIntegration::start(
            runtime.clone(),
            Vec::new(),
            BTreeMap::from([("slow_command".into(), command)]),
        )
        .await
        .unwrap();
        let service_call = ServiceCall {
            domain: "rest_command".into(),
            service: "slow_command".into(),
            data: Map::new(),
            target: Map::new(),
            context: Context::default(),
        };
        let service_runtime = runtime.clone();
        let command_task =
            tokio::task::spawn_blocking(move || service_runtime.call_service(service_call));
        wait_counter(&fixture.slow_requests, 1);
        let before = tokio::time::Instant::now();
        integration.shutdown().await.unwrap();
        assert!(before.elapsed() < Duration::from_secs(1));
        assert!(command_task.await.unwrap().is_err());
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_late_resource_validation_leaves_no_commands_or_pollers() {
        let (base, fixture, server) = fixture().await;
        let runtime = runtime();
        let mut invalid_later_resource = sensor_config(format!("{base}/sensor"));
        invalid_later_resource.sensor[0].name = "A Second Fixture Temperature".into();
        invalid_later_resource.sensor[0].unique_id = Some("second_fixture_temperature".into());
        invalid_later_resource.scan_interval = 0;

        let result = RestIntegration::start(
            runtime.clone(),
            vec![
                sensor_config(format!("{base}/sensor")),
                invalid_later_resource,
            ],
            BTreeMap::from([(
                "would_register".into(),
                command_config(format!("{base}/command")),
            )]),
        )
        .await;
        assert!(result.is_err());
        let call = ServiceCall {
            domain: "rest_command".into(),
            service: "would_register".into(),
            data: Map::new(),
            target: Map::new(),
            context: Context::default(),
        };
        assert!(runtime.call_service(call).is_err());
        assert_eq!(fixture.network_requests.load(Ordering::SeqCst), 0);
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert_eq!(fixture.network_requests.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_command_registration_is_rolled_back_without_removing_other_services() {
        let (base, fixture, server) = fixture().await;
        let runtime = runtime();
        runtime
            .register_service(
                "rest_command",
                "z_conflict",
                json!({"name": "existing"}),
                Arc::new(|_, call| {
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: Some(json!({"existing": true})),
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        let mut resource = sensor_config(format!("{base}/sensor"));
        resource.sensor[0].name = "Poller must not start".into();

        let result = RestIntegration::start(
            runtime.clone(),
            vec![resource],
            BTreeMap::from([
                (
                    "a_registered_first".into(),
                    command_config(format!("{base}/command")),
                ),
                (
                    "z_conflict".into(),
                    command_config(format!("{base}/command")),
                ),
            ]),
        )
        .await;
        assert!(result.is_err());

        let rolled_back_call = ServiceCall {
            domain: "rest_command".into(),
            service: "a_registered_first".into(),
            data: Map::new(),
            target: Map::new(),
            context: Context::default(),
        };
        assert!(runtime.call_service(rolled_back_call).is_err());
        let existing_call = ServiceCall {
            domain: "rest_command".into(),
            service: "z_conflict".into(),
            data: Map::new(),
            target: Map::new(),
            context: Context::default(),
        };
        assert_eq!(
            runtime
                .call_service(existing_call)
                .unwrap()
                .response
                .unwrap(),
            json!({"existing": true})
        );
        assert_eq!(fixture.network_requests.load(Ordering::SeqCst), 0);
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert_eq!(fixture.network_requests.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_unregisters_commands_and_reopens_persisted_runtime_without_network_activity()
    {
        let (base, fixture, server) = fixture().await;
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("rest-shutdown.sqlite3");
        let store = Arc::new(SqliteStore::open(&database).unwrap());
        let runtime = Runtime::new(store.clone()).unwrap();
        let integration = RestIntegration::start(
            runtime.clone(),
            vec![sensor_config(format!("{base}/sensor"))],
            BTreeMap::from([(
                "shutdown_command".into(),
                command_config(format!("{base}/command")),
            )]),
        )
        .await
        .unwrap();
        wait_state(&runtime, "sensor.fixture_temperature", "23");

        integration.shutdown().await.unwrap();
        let service_call = ServiceCall {
            domain: "rest_command".into(),
            service: "shutdown_command".into(),
            data: Map::new(),
            target: Map::new(),
            context: Context::default(),
        };
        assert!(runtime.call_service(service_call).is_err());
        let requests_at_shutdown = fixture.network_requests.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert_eq!(
            fixture.network_requests.load(Ordering::SeqCst),
            requests_at_shutdown
        );

        drop(runtime);
        assert_eq!(
            Arc::strong_count(&store),
            1,
            "REST handlers retained Runtime"
        );
        drop(store);
        let reopened = Runtime::new(Arc::new(SqliteStore::open(&database).unwrap())).unwrap();
        assert_eq!(
            reopened
                .get_state("sensor.fixture_temperature")
                .unwrap()
                .state,
            "23"
        );
        assert!(
            reopened
                .call_service(ServiceCall {
                    domain: "rest_command".into(),
                    service: "shutdown_command".into(),
                    data: Map::new(),
                    target: Map::new(),
                    context: Context::default(),
                })
                .is_err()
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn implicit_drop_unregisters_commands_cancels_poller_and_reopens_sqlite() {
        let (base, fixture, server) = fixture().await;
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("rest-drop.sqlite3");
        let store = Arc::new(SqliteStore::open(&database).unwrap());
        let runtime = Runtime::new(store.clone()).unwrap();
        let integration = RestIntegration::start(
            runtime.clone(),
            vec![sensor_config(format!("{base}/sensor"))],
            BTreeMap::from([(
                "drop_command".into(),
                command_config(format!("{base}/command")),
            )]),
        )
        .await
        .unwrap();
        wait_state(&runtime, "sensor.fixture_temperature", "23");

        drop(integration);
        assert!(
            runtime
                .call_service(ServiceCall {
                    domain: "rest_command".into(),
                    service: "drop_command".into(),
                    data: Map::new(),
                    target: Map::new(),
                    context: Context::default(),
                })
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let requests_after_drop = fixture.network_requests.load(Ordering::SeqCst);
        drop(runtime);
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert_eq!(
            fixture.network_requests.load(Ordering::SeqCst),
            requests_after_drop
        );
        assert_eq!(
            Arc::strong_count(&store),
            1,
            "REST handlers retained Runtime"
        );
        drop(store);
        let reopened = Runtime::new(Arc::new(SqliteStore::open(&database).unwrap())).unwrap();
        assert_eq!(
            reopened
                .get_state("sensor.fixture_temperature")
                .unwrap()
                .state,
            "23"
        );
        assert!(
            reopened
                .call_service(ServiceCall {
                    domain: "rest_command".into(),
                    service: "drop_command".into(),
                    data: Map::new(),
                    target: Map::new(),
                    context: Context::default(),
                })
                .is_err()
        );
        server.abort();
    }

    #[test]
    fn config_debug_redacts_request_secrets_and_unknown_fields_are_rejected() {
        let mut config = sensor_config("http://example.invalid/?token=private".into());
        config
            .headers
            .insert("Authorization".into(), "Bearer header-secret".into());
        config.password = Some("password-secret".into());
        config.payload = Some("token=body-secret".into());
        let rendered = format!("{config:?}");
        for secret in [
            "private",
            "header-secret",
            "password-secret",
            "body-secret",
            "loopback-secret-token",
        ] {
            assert!(!rendered.contains(secret));
        }
        assert!(
            serde_yaml::from_str::<RestCommandConfig>("url: http://127.0.0.1\nextra: true\n")
                .is_err()
        );
        let yaml = "resource: http://127.0.0.1/data\nscan_interval: '00:00:15'\npayload_template: '{{ value }}'\nsensor:\n  - name: YAML sensor\n    json_attributes: unit,zero\n";
        let parsed: RestConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.scan_interval, 15);
        assert_eq!(parsed.sensor[0].json_attributes, vec!["unit", "zero"]);
        assert_eq!(parsed.ssl_cipher_list, "python_default");
        validate_config(&[parsed], &BTreeMap::new()).unwrap();
        let list_yaml = "resource: http://127.0.0.1/data\nencoding: windows-1252\nsensor:\n  - name: YAML list sensor\n    json_attributes: [unit, zero]\n";
        let parsed_list: RestConfig = serde_yaml::from_str(list_yaml).unwrap();
        assert_eq!(parsed_list.sensor[0].json_attributes, vec!["unit", "zero"]);
        assert_eq!(parsed_list.encoding, "windows-1252");
        validate_config(std::slice::from_ref(&parsed_list), &BTreeMap::new()).unwrap();

        let mut unsupported_cipher = parsed_list;
        unsupported_cipher.ssl_cipher_list = "insecure".into();
        assert!(validate_config(&[unsupported_cipher], &BTreeMap::new()).is_err());
        assert!(serde_yaml::from_str::<RestConfig>(
            "resource: http://127.0.0.1/data\ncipher: DEFAULT\nsensor:\n  - name: Invalid cipher sensor\n"
        )
        .is_err());

        let command = RestCommandConfig {
            url: "http://127.0.0.1/command".into(),
            method: "POST".into(),
            headers: BTreeMap::new(),
            payload: None,
            authentication: "basic".into(),
            username: None,
            password: None,
            timeout: 10,
            content_type: None,
            verify_ssl: true,
            insecure_cipher: false,
            skip_url_encoding: false,
        };
        let commands = BTreeMap::from([("example".into(), command.clone())]);
        validate_config(&[], &commands).unwrap();
        let mut insecure_cipher = command.clone();
        insecure_cipher.insecure_cipher = true;
        assert!(
            validate_config(&[], &BTreeMap::from([("example".into(), insecure_cipher)])).is_err()
        );
        let mut skip_url_encoding = command;
        skip_url_encoding.skip_url_encoding = true;
        assert!(
            validate_config(
                &[],
                &BTreeMap::from([("example".into(), skip_url_encoding)])
            )
            .is_err()
        );

        let headers = BTreeMap::from([(
            "content-type".into(),
            vec!["text/plain; charset=windows-1252".into()],
        )]);
        let response_encoding = response_charset(&headers).unwrap();
        assert_eq!(
            decode_body(&[0xe9], response_encoding).as_deref().ok(),
            Some("é")
        );
        assert_eq!(
            decode_body(&[0xe9], "windows-1252").as_deref().ok(),
            Some("é")
        );
        let nested = json!([{"address": "Main St"}]);
        assert_eq!(
            json_path_get(&nested, "$.[0].address"),
            Some(&json!("Main St"))
        );
    }
}
