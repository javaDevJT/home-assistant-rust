//! WebSocket adapter for the application-credentials collection.
//!
//! The collection DTOs intentionally contain client secrets, so callers must
//! enforce the upstream administrator boundary before invoking any `*_admin`
//! operation or opening an admin subscription. The `config` command is the
//! public, non-secret provider catalog; `config_entry` is administrator-only.

use std::sync::{Arc, Mutex, MutexGuard};

use hass_application_credentials::{
    ApplicationCredentialsError, ApplicationCredentialsManager, CreateCredential,
    CredentialChangeType, CredentialRecord, CredentialSubscription, SubscriptionError,
};
use serde_json::{Map, Value, json};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

const STREAM_CHANNEL_CAPACITY: usize = 64;

/// A safe protocol error projection. It never contains a credential payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ApiError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl ApiError {
    fn invalid_format(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_format",
            message: message.into(),
        }
    }
}

impl From<ApplicationCredentialsError> for ApiError {
    fn from(error: ApplicationCredentialsError) -> Self {
        let code = match &error {
            ApplicationCredentialsError::NoPlatform(_)
            | ApplicationCredentialsError::InvalidPlatform(_)
            | ApplicationCredentialsError::UpdatesNotSupported => "invalid_format",
            ApplicationCredentialsError::ItemNotFound(_) => "not_found",
            ApplicationCredentialsError::CredentialInUse { .. } => "home_assistant_error",
            ApplicationCredentialsError::ConfigEntryNotFound(_) => "invalid_config_entry_id",
            ApplicationCredentialsError::IntegrationStopped
            | ApplicationCredentialsError::StorageUnavailable
            | ApplicationCredentialsError::MalformedStore
            | ApplicationCredentialsError::UnsupportedStoreVersion(_)
            | ApplicationCredentialsError::DuplicatePlatform(_)
            | ApplicationCredentialsError::PlatformCallback { .. } => "home_assistant_error",
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

/// Validate the registered command schema before authorization, as the source
/// WebSocket dispatcher does. This performs no provider or collection lookup.
pub(crate) fn validate_schema(
    kind: &str,
    command: &Map<String, Value>,
) -> std::result::Result<(), ApiError> {
    match kind {
        "application_credentials/list"
        | "application_credentials/config"
        | "application_credentials/subscribe" => ensure_fields(command, &["id", "type"]),
        "application_credentials/create" => {
            ensure_fields(
                command,
                &[
                    "id",
                    "type",
                    "domain",
                    "client_id",
                    "client_secret",
                    "auth_domain",
                    "name",
                ],
            )?;
            for field in ["domain", "client_id", "client_secret"] {
                required_string(command, field)?;
            }
            for field in ["auth_domain", "name"] {
                if command.get(field).is_some_and(|value| !value.is_string()) {
                    return Err(ApiError::invalid_format(format!(
                        "{field} must be a string"
                    )));
                }
            }
            Ok(())
        }
        "application_credentials/update" | "application_credentials/delete" => {
            ensure_fields(command, &["id", "type", "application_credentials_id"])?;
            required_string(command, "application_credentials_id")?;
            Ok(())
        }
        "application_credentials/config_entry" => {
            ensure_fields(command, &["id", "type", "config_entry_id"])?;
            required_string(command, "config_entry_id")?;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// List stored records. The caller must authorize an administrator first;
/// `CredentialRecord` serialization includes `client_secret` by design.
pub(crate) fn list_admin(
    manager: &ApplicationCredentialsManager,
    command: &Map<String, Value>,
) -> std::result::Result<Value, ApiError> {
    ensure_fields(command, &["id", "type"])?;
    Ok(json!(manager.list_admin()?))
}

/// Create a credential with the pinned schema's required and optional fields.
/// Only `client_id` and `client_secret` are trimmed; `auth_domain` and `name`
/// retain their supplied strings, matching the source validators.
pub(crate) fn create_admin(
    manager: &ApplicationCredentialsManager,
    command: &Map<String, Value>,
) -> std::result::Result<Value, ApiError> {
    ensure_fields(
        command,
        &[
            "id",
            "type",
            "domain",
            "client_id",
            "client_secret",
            "auth_domain",
            "name",
        ],
    )?;
    let input = CreateCredential {
        domain: required_string(command, "domain")?.to_owned(),
        client_id: required_string(command, "client_id")?.to_owned(),
        client_secret: required_string(command, "client_secret")?.to_owned(),
        auth_domain: optional_string(command, "auth_domain")?,
        name: optional_string(command, "name")?,
    };
    Ok(json!(manager.create_admin(input)?))
}

/// Reject edits after validating that the requested item exists.
pub(crate) fn update_admin(
    manager: &ApplicationCredentialsManager,
    command: &Map<String, Value>,
) -> std::result::Result<Value, ApiError> {
    ensure_fields(command, &["id", "type", "application_credentials_id"])?;
    let item_id = required_string(command, "application_credentials_id")?;
    Ok(json!(manager.update_admin(item_id)?))
}

/// Delete a stored record after the SDK's config-entry in-use guard succeeds.
pub(crate) fn delete_admin(
    manager: &ApplicationCredentialsManager,
    command: &Map<String, Value>,
) -> std::result::Result<Value, ApiError> {
    ensure_fields(command, &["id", "type", "application_credentials_id"])?;
    let item_id = required_string(command, "application_credentials_id")?;
    manager.delete_admin(item_id)?;
    Ok(Value::Null)
}

/// Return supported loaded providers and their optional placeholders. The SDK
/// projection contains no client credentials or secret values.
pub(crate) async fn config(
    manager: &ApplicationCredentialsManager,
    command: &Map<String, Value>,
) -> std::result::Result<Value, ApiError> {
    ensure_fields(command, &["id", "type"])?;
    Ok(json!(manager.config().await?))
}

/// Resolve the selected stable credential ID without returning its secret.
pub(crate) fn config_entry(
    manager: &ApplicationCredentialsManager,
    command: &Map<String, Value>,
) -> std::result::Result<Value, ApiError> {
    ensure_fields(command, &["id", "type", "config_entry_id"])?;
    let entry_id = required_string(command, "config_entry_id")?;
    Ok(match manager.credential_id_for_config_entry(entry_id)? {
        Some(credential_id) => json!({ "application_credentials_id": credential_id }),
        None => json!({}),
    })
}

/// Prepare an admin collection subscription before sending its result ACK.
/// The caller sends [`CredentialStream::initial_snapshot`] after the ACK, then
/// calls [`CredentialStream::start`] and retains the stream until unsubscribe.
pub(crate) fn subscribe_admin(
    manager: &ApplicationCredentialsManager,
    id: u64,
    command: &Map<String, Value>,
) -> std::result::Result<CredentialStream, ApiError> {
    ensure_fields(command, &["id", "type"])?;
    let subscription = manager.subscribe_admin()?;
    let (sender, receiver) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
    let (cancel, cancel_receiver) = oneshot::channel();
    Ok(CredentialStream {
        id,
        subscription: Some(subscription),
        receiver,
        task: None,
        cancel: Some(cancel),
        cancel_receiver: Some(cancel_receiver),
        error: Arc::new(Mutex::new(None)),
        sender: Some(sender),
    })
}

/// One admin-only storage-collection subscription and bounded event pump.
pub(crate) struct CredentialStream {
    id: u64,
    subscription: Option<CredentialSubscription>,
    receiver: mpsc::Receiver<Value>,
    sender: Option<mpsc::Sender<Value>>,
    task: Option<JoinHandle<()>>,
    cancel: Option<oneshot::Sender<()>>,
    cancel_receiver: Option<oneshot::Receiver<()>>,
    error: Arc<Mutex<Option<SubscriptionError>>>,
}

impl CredentialStream {
    /// Build the generic collection's initial `added` array exactly once.
    /// Call after the WebSocket result ACK and before starting live forwarding.
    pub(crate) fn initial_snapshot(&self) -> Option<Value> {
        let records = self.subscription.as_ref()?.take_snapshot()?;
        Some(event_frame(
            self.id,
            records
                .into_iter()
                .map(|record| change_item("added", record))
                .collect(),
        ))
    }

    /// Begin forwarding the SDK's registered live change queue.
    pub(crate) fn start(&mut self) {
        let (Some(mut subscription), Some(mut cancel_receiver), Some(sender)) = (
            self.subscription.take(),
            self.cancel_receiver.take(),
            self.sender.take(),
        ) else {
            return;
        };
        let id = self.id;
        let error = Arc::clone(&self.error);
        self.task = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut cancel_receiver => return,
                    received = subscription.recv() => match received {
                        Ok(event) => {
                            let change_type = match event.change {
                                CredentialChangeType::Added => "added",
                                CredentialChangeType::Removed => "removed",
                            };
                            let frame = event_frame(id, vec![change_item(change_type, event.item)]);
                            tokio::select! {
                                biased;
                                _ = &mut cancel_receiver => return,
                                result = sender.send(frame) => if result.is_err() {
                                    return;
                                },
                            }
                        }
                        Err(subscription_error) => {
                            *lock(&error) = Some(subscription_error);
                            return;
                        }
                    }
                }
            }
        }));
    }

    /// Read one complete event frame without waiting.
    pub(crate) fn try_recv(&mut self) -> std::result::Result<Value, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Take and clear a terminal SDK lag or close error.
    pub(crate) fn take_error(&self) -> Option<SubscriptionError> {
        lock(&self.error).take()
    }

    /// Bounded event frames available to the WebSocket owner per poll.
    pub(crate) const fn capacity(&self) -> usize {
        STREAM_CHANNEL_CAPACITY
    }

    /// Whether the forwarder ended and its bounded queue has drained.
    pub(crate) fn is_finished(&self) -> bool {
        self.task.as_ref().is_some_and(JoinHandle::is_finished) && self.receiver.is_empty()
    }

    /// Stop forwarding and wait for the SDK subscription guard to drop.
    pub(crate) async fn cancel(mut self) {
        self.request_cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    fn request_cancel(&mut self) {
        self.subscription.take();
        self.sender.take();
        self.cancel_receiver.take();
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

impl Drop for CredentialStream {
    fn drop(&mut self) {
        self.request_cancel();
    }
}

fn ensure_fields(
    command: &Map<String, Value>,
    allowed: &[&str],
) -> std::result::Result<(), ApiError> {
    if command.keys().all(|key| allowed.contains(&key.as_str())) {
        Ok(())
    } else {
        Err(ApiError::invalid_format("Unexpected fields"))
    }
}

fn required_string<'a>(
    command: &'a Map<String, Value>,
    field: &str,
) -> std::result::Result<&'a str, ApiError> {
    command.get(field).and_then(Value::as_str).ok_or_else(|| {
        ApiError::invalid_format(format!("{field} is required and must be a string"))
    })
}

fn optional_string(
    command: &Map<String, Value>,
    field: &str,
) -> std::result::Result<Option<String>, ApiError> {
    command.get(field).map_or(Ok(None), |value| {
        value
            .as_str()
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| ApiError::invalid_format(format!("{field} must be a string")))
    })
}

fn change_item(change_type: &str, record: CredentialRecord) -> Value {
    let item_id = record.id.clone();
    json!({
        "change_type": change_type,
        "application_credentials_id": item_id,
        "item": record,
    })
}

fn event_frame(id: u64, changes: Vec<Value>) -> Value {
    json!({
        "id": id,
        "type": "event",
        "event": changes,
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
