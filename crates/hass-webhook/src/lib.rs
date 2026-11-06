//! Process-local webhook registration and callback dispatch.
//!
//! This crate intentionally does not depend on `hass-core`. HTTP adapters can
//! translate requests into [`WebhookRequest`] and automation adapters can use
//! callbacks to enqueue private trigger work without publishing it on the
//! runtime event bus.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    future::Future,
    net::IpAddr,
    pin::Pin,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::Serialize;
use serde_json::Value;

/// Methods accepted by Home Assistant's webhook route.
pub const SUPPORTED_METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT"];

/// Methods accepted by a directly registered webhook when none are specified.
pub const DEFAULT_METHODS: &[&str] = &["POST", "PUT"];

/// Typed callback result. `Ok(None)` means the callback accepted the request
/// without a custom response body.
pub type WebhookCallbackResult = Result<Option<WebhookResponse>, WebhookHandlerError>;

type HandlerFuture = Pin<Box<dyn Future<Output = WebhookCallbackResult> + Send + 'static>>;
type Handler = dyn Fn(WebhookRequest) -> HandlerFuture + Send + Sync + 'static;

#[derive(Default)]
struct Registry {
    handlers: RwLock<HashMap<String, Entry>>,
    next_registration: AtomicU64,
}

struct Entry {
    registration_id: u64,
    order: u64,
    domain: String,
    name: String,
    local_only: bool,
    allowed_methods: Vec<String>,
    handler: Arc<Handler>,
}

/// A cloneable handle to the in-memory webhook callback registry.
#[derive(Clone, Default)]
pub struct WebhookManager {
    registry: Arc<Registry>,
}

/// Options shown in the admin-only webhook inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookConfig {
    /// Integration or owner domain shown in the admin inventory.
    pub domain: String,
    /// Human-readable callback name shown in the admin inventory.
    pub name: String,
    /// Reject requests whose trusted client origin is not local.
    pub local_only: bool,
    /// Explicit methods, or `None` to use `POST` and `PUT`.
    pub allowed_methods: Option<Vec<String>>,
}

impl WebhookConfig {
    /// Create direct-registration options with Home Assistant's method defaults.
    pub fn new(domain: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            name: name.into(),
            local_only: false,
            allowed_methods: None,
        }
    }
}

/// A callback record intended only for admin-authorized inventory commands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WebhookInfo {
    /// The callback ID is a capability and must not be shown to public clients.
    pub webhook_id: String,
    /// Integration or owner domain.
    pub domain: String,
    /// Human-readable callback name.
    pub name: String,
    /// Whether only local client origins may invoke this callback.
    pub local_only: bool,
    /// Supported methods, sorted for stable inventory output.
    pub allowed_methods: Vec<String>,
}

/// Resolved client origin supplied by the HTTP or WebSocket adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WebhookOrigin {
    /// Resolved client address, when available.
    pub client_ip: Option<IpAddr>,
    /// Whether the adapter's trusted network policy classified the client as local.
    pub is_local: bool,
}

/// Normalized inbound request metadata passed only to the registered callback.
///
/// This type intentionally does not implement `Debug` or `Serialize`: requests
/// contain the bearer webhook ID and payload and must stay out of general logs
/// and public inventories.
pub struct WebhookRequest {
    /// HTTP method as received.
    pub method: String,
    /// Decoded query keys. For duplicate keys, the first value is retained,
    /// matching indexed access on Home Assistant's multidict query mapping.
    pub query: BTreeMap<String, String>,
    /// Lowercase header names and their first textual values.
    pub headers: BTreeMap<String, String>,
    /// Raw request body, bounded by the transport adapter.
    pub body: Vec<u8>,
    /// Parsed JSON when the content type selects JSON parsing. An empty JSON
    /// request is represented as an empty object.
    pub json: Option<Value>,
    /// Parsed form fields for non-JSON form requests.
    pub form: BTreeMap<String, String>,
    /// Resolved transport origin.
    pub origin: WebhookOrigin,
    webhook_id: String,
}

