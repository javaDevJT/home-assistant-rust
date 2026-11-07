//! Native System Health provider registration and cancellable collection.
//!
//! This crate ports the provider and collection behavior from Home Assistant
//! Core source commit `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. It does not
//! discover Python platforms, install API routes, or publish runtime events.

use std::{
    collections::BTreeMap,
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    task::{Context as TaskContext, Poll},
    time::Duration,
};

use chrono::{DateTime, Utc};
use futures_util::{
    FutureExt,
    stream::{FuturesUnordered, StreamExt},
};
use hass_core::{Error, Result};
use serde::{Serialize, Serializer, ser::SerializeMap};
use serde_json::{Map, Value, json};

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5);
const REACHABILITY_TIMEOUT: Duration = Duration::from_secs(5);

/// An owned, sendable future used by System Health providers.
pub type HealthFuture<T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'static>>;

/// A domain's provider fields.
pub type SystemHealthInfo = BTreeMap<String, SystemHealthValue>;

/// Callback used to collect one domain's provider fields.
pub type SystemHealthCallback =
    Arc<dyn Fn() -> HealthFuture<SystemHealthInfo> + Send + Sync + 'static>;

/// A resolved provider value, retaining datetime identity through transport.
#[derive(Clone, Debug, PartialEq)]
pub enum SystemHealthData {
    /// An arbitrary JSON value supplied by the provider.
    Json(Value),
    /// A UTC datetime encoded using the System Health date transport shape.
    DateTime(DateTime<Utc>),
}

impl SystemHealthData {
    /// Convert a resolved value to the System Health WebSocket representation.
    pub fn to_transport_value(&self) -> Value {
        match self {
            Self::Json(value) => value.clone(),
            Self::DateTime(value) => json!({
                "value": isoformat(value),
                "type": "date",
            }),
        }
    }

    fn to_get_info_value(&self) -> Value {
        match self {
            Self::Json(value) => get_info_value(value.clone()),
            Self::DateTime(value) => Value::String(isoformat(value)),
        }
    }
}

/// A provider field that is immediate or resolved asynchronously after the
/// initial response.
pub enum SystemHealthValue {
    /// A value available for the initial event.
    Immediate(SystemHealthData),
    /// A value represented as pending, then delivered as an update.
    Deferred(HealthFuture<SystemHealthData>),
}

impl SystemHealthValue {
    /// Transfer a spawned provider task into this report's cancellation scope.
    ///
    /// Dropping a raw Tokio JoinHandle detaches its task. Use this adapter for
    /// spawned work so dropping the session also requests task cancellation.
    /// Other deferred futures must own and cancel any work they start.
    pub fn from_task(task: tokio::task::JoinHandle<Result<SystemHealthData>>) -> Self {
        Self::Deferred(Box::pin(OwnedHealthTask { task }))
    }
}

struct OwnedHealthTask {
    task: tokio::task::JoinHandle<Result<SystemHealthData>>,
}

impl Future for OwnedHealthTask {
    type Output = Result<SystemHealthData>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.task).poll(cx) {
            Poll::Ready(Ok(value)) => Poll::Ready(value),
            Poll::Ready(Err(error)) => Poll::Ready(Err(Error::Service(format!(
                "System Health provider task failed: {error}"
            )))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for OwnedHealthTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Map returned by the internal [`SystemHealthManager::get_info`] helper.
pub type SystemHealthInfoMap = BTreeMap<String, BTreeMap<String, Value>>;

/// Initial per-domain response data, serialized in the source wire shape.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemHealthDomainData {
    /// Initial fields, including explicit pending markers for deferred values.
    pub info: BTreeMap<String, SystemHealthInitialValue>,
    /// Optional integration management URL. Empty URLs are omitted on wire.
    pub manage_url: Option<String>,
}

impl Serialize for SystemHealthDomainData {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let include_url = self
            .manage_url
            .as_deref()
            .is_some_and(|manage_url| !manage_url.is_empty());
        let mut map = serializer.serialize_map(Some(1 + usize::from(include_url)))?;
        map.serialize_entry("info", &self.info)?;
        if include_url {
            map.serialize_entry("manage_url", &self.manage_url)?;
        }
        map.end()
    }
}