impl WebhookRequest {
    /// Construct a request for dispatch; the manager adds its registered ID.
    pub fn new(method: impl Into<String>, origin: WebhookOrigin) -> Self {
        Self {
            method: method.into(),
            query: BTreeMap::new(),
            headers: BTreeMap::new(),
            body: Vec::new(),
            json: None,
            form: BTreeMap::new(),
            origin,
            webhook_id: String::new(),
        }
    }

    /// Return the registered webhook ID for callback code.
    pub fn webhook_id(&self) -> &str {
        &self.webhook_id
    }
}

/// Response returned by a webhook callback.
///
/// A callback that only schedules work should return `Ok(None)`, which maps to
/// an empty `200 OK` response.
pub struct WebhookResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers, validated by the API adapter.
    pub headers: BTreeMap<String, String>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

impl WebhookResponse {
    /// Create a response with the supplied status and body.
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: BTreeMap::new(),
            body: body.into(),
        }
    }

    /// Create an empty `200 OK` response.
    pub fn ok() -> Self {
        Self::new(200, Vec::new())
    }
}

/// A callback failed without exposing its error text to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebhookHandlerError;

/// Registration validation error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebhookRegistrationError {
    /// The ID already has a registered callback.
    DuplicateId,
    /// The requested HTTP method is not supported by the webhook route.
    UnsupportedMethod(String),
}

impl fmt::Display for WebhookRegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId => f.write_str("webhook callback is already registered"),
            Self::UnsupportedMethod(method) => {
                write!(f, "unsupported webhook method `{method}`")
            }
        }
    }
}

impl std::error::Error for WebhookRegistrationError {}

/// A callback registration whose lifetime owns its registry entry.
///
/// The guard retains only a weak reference to the manager's registry. Dropping
/// the final manager releases all callbacks; dropping this guard removes only
/// the exact registration that created it.
pub struct WebhookRegistration {
    registry: Weak<Registry>,
    webhook_id: String,
    registration_id: u64,
}

impl Drop for WebhookRegistration {
    fn drop(&mut self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let mut handlers = registry
            .handlers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if handlers
            .get(&self.webhook_id)
            .is_some_and(|entry| entry.registration_id == self.registration_id)
        {
            handlers.remove(&self.webhook_id);
        }
    }
}