/// Value shape used in the initial event.
#[derive(Debug, Clone, PartialEq)]
pub enum SystemHealthInitialValue {
    /// An immediate JSON value.
    Json(Value),
    /// An immediate datetime.
    DateTime(DateTime<Utc>),
    /// A deferred value that will be emitted as a later update.
    Pending,
}

impl SystemHealthInitialValue {
    /// Convert the initial value to the System Health WebSocket representation.
    pub fn to_transport_value(&self) -> Value {
        match self {
            Self::Json(value) => value.clone(),
            Self::DateTime(value) => SystemHealthData::DateTime(*value).to_transport_value(),
            Self::Pending => json!({"type": "pending"}),
        }
    }

    fn to_get_info_value(&self) -> Option<Value> {
        match self {
            Self::Json(value) => Some(get_info_value(value.clone())),
            Self::DateTime(value) => Some(Value::String(isoformat(value))),
            Self::Pending => None,
        }
    }
}

impl Serialize for SystemHealthInitialValue {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.to_transport_value().serialize(serializer)
    }
}

/// Session update outcome. Failed updates are deliberately generic on the
/// public API; provider error details remain private to the internal helper and
/// tracing diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub enum SystemHealthUpdateResult {
    /// Deferred value completed successfully.
    Success(SystemHealthData),
    /// Deferred value failed; the WebSocket adapter should send generic
    /// `{"type":"failed","error":"unknown"}` data.
    Failed,
}

/// One deferred field completion, yielded in completion order.
pub struct SystemHealthUpdate {
    /// Provider domain.
    pub domain: String,
    /// Provider field name.
    pub key: String,
    /// Public transport outcome, with failures already generalized.
    pub result: SystemHealthUpdateResult,
    error_detail: Option<String>,
}

impl SystemHealthUpdate {
    /// Convert an update to the source System Health event envelope.
    ///
    /// This only emits generic errors; internal exception details are never
    /// included in this JSON value.
    pub fn to_transport_event(&self) -> Value {
        match &self.result {
            SystemHealthUpdateResult::Success(data) => json!({
                "type": "update",
                "domain": self.domain,
                "key": self.key,
                "success": true,
                "data": data.to_transport_value(),
            }),
            SystemHealthUpdateResult::Failed => json!({
                "type": "update",
                "domain": self.domain,
                "key": self.key,
                "success": false,
                "error": {"type": "failed", "error": "unknown"},
            }),
        }
    }
}

impl std::fmt::Debug for SystemHealthUpdate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SystemHealthUpdate")
            .field("domain", &self.domain)
            .field("key", &self.key)
            .field("result", &self.result)
            .finish()
    }
}

type DeferredUpdate = Pin<Box<dyn Future<Output = SystemHealthUpdate> + Send + 'static>>;

/// Owned initial response and deferred-update stream.
///
/// No work is detached into a Tokio task. Dropping this session drops every
/// outstanding deferred future promptly.
pub struct SystemHealthSession {
    /// Initial data keyed by sorted integration domain.
    pub initial: BTreeMap<String, SystemHealthDomainData>,
    updates: FuturesUnordered<DeferredUpdate>,
}

impl SystemHealthSession {
    /// Wait for the next deferred value, or return `None` when all have settled.
    pub async fn next_update(&mut self) -> Option<SystemHealthUpdate> {
        self.updates.next().await
    }
}

struct Registration {
    callback: SystemHealthCallback,
    manage_url: Option<String>,
}

struct ManagerInner {
    registrations: Mutex<BTreeMap<String, Registration>>,
}

/// Strong owner for native System Health callback registrations.
///
/// The application should retain this for as long as API consumers use the
/// weak manager handle.
pub struct SystemHealthIntegration {
    inner: Arc<ManagerInner>,
}