impl WebhookManager {
    /// Create an empty process-local webhook registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Generate a 256-bit lowercase hexadecimal webhook ID.
    pub fn generate_id() -> Result<String, getrandom::Error> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes)?;
        let mut id = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            use fmt::Write as _;
            write!(id, "{byte:02x}").expect("writing to a string cannot fail");
        }
        Ok(id)
    }

    /// Register an asynchronous callback and return the guard that owns it.
    pub fn register<F, Fut>(
        &self,
        webhook_id: impl Into<String>,
        config: WebhookConfig,
        handler: F,
    ) -> Result<WebhookRegistration, WebhookRegistrationError>
    where
        F: Fn(WebhookRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = WebhookCallbackResult> + Send + 'static,
    {
        let webhook_id = webhook_id.into();
        let mut allowed_methods = config.allowed_methods.unwrap_or_else(|| {
            DEFAULT_METHODS
                .iter()
                .map(|method| (*method).to_owned())
                .collect()
        });
        if let Some(method) = allowed_methods
            .iter()
            .find(|method| !SUPPORTED_METHODS.contains(&method.as_str()))
        {
            return Err(WebhookRegistrationError::UnsupportedMethod(method.clone()));
        }
        allowed_methods.sort();
        allowed_methods.dedup();

        let handler: Arc<Handler> = Arc::new(move |request| Box::pin(handler(request)));
        let mut handlers = self
            .registry
            .handlers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if handlers.contains_key(&webhook_id) {
            return Err(WebhookRegistrationError::DuplicateId);
        }
        let registration_id = self
            .registry
            .next_registration
            .fetch_add(1, Ordering::Relaxed);
        let order = registration_id;
        handlers.insert(
            webhook_id.clone(),
            Entry {
                registration_id,
                order,
                domain: config.domain,
                name: config.name,
                local_only: config.local_only,
                allowed_methods,
                handler,
            },
        );
        Ok(WebhookRegistration {
            registry: Arc::downgrade(&self.registry),
            webhook_id,
            registration_id,
        })
    }

    /// Dispatch a normalized request through the process-local registry.
    pub async fn dispatch(&self, webhook_id: &str, mut request: WebhookRequest) -> WebhookResponse {
        request.webhook_id = webhook_id.to_owned();
        if !SUPPORTED_METHODS.contains(&request.method.as_str()) {
            return WebhookResponse::new(405, Vec::new());
        }
        let entry = {
            let handlers = self
                .registry
                .handlers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            handlers.get(webhook_id).map(|entry| {
                (
                    entry.local_only,
                    entry.allowed_methods.clone(),
                    Arc::clone(&entry.handler),
                )
            })
        };
        let Some((local_only, allowed_methods, handler)) = entry else {
            return WebhookResponse::ok();
        };
        if !allowed_methods
            .iter()
            .any(|method| method == &request.method)
        {
            return if request.method == "HEAD" {
                WebhookResponse::ok()
            } else {
                WebhookResponse::new(405, Vec::new())
            };
        }
        if local_only && !request.origin.is_local {
            return WebhookResponse::ok();
        }
        match handler(request).await {
            Ok(Some(response)) => response,
            Ok(None) | Err(_) => WebhookResponse::ok(),
        }
    }

    /// Return registration metadata for an admin-authorized inventory command.
    ///
    /// The result includes webhook IDs, so callers must not expose it to
    /// unauthenticated or non-admin clients.
    pub fn admin_inventory(&self) -> Vec<WebhookInfo> {
        let handlers = self
            .registry
            .handlers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut entries = handlers
            .iter()
            .map(|(webhook_id, entry)| {
                (
                    entry.order,
                    WebhookInfo {
                        webhook_id: webhook_id.clone(),
                        domain: entry.domain.clone(),
                        name: entry.name.clone(),
                        local_only: entry.local_only,
                        allowed_methods: entry.allowed_methods.clone(),
                    },
                )
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|(order, _)| *order);
        entries.into_iter().map(|(_, entry)| entry).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        WebhookConfig, WebhookHandlerError, WebhookManager, WebhookOrigin,
        WebhookRegistrationError, WebhookRequest, WebhookResponse,
    };
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::sync::{mpsc, watch};

    fn local_request(method: &str) -> WebhookRequest {
        WebhookRequest::new(
            method,
            WebhookOrigin {
                client_ip: Some("127.0.0.1".parse().unwrap()),
                is_local: true,
            },
        )
    }

    #[tokio::test]
    async fn registration_guard_unregisters_callback_and_unknown_ids_stay_silent() {
        let manager = WebhookManager::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let registration = manager
            .register(
                "private-id",
                WebhookConfig::new("automation", "hook"),
                move |_| {
                    let callback_calls = Arc::clone(&callback_calls);
                    async move {
                        callback_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(None)
                    }
                },
            )
            .unwrap();

        assert_eq!(manager.admin_inventory().len(), 1);
        let response = manager.dispatch("private-id", local_request("POST")).await;
        assert_eq!(response.status, 200);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        drop(registration);
        assert!(manager.admin_inventory().is_empty());
        let response = manager.dispatch("private-id", local_request("POST")).await;
        assert_eq!(response.status, 200);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn locality_methods_and_callback_failures_follow_source_responses() {
        let manager = WebhookManager::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let mut config = WebhookConfig::new("automation", "local hook");
        config.local_only = true;
        let _registration = manager
            .register("local", config, move |_| {
                let callback_calls = Arc::clone(&callback_calls);
                async move {
                    callback_calls.fetch_add(1, Ordering::SeqCst);
                    Err(WebhookHandlerError)
                }
            })
            .unwrap();

        let mut remote = WebhookRequest::new(
            "POST",
            WebhookOrigin {
                client_ip: Some("203.0.113.10".parse().unwrap()),
                is_local: false,
            },
        );
        remote.body = b"private payload".to_vec();
        let response = manager.dispatch("local", remote).await;
        assert_eq!(response.status, 200);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        assert_eq!(
            manager.dispatch("local", local_request("GET")).await.status,
            405
        );
        assert_eq!(
            manager
                .dispatch("local", local_request("HEAD"))
                .await
                .status,
            200
        );
        assert_eq!(
            manager
                .dispatch("local", local_request("POST"))
                .await
                .status,
            200
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn explicit_methods_and_inventory_do_not_capture_request_payloads() {
        let manager = WebhookManager::new();
        let mut config = WebhookConfig::new("test", "inventory hook");
        config.allowed_methods = Some(vec!["PUT".into(), "GET".into()]);
        let _registration = manager
            .register("id-hidden-from-public-lists", config, |_| async {
                Ok(Some(WebhookResponse::new(202, b"accepted".to_vec())))
            })
            .unwrap();

        let mut request = local_request("PUT");
        request.body = b"payload-secret".to_vec();
        request
            .headers
            .insert("x-private-token".into(), "header-secret".into());
        request.json = Some(json!({"secret": "json-secret"}));
        request.form.insert("secret".into(), "form-secret".into());
        let response = manager
            .dispatch("id-hidden-from-public-lists", request)
            .await;
        assert_eq!(response.status, 202);
        assert_eq!(response.body, b"accepted");

        let inventory = serde_json::to_string(&manager.admin_inventory()).unwrap();
        assert!(inventory.contains("id-hidden-from-public-lists"));
        assert!(inventory.contains("inventory hook"));
        assert!(!inventory.contains("payload-secret"));
        assert!(!inventory.contains("header-secret"));
        assert!(!inventory.contains("json-secret"));
        assert!(!inventory.contains("form-secret"));
    }

    #[test]
    fn duplicate_and_unsupported_registration_are_rejected_without_echoing_ids() {
        let manager = WebhookManager::new();
        let _registration = manager
            .register("secret-id", WebhookConfig::new("test", "hook"), |_| async {
                Ok(None)
            })
            .unwrap();
        assert!(matches!(
            manager.register("secret-id", WebhookConfig::new("test", "hook"), |_| async {
                Ok(None)
            }),
            Err(WebhookRegistrationError::DuplicateId)
        ));

        let mut config = WebhookConfig::new("test", "hook");
        config.allowed_methods = Some(vec!["PATCH".into()]);
        let error = match manager.register("another-secret", config, |_| async { Ok(None) }) {
            Ok(_) => panic!("unsupported methods must be rejected"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            WebhookRegistrationError::UnsupportedMethod("PATCH".into())
        );
        assert!(!error.to_string().contains("another-secret"));
    }

    #[test]
    fn generated_ids_match_the_upstream_hex_shape() {
        let id = WebhookManager::generate_id().unwrap();
        assert_eq!(id.len(), 64);
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }

    #[test]
    fn registration_guard_weakly_tracks_manager_lifetime() {
        struct DropMarker(Arc<AtomicBool>);

        impl Drop for DropMarker {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let manager = WebhookManager::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let marker = DropMarker(Arc::clone(&dropped));
        let registration = manager
            .register(
                "manager-lifetime",
                WebhookConfig::new("test", "guard"),
                move |_| {
                    let _ = &marker;
                    async { Ok(None) }
                },
            )
            .unwrap();

        drop(manager);
        assert!(dropped.load(Ordering::SeqCst));
        drop(registration);
    }

    #[tokio::test]
    async fn dropping_registration_suppresses_new_calls_but_finishes_inflight_call() {
        let manager = WebhookManager::new();
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (release_tx, release_rx) = watch::channel(false);
        let registration = manager
            .register(
                "inflight",
                WebhookConfig::new("test", "inflight"),
                move |_| {
                    let started_tx = started_tx.clone();
                    let mut release_rx = release_rx.clone();
                    async move {
                        let _ = started_tx.send(());
                        let _ = release_rx.wait_for(|released| *released).await;
                        Ok(None)
                    }
                },
            )
            .unwrap();
        let dispatch_manager = manager.clone();
        let inflight = tokio::spawn(async move {
            dispatch_manager
                .dispatch("inflight", local_request("POST"))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), started_rx.recv())
            .await
            .expect("callback should start before its registration is dropped");

        drop(registration);
        assert!(manager.admin_inventory().is_empty());
        assert_eq!(
            manager
                .dispatch("inflight", local_request("POST"))
                .await
                .status,
            200
        );
        assert!(started_rx.try_recv().is_err());
        release_tx.send(true).unwrap();
        assert_eq!(inflight.await.unwrap().status, 200);
    }
}