impl SystemHealthIntegration {
    /// Create an empty provider registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                registrations: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Return a weak handle suitable for passing to API features.
    pub fn manager(&self) -> SystemHealthManager {
        SystemHealthManager {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Register or replace the callback and management URL for `domain`.
    pub fn register_info(
        &self,
        domain: impl Into<String>,
        callback: SystemHealthCallback,
        manage_url: Option<String>,
    ) -> Result<()> {
        self.manager().register_info(domain, callback, manage_url)
    }
}

impl Default for SystemHealthIntegration {
    fn default() -> Self {
        Self::new()
    }
}

/// Weak handle to the app-owned System Health registry.
#[derive(Clone)]
pub struct SystemHealthManager {
    inner: Weak<ManagerInner>,
}

impl SystemHealthManager {
    /// Register or replace the callback and management URL for `domain`.
    pub fn register_info(
        &self,
        domain: impl Into<String>,
        callback: SystemHealthCallback,
        manage_url: Option<String>,
    ) -> Result<()> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| Error::NotFound("system_health integration".to_owned()))?;
        let mut registrations = inner
            .registrations
            .lock()
            .map_err(|_| Error::Service("system_health registry is unavailable".to_owned()))?;
        registrations.insert(
            domain.into(),
            Registration {
                callback,
                manage_url,
            },
        );
        Ok(())
    }

    /// Run all provider callbacks concurrently and return the initial data and
    /// owned stream of deferred completions.
    pub async fn collect(&self) -> Result<SystemHealthSession> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| Error::NotFound("system_health integration".to_owned()))?;
        let registrations = inner
            .registrations
            .lock()
            .map_err(|_| Error::Service("system_health registry is unavailable".to_owned()))?
            .iter()
            .map(|(domain, registration)| {
                (
                    domain.clone(),
                    registration.callback.clone(),
                    registration.manage_url.clone(),
                )
            })
            .collect::<Vec<_>>();
        drop(inner);

        let mut callbacks = FuturesUnordered::new();
        for (domain, callback, manage_url) in registrations {
            callbacks.push(async move {
                let result = tokio::time::timeout(
                    CALLBACK_TIMEOUT,
                    catch_provider_panic(async move { callback().await }),
                )
                .await;
                (domain, manage_url, result)
            });
        }

        let mut initial = BTreeMap::new();
        let updates = FuturesUnordered::<DeferredUpdate>::new();
        while let Some((domain, manage_url, result)) = callbacks.next().await {
            let mut info = BTreeMap::new();
            match result {
                Ok(Ok(provider_info)) => {
                    for (key, value) in provider_info {
                        match value {
                            SystemHealthValue::Immediate(SystemHealthData::Json(value)) => {
                                info.insert(key, SystemHealthInitialValue::Json(value));
                            }
                            SystemHealthValue::Immediate(SystemHealthData::DateTime(value)) => {
                                info.insert(key, SystemHealthInitialValue::DateTime(value));
                            }
                            SystemHealthValue::Deferred(future) => {
                                info.insert(key.clone(), SystemHealthInitialValue::Pending);
                                let update_domain = domain.clone();
                                let update_key = key.clone();
                                updates.push(Box::pin(async move {
                                    match catch_provider_panic(future).await {
                                        Ok(value) => SystemHealthUpdate {
                                            domain: update_domain,
                                            key: update_key,
                                            result: SystemHealthUpdateResult::Success(value),
                                            error_detail: None,
                                        },
                                        Err(error) => {
                                            tracing::error!(
                                                domain = %update_domain,
                                                key = %update_key,
                                                error = %error,
                                                "Error fetching deferred system health value"
                                            );
                                            SystemHealthUpdate {
                                                domain: update_domain,
                                                key: update_key,
                                                result: SystemHealthUpdateResult::Failed,
                                                error_detail: Some(error.to_string()),
                                            }
                                        }
                                    }
                                }));
                            }
                        }
                    }
                }
                Ok(Err(error)) => {
                    tracing::error!(domain = %domain, error = %error, "Error fetching system health info");
                    info.insert(
                        "error".to_owned(),
                        SystemHealthInitialValue::Json(json!({
                            "type": "failed",
                            "error": "unknown",
                        })),
                    );
                }
                Err(_) => {
                    info.insert(
                        "error".to_owned(),
                        SystemHealthInitialValue::Json(json!({
                            "type": "failed",
                            "error": "timeout",
                        })),
                    );
                }
            }

            initial.insert(domain, SystemHealthDomainData { info, manage_url });
        }

        Ok(SystemHealthSession { initial, updates })
    }

    /// Return the source-style internal report, resolving every deferred field.
    ///
    /// Unlike the WebSocket stream, this helper includes deferred provider
    /// error details as `Exception: ...` strings, matching the separate source
    /// helper. Do not use this method to form authenticated WebSocket events.
    pub async fn get_info(&self) -> Result<SystemHealthInfoMap> {
        let mut session = self.collect().await?;
        let mut domains = BTreeMap::new();
        for (domain, data) in std::mem::take(&mut session.initial) {
            let info: BTreeMap<String, Value> = data
                .info
                .into_iter()
                .filter_map(|(key, value)| value.to_get_info_value().map(|value| (key, value)))
                .collect();
            domains.insert(domain, info);
        }

        while let Some(update) = session.next_update().await {
            let value = match update.result {
                SystemHealthUpdateResult::Success(value) => value.to_get_info_value(),
                SystemHealthUpdateResult::Failed => Value::String(format!(
                    "Exception: {}",
                    update.error_detail.as_deref().unwrap_or("unknown")
                )),
            };
            if let Some(info) = domains.get_mut(&update.domain) {
                info.insert(update.key, value);
            }
        }
        Ok(domains)
    }
}

// Provider failures must not terminate the transport task without a response.
// Keep the actual panic detail for logs/internal get_info, while the session's
// public encoding uses the same generic error as other provider failures.
async fn catch_provider_panic<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .unwrap_or_else(|payload| {
            let detail = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic payload");
            Err(Error::Service(format!(
                "System Health provider panicked: {detail}"
            )))
        })
}

/// Test whether an HTTP URL can be reached using the source five-second GET.
///
/// Any HTTP status counts as reachable; response bodies and error details are
/// not exposed. `more_info` is copied only into a failed result.
pub async fn check_can_reach_url(url: &str, more_info: Option<&str>) -> Value {
    let request = reqwest::Client::new()
        .get(url)
        .timeout(REACHABILITY_TIMEOUT)
        .send();
    let failure = match tokio::time::timeout(REACHABILITY_TIMEOUT, request).await {
        Ok(Ok(_response)) => return Value::String("ok".to_owned()),
        Ok(Err(error)) if error.is_timeout() => "timeout",
        Err(_) => "timeout",
        Ok(Err(_)) => "unreachable",
    };
    let mut result = Map::new();
    result.insert("type".to_owned(), Value::String("failed".to_owned()));
    result.insert("error".to_owned(), Value::String(failure.to_owned()));
    if let Some(more_info) = more_info {
        result.insert("more_info".to_owned(), Value::String(more_info.to_owned()));
    }
    Value::Object(result)
}

fn isoformat(value: &DateTime<Utc>) -> String {
    let mut formatted = value.format("%Y-%m-%dT%H:%M:%S").to_string();
    let micros = value.timestamp_subsec_micros();
    if micros != 0 {
        formatted.push_str(&format!(".{micros:06}"));
    }
    formatted.push_str("+00:00");
    formatted
}

fn get_info_value(value: Value) -> Value {
    let Some(object) = value.as_object() else {
        return value;
    };
    if object.get("type") != Some(&Value::String("failed".to_owned())) {
        return value;
    }
    let error = object
        .get("error")
        .map(python_string)
        .unwrap_or_else(|| "unknown".to_owned());
    Value::String(format!("Failed: {error}"))
}

fn python_string(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(value) => {
            if *value {
                "True".to_owned()
            } else {
                "False".to_owned()
            }
        }
        Value::Number(value) => value.to_string(),
        Value::String(value) => value.clone(),
        // Compound error values are unusual; JSON compact form keeps their
        // content readable without leaking a Rust Debug representation.
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        future::Future,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use chrono::{TimeZone, Utc};
    use hass_core::Error;
    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Barrier,
    };

    use super::{
        HealthFuture, SystemHealthCallback, SystemHealthData, SystemHealthInfo,
        SystemHealthInitialValue, SystemHealthIntegration, SystemHealthUpdateResult,
        SystemHealthValue, check_can_reach_url,
    };

    fn callback<F, Fut>(factory: F) -> SystemHealthCallback
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = hass_core::Result<SystemHealthInfo>> + Send + 'static,
    {
        Arc::new(move || Box::pin(factory()) as HealthFuture<SystemHealthInfo>)
    }

    fn json_info(key: &str, value: Value) -> SystemHealthInfo {
        BTreeMap::from([(
            key.to_owned(),
            SystemHealthValue::Immediate(SystemHealthData::Json(value)),
        )])
    }

    #[tokio::test]
    async fn same_domain_registration_replaces_callback_and_manage_url() {
        let integration = SystemHealthIntegration::new();
        let manager = integration.manager();
        let calls = Arc::new(AtomicUsize::new(0));
        let prior_calls = calls.clone();
        integration
            .register_info(
                "example",
                callback(move || {
                    let calls = prior_calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(json_info("value", json!("old")))
                    }
                }),
                Some("https://old.invalid".to_owned()),
            )
            .unwrap();
        integration
            .register_info(
                "example",
                callback(|| async { Ok(json_info("value", json!("new"))) }),
                Some("https://new.invalid".to_owned()),
            )
            .unwrap();

        let session = manager.collect().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let encoded = serde_json::to_value(session.initial).unwrap();
        assert_eq!(encoded["example"]["info"]["value"], "new");
        assert_eq!(encoded["example"]["manage_url"], "https://new.invalid");
    }

    #[tokio::test]
    async fn callback_collection_is_concurrent_and_domains_are_sorted() {
        let integration = SystemHealthIntegration::new();
        let barrier = Arc::new(Barrier::new(2));
        for domain in ["zeta", "alpha"] {
            let barrier = barrier.clone();
            integration
                .register_info(
                    domain,
                    callback(move || {
                        let barrier = barrier.clone();
                        async move {
                            barrier.wait().await;
                            Ok(json_info("ready", json!(true)))
                        }
                    }),
                    None,
                )
                .unwrap();
        }

        let session = integration.manager().collect().await.unwrap();
        assert_eq!(
            session
                .initial
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
    }

    #[tokio::test]
    async fn callback_error_and_timeout_are_generic_in_initial_data() {
        let integration = SystemHealthIntegration::new();
        integration
            .register_info(
                "error",
                callback(|| async { Err(Error::Service("private token path".to_owned())) }),
                None,
            )
            .unwrap();
        integration
            .register_info(
                "timeout",
                callback(|| async {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    Ok(SystemHealthInfo::new())
                }),
                None,
            )
            .unwrap();

        let session = integration.manager().collect().await.unwrap();
        let encoded = serde_json::to_value(session.initial).unwrap();
        assert_eq!(
            encoded["error"]["info"]["error"],
            json!({"type":"failed", "error":"unknown"})
        );
        assert_eq!(
            encoded["timeout"]["info"]["error"],
            json!({"type":"failed", "error":"timeout"})
        );
        assert!(!encoded.to_string().contains("private token path"));
    }

    #[tokio::test]
    async fn provider_panics_complete_with_generic_transport_failures() {
        let integration = SystemHealthIntegration::new();
        integration
            .register_info(
                "callback_panic",
                Arc::new(|| panic!("private callback panic")),
                None,
            )
            .unwrap();
        integration
            .register_info(
                "deferred_panic",
                callback(|| async {
                    Ok(BTreeMap::from([(
                        "value".to_owned(),
                        SystemHealthValue::Deferred(Box::pin(async {
                            panic!("private deferred panic")
                        })),
                    )]))
                }),
                None,
            )
            .unwrap();
        let mut session = integration.manager().collect().await.unwrap();
        let initial = serde_json::to_value(&session.initial).unwrap();
        assert_eq!(
            initial["callback_panic"]["info"]["error"],
            json!({"type":"failed", "error":"unknown"})
        );
        assert!(!initial.to_string().contains("private"));
        let update = session.next_update().await.unwrap().to_transport_event();
        assert_eq!(update["success"], false);
        assert!(!update.to_string().contains("private"));
        assert!(session.next_update().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn callback_timeout_uses_the_pinned_five_second_deadline() {
        let integration = SystemHealthIntegration::new();
        integration
            .register_info(
                "timeout",
                callback(|| async {
                    tokio::time::sleep(Duration::from_secs(6)).await;
                    Ok(SystemHealthInfo::new())
                }),
                None,
            )
            .unwrap();

        let session = integration.manager().collect().await.unwrap();
        assert_eq!(
            session.initial["timeout"].info["error"],
            SystemHealthInitialValue::Json(json!({"type":"failed", "error":"timeout"}))
        );
    }

    #[tokio::test]
    async fn deferred_dates_encode_as_dates_and_internal_helper_stringifies() {
        let integration = SystemHealthIntegration::new();
        let date = Utc.with_ymd_and_hms(2026, 11, 6, 9, 10, 11).unwrap();
        let callback_date = date;
        integration
            .register_info(
                "dates",
                callback(move || {
                    let date = callback_date;
                    async move {
                        Ok(BTreeMap::from([
                            (
                                "immediate".to_owned(),
                                SystemHealthValue::Immediate(SystemHealthData::DateTime(date)),
                            ),
                            (
                                "deferred".to_owned(),
                                SystemHealthValue::Deferred(Box::pin(async move {
                                    Ok(SystemHealthData::DateTime(date))
                                })
                                    as HealthFuture<SystemHealthData>),
                            ),
                        ]))
                    }
                }),
                None,
            )
            .unwrap();

        let mut session = integration.manager().collect().await.unwrap();
        let initial = serde_json::to_value(&session.initial).unwrap();
        assert_eq!(
            initial["dates"]["info"]["immediate"],
            json!({"value":"2026-11-06T09:10:11+00:00", "type":"date"})
        );
        assert_eq!(
            initial["dates"]["info"]["deferred"],
            json!({"type":"pending"})
        );
        let update = session.next_update().await.unwrap();
        assert_eq!(update.domain, "dates");
        assert_eq!(update.key, "deferred");
        assert_eq!(
            update.result,
            SystemHealthUpdateResult::Success(SystemHealthData::DateTime(date))
        );
        assert_eq!(
            update.to_transport_event(),
            json!({
                "type":"update",
                "domain":"dates",
                "key":"deferred",
                "success":true,
                "data":{"value":"2026-11-06T09:10:11+00:00", "type":"date"}
            })
        );
        assert!(session.next_update().await.is_none());

        let helper = integration.manager().get_info().await.unwrap();
        assert_eq!(helper["dates"]["immediate"], "2026-11-06T09:10:11+00:00");
        assert_eq!(helper["dates"]["deferred"], "2026-11-06T09:10:11+00:00");
    }

    #[tokio::test]
    async fn deferred_errors_are_generic_for_transport_but_detailed_in_get_info() {
        let integration = SystemHealthIntegration::new();
        integration
            .register_info(
                "errors",
                callback(|| async {
                    Ok(BTreeMap::from([
                        (
                            "later".to_owned(),
                            SystemHealthValue::Deferred(Box::pin(async {
                                Err(Error::Service("sentinel secret".to_owned()))
                            })
                                as HealthFuture<SystemHealthData>),
                        ),
                        (
                            "failed_value".to_owned(),
                            SystemHealthValue::Immediate(SystemHealthData::Json(json!({
                                "type":"failed", "error":"offline"
                            }))),
                        ),
                        (
                            "failed_default".to_owned(),
                            SystemHealthValue::Immediate(SystemHealthData::Json(json!({
                                "type":"failed"
                            }))),
                        ),
                    ]))
                }),
                None,
            )
            .unwrap();

        let mut session = integration.manager().collect().await.unwrap();
        let update = session.next_update().await.unwrap();
        assert_eq!(update.result, SystemHealthUpdateResult::Failed);
        assert_eq!(
            update.to_transport_event(),
            json!({
                "type":"update",
                "domain":"errors",
                "key":"later",
                "success":false,
                "error":{"type":"failed", "error":"unknown"}
            })
        );
        assert!(
            !update
                .to_transport_event()
                .to_string()
                .contains("sentinel secret")
        );
        assert!(!format!("{update:?}").contains("sentinel secret"));
        assert!(session.next_update().await.is_none());

        let info = integration.manager().get_info().await.unwrap();
        assert_eq!(
            info["errors"]["later"],
            "Exception: service error: sentinel secret"
        );
        assert_eq!(info["errors"]["failed_value"], "Failed: offline");
        assert_eq!(info["errors"]["failed_default"], "Failed: unknown");
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn dropping_session_drops_unfinished_deferred_futures() {
        let integration = SystemHealthIntegration::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let callback_dropped = dropped.clone();
        integration
            .register_info(
                "deferred",
                callback(move || {
                    let dropped = callback_dropped.clone();
                    async move {
                        let guard = DropFlag(dropped);
                        let future: HealthFuture<SystemHealthData> = Box::pin(async move {
                            let _guard = guard;
                            std::future::pending().await
                        });
                        Ok(BTreeMap::from([(
                            "blocked".to_owned(),
                            SystemHealthValue::Deferred(future),
                        )]))
                    }
                }),
                None,
            )
            .unwrap();

        let mut session = integration.manager().collect().await.unwrap();
        assert_eq!(
            session.initial["deferred"].info["blocked"],
            SystemHealthInitialValue::Pending
        );
        {
            let next = session.next_update();
            tokio::pin!(next);
            tokio::select! {
                biased;
                _ = &mut next => panic!("blocked deferred value completed"),
                _ = tokio::task::yield_now() => {},
            }
        }
        drop(session);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn transferred_tasks_complete_or_abort_with_the_session() {
        let integration = SystemHealthIntegration::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let callback_dropped = dropped.clone();
        integration
            .register_info(
                "tasks",
                callback(move || {
                    let guard = DropFlag(callback_dropped.clone());
                    async move {
                        let pending = tokio::spawn(async move {
                            let _guard = guard;
                            std::future::pending::<hass_core::Result<SystemHealthData>>().await
                        });
                        let complete =
                            tokio::spawn(async { Ok(SystemHealthData::Json(json!("complete"))) });
                        Ok(BTreeMap::from([
                            ("pending".into(), SystemHealthValue::from_task(pending)),
                            ("complete".into(), SystemHealthValue::from_task(complete)),
                        ]))
                    }
                }),
                None,
            )
            .unwrap();
        let mut session = integration.manager().collect().await.unwrap();
        let update = session.next_update().await.unwrap().to_transport_event();
        assert_eq!(update["key"], "complete");
        assert_eq!(update["data"], "complete");
        drop(session);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("transferred task must be aborted when its report is dropped");
    }

    #[tokio::test]
    async fn owner_drop_invalidates_weak_handle() {
        let integration = SystemHealthIntegration::new();
        let manager = integration.manager();
        drop(integration);
        assert!(manager.collect().await.is_err());
        assert!(
            manager
                .register_info(
                    "lost",
                    callback(|| async { Ok(SystemHealthInfo::new()) }),
                    None
                )
                .is_err()
        );
    }

    async fn serve_once(status: u16, delay: Duration) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await;
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let response =
                format!("HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(response.as_bytes()).await;
        });
        (format!("http://{address}/"), server)
    }

    #[tokio::test]
    async fn reachability_helper_uses_loopback_only_and_ignores_status() {
        for status in [200, 500] {
            let (url, server) = serve_once(status, Duration::ZERO).await;
            assert_eq!(check_can_reach_url(&url, None).await, json!("ok"));
            server.await.unwrap();
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        assert_eq!(
            check_can_reach_url(&format!("http://{address}/"), Some("details")).await,
            json!({"type":"failed", "error":"unreachable", "more_info":"details"})
        );
    }

    #[tokio::test]
    async fn reachability_timeout_uses_loopback_delayed_response() {
        let (url, server) = serve_once(200, Duration::from_secs(7)).await;
        assert_eq!(
            check_can_reach_url(&url, Some("slow local server")).await,
            json!({"type":"failed", "error":"timeout", "more_info":"slow local server"})
        );
        server.abort();
    }
}
