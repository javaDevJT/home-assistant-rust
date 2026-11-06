//! Native port of Home Assistant's in-memory `persistent_notification` feed.
//!
//! Notifications live only for the lifetime of this integration instance. The
//! crate exposes the same create/dismiss service contract and private
//! dispatcher updates used by Home Assistant's WebSocket subscription.

use std::sync::{Arc, Mutex, Weak};

use chrono::{DateTime, Utc};
use hass_core::{
    Context, Error, Event, Result, Runtime, ServiceCall, ServiceHandler, ServiceResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;
use uuid::Uuid;

pub const DOMAIN: &str = "persistent_notification";
pub const SERVICE_CREATE: &str = "create";
pub const SERVICE_DISMISS: &str = "dismiss";
pub const SERVICE_DISMISS_ALL: &str = "dismiss_all";

/// The upstream dispatcher signal consumed by
/// `persistent_notification/subscribe`.
pub const SIGNAL_PERSISTENT_NOTIFICATIONS_UPDATED: &str = "persistent_notifications_updated";

/// Maximum UTF-8 size of a notification message.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Maximum UTF-8 size of a notification title.
pub const MAX_TITLE_BYTES: usize = 8 * 1024;
/// Maximum UTF-8 size of a notification ID.
pub const MAX_NOTIFICATION_ID_BYTES: usize = 512;
/// Maximum number of live notifications.
pub const MAX_NOTIFICATIONS: usize = 512;
/// Maximum encoded list size, leaving room under Runtime's private-signal cap.
pub const MAX_NOTIFICATION_FEED_BYTES: usize = 900 * 1024;
const MAX_PRIVATE_SIGNAL_BYTES: usize = 1024 * 1024;

/// Public notification object returned by `persistent_notification/get`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Notification {
    pub message: String,
    pub notification_id: String,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// The update names sent by `persistent_notification/subscribe`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NotificationUpdateType {
    Current,
    Added,
    Updated,
    Removed,
}

#[derive(Default)]
struct NotificationState {
    active: bool,
    entries: Vec<Notification>,
}

struct NotificationCore {
    state: Mutex<NotificationState>,
    // This is cleared by shutdown. Service handlers hold only Weak<Core> and
    // receive Runtime through the callback, so Runtime -> service -> Core does
    // not form an ownership cycle.
    signal_runtime: Mutex<Option<Runtime>>,
}

/// Cloneable read/write handle for one notification integration instance.
#[derive(Clone)]
pub struct NotificationManager {
    core: Arc<NotificationCore>,
}

/// Owns service registration and the lifetime of a notification feed.
pub struct NotificationIntegration {
    manager: NotificationManager,
    runtime: Option<Runtime>,
    owner: String,
    services_registered: bool,
}

impl NotificationIntegration {
    /// Register `persistent_notification.create`, `dismiss`, and `dismiss_all`
    /// on `runtime`, creating a fresh in-memory feed.
    pub fn start(runtime: Runtime) -> Result<Self> {
        ensure_services_available(&runtime)?;

        let owner = format!("{DOMAIN}.{}", Uuid::new_v4().simple());
        let core = Arc::new(NotificationCore {
            state: Mutex::new(NotificationState {
                active: true,
                entries: Vec::new(),
            }),
            signal_runtime: Mutex::new(Some(runtime.clone())),
        });

        for service in [SERVICE_CREATE, SERVICE_DISMISS, SERVICE_DISMISS_ALL] {
            let handler = service_handler(Arc::downgrade(&core), service);
            if let Err(error) = runtime.register_owned_service(
                &owner,
                DOMAIN,
                service,
                service_description(service),
                handler,
            ) {
                // A concurrent registration can race the preflight check. Roll
                // back any services already installed by this owner.
                deactivate(&core);
                let _ = runtime.unregister_owned_services(&owner);
                release_signal_runtime(&core);
                return Err(error);
            }
        }

        Ok(Self {
            manager: NotificationManager { core },
            runtime: Some(runtime),
            owner,
            services_registered: true,
        })
    }

    /// Return a cloneable handle for this integration's in-memory feed.
    pub fn manager(&self) -> NotificationManager {
        self.manager.clone()
    }

    /// Deactivate the feed, unregister its services, and release its Runtime
    /// reference. Existing manager clones remain readable but cannot mutate or
    /// subscribe after shutdown.
    pub fn shutdown(&mut self) -> Result<()> {
        deactivate(&self.manager.core);

        if self.services_registered {
            let runtime = self.runtime.as_ref().ok_or_else(|| {
                Error::Service("notification integration lost its Runtime".into())
            })?;
            runtime.unregister_owned_services(&self.owner)?;
            self.services_registered = false;
        }

        release_signal_runtime(&self.manager.core);
        self.runtime.take();
        Ok(())
    }
}

impl Drop for NotificationIntegration {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl NotificationManager {
    /// Return the current feed in insertion order, matching Python dict value
    /// order in the pinned implementation. Updating an ID does not move it.
    pub fn list(&self) -> Result<Vec<Notification>> {
        let state = self.lock_state();
        Ok(state.entries.clone())
    }

    /// Create a notification, or replace the entry with the same explicit ID.
    /// A replacement refreshes `created_at` while retaining its list position.
    pub fn create(
        &self,
        message: &str,
        title: Option<&str>,
        notification_id: Option<&str>,
    ) -> Result<Notification> {
        let runtime = self.live_runtime()?;
        create_with_runtime(
            &self.core,
            &runtime,
            Context::default(),
            message,
            title,
            notification_id,
        )
    }

    /// Replace the notification identified by `notification_id`, or add it if
    /// it is not present. This uses the same ID-based semantics as upstream's
    /// create service.
    pub fn update(
        &self,
        notification_id: &str,
        message: &str,
        title: Option<&str>,
    ) -> Result<Notification> {
        self.create(message, title, Some(notification_id))
    }

    /// Dismiss one notification. Returns `false` when the ID is not present;
    /// that no-op emits no dispatcher signal, as upstream does.
    pub fn dismiss(&self, notification_id: &str) -> Result<bool> {
        let runtime = self.live_runtime()?;
        dismiss_with_runtime(&self.core, &runtime, Context::default(), notification_id)
    }

    /// Dismiss every notification and publish one `removed` update, including
    /// when the feed is already empty (matching upstream behavior).
    pub fn dismiss_all(&self) -> Result<usize> {
        let runtime = self.live_runtime()?;
        dismiss_all_with_runtime(&self.core, &runtime, Context::default())
    }

    /// Atomically snapshot the feed and subscribe to subsequent private Runtime
    /// signals. The receiver sees the same `Event` objects sent through
    /// `persistent_notifications_updated`; callers should filter by
    /// `event_type` because this Runtime channel also carries other integrations'
    /// private signals.
    pub fn snapshot_and_subscribe(
        &self,
    ) -> Result<(Vec<Notification>, broadcast::Receiver<Event>)> {
        let state = self.lock_state();
        ensure_active(&state)?;
        let runtime = self.live_runtime()?;
        let receiver = runtime.subscribe_signals();
        Ok((state.entries.clone(), receiver))
    }

    fn live_runtime(&self) -> Result<Runtime> {
        self.core
            .signal_runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()
            .ok_or_else(|| Error::Service("notification integration is shut down".into()))
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, NotificationState> {
        self.core
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn deactivate(core: &NotificationCore) {
    let mut state = core
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.active = false;
}

fn release_signal_runtime(core: &NotificationCore) {
    core.signal_runtime
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
}

fn ensure_active(state: &NotificationState) -> Result<()> {
    if state.active {
        Ok(())
    } else {
        Err(Error::Service(
            "notification integration is shut down".into(),
        ))
    }
}

fn ensure_services_available(runtime: &Runtime) -> Result<()> {
    let registered = runtime.services();
    let Some(domain) = registered.get(DOMAIN).and_then(Value::as_object) else {
        return Ok(());
    };
    for service in [SERVICE_CREATE, SERVICE_DISMISS, SERVICE_DISMISS_ALL] {
        if domain.contains_key(service) {
            return Err(Error::Service(format!(
                "service {DOMAIN}.{service} is already registered"
            )));
        }
    }
    Ok(())
}

fn service_description(service: &str) -> Value {
    match service {
        SERVICE_CREATE => json!({
            "fields": {
                "message": {"required": true},
                "title": {"required": false},
                "notification_id": {"required": false}
            }
        }),
        SERVICE_DISMISS => json!({
            "fields": {
                "notification_id": {"required": true}
            }
        }),
        SERVICE_DISMISS_ALL => json!({"fields": {}}),
        _ => Value::Null,
    }
}

fn service_handler(core: Weak<NotificationCore>, service: &'static str) -> ServiceHandler {
    Arc::new(move |runtime, call| {
        let Some(core) = core.upgrade() else {
            return Err(Error::Service(
                "notification integration is unavailable".into(),
            ));
        };
        handle_service(&core, runtime, call, service)
    })
}

fn handle_service(
    core: &NotificationCore,
    runtime: &Runtime,
    call: &ServiceCall,
    service: &str,
) -> Result<ServiceResult> {
    match service {
        SERVICE_CREATE => {
            ensure_fields(&call.data, &["message", "title", "notification_id"])?;
            let message = required_string(&call.data, "message")?;
            let title = optional_string(&call.data, "title")?;
            let notification_id = optional_string(&call.data, "notification_id")?;
            create_with_runtime(
                core,
                runtime,
                call.context.clone(),
                &message,
                title.as_deref(),
                notification_id.as_deref(),
            )?;
        }
        SERVICE_DISMISS => {
            ensure_fields(&call.data, &["notification_id"])?;
            let notification_id = required_string(&call.data, "notification_id")?;
            dismiss_with_runtime(core, runtime, call.context.clone(), &notification_id)?;
        }
        SERVICE_DISMISS_ALL => {
            ensure_fields(&call.data, &[])?;
            dismiss_all_with_runtime(core, runtime, call.context.clone())?;
        }
        _ => {
            return Err(Error::Service(format!(
                "unknown {DOMAIN} service {service}"
            )));
        }
    }

    Ok(ServiceResult {
        changed_states: Vec::new(),
        response: None,
        context: call.context.clone(),
    })
}

fn ensure_fields(data: &Map<String, Value>, allowed: &[&str]) -> Result<()> {
    if let Some(field) = data.keys().find(|field| !allowed.contains(&field.as_str())) {
        return Err(Error::InvalidInput(format!(
            "unexpected service field {field}"
        )));
    }
    Ok(())
}

fn required_string(data: &Map<String, Value>, field: &str) -> Result<String> {
    let value = data
        .get(field)
        .ok_or_else(|| Error::InvalidInput(format!("{field} is required")))?;
    coerce_string(value, field)
}

fn optional_string(data: &Map<String, Value>, field: &str) -> Result<Option<String>> {
    data.get(field)
        .map(|value| coerce_string(value, field))
        .transpose()
}

/// Match pinned cv.string for JSON scalars, including Python bool spelling
/// and its short scientific float representation.
fn coerce_string(value: &Value, field: &str) -> Result<String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Bool(value) => Ok(if *value { "True" } else { "False" }.into()),
        Value::Number(value) if value.is_f64() => {
            let number = value
                .as_f64()
                .ok_or_else(|| Error::InvalidInput(format!("Invalid {field} number")))?;
            let value = format!("{number:?}");
            if let Some((base, exponent)) = value.split_once('e') {
                let exponent = exponent
                    .parse::<i32>()
                    .map_err(|_| Error::InvalidInput(format!("Invalid {field} number")))?;
                Ok(format!(
                    "{base}e{}{abs:02}",
                    if exponent < 0 { "-" } else { "+" },
                    abs = exponent.unsigned_abs()
                ))
            } else {
                Ok(value)
            }
        }
        Value::Number(value) => Ok(value.to_string()),
        _ => Err(Error::InvalidInput(format!(
            "{field} must be a scalar string value"
        ))),
    }
}

fn validate_text_lengths(
    message: &str,
    title: Option<&str>,
    notification_id: Option<&str>,
) -> Result<()> {
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(Error::InvalidInput(format!(
            "message exceeds {MAX_MESSAGE_BYTES} UTF-8 bytes"
        )));
    }
    if title.is_some_and(|title| title.len() > MAX_TITLE_BYTES) {
        return Err(Error::InvalidInput(format!(
            "title exceeds {MAX_TITLE_BYTES} UTF-8 bytes"
        )));
    }
    if notification_id.is_some_and(|id| id.len() > MAX_NOTIFICATION_ID_BYTES) {
        return Err(Error::InvalidInput(format!(
            "notification_id exceeds {MAX_NOTIFICATION_ID_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn create_with_runtime(
    core: &NotificationCore,
    runtime: &Runtime,
    context: Context,
    message: &str,
    title: Option<&str>,
    notification_id: Option<&str>,
) -> Result<Notification> {
    validate_text_lengths(message, title, notification_id)?;
    let notification_id = notification_id
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
    let notification = Notification {
        message: message.to_owned(),
        notification_id: notification_id.clone(),
        title: title.map(str::to_owned),
        created_at: Utc::now(),
    };

    let mut state = core
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_active(&state)?;

    let existing = state
        .entries
        .iter()
        .position(|entry| entry.notification_id == notification_id);
    let update_type = if existing.is_some() {
        NotificationUpdateType::Updated
    } else {
        NotificationUpdateType::Added
    };

    let mut candidate = state.entries.clone();
    if let Some(index) = existing {
        candidate[index] = notification.clone();
    } else {
        if candidate.len() >= MAX_NOTIFICATIONS {
            return Err(Error::InvalidInput(format!(
                "notification feed exceeds {MAX_NOTIFICATIONS} entries"
            )));
        }
        candidate.push(notification.clone());
    }

    validate_feed(&candidate)?;
    let data = update_data(update_type, std::slice::from_ref(&notification))?;
    let previous = std::mem::replace(&mut state.entries, candidate);
    if let Err(error) =
        runtime.dispatch_signal(SIGNAL_PERSISTENT_NOTIFICATIONS_UPDATED, data, context)
    {
        state.entries = previous;
        return Err(error);
    }
    Ok(notification)
}

fn dismiss_with_runtime(
    core: &NotificationCore,
    runtime: &Runtime,
    context: Context,
    notification_id: &str,
) -> Result<bool> {
    validate_text_lengths("", None, Some(notification_id))?;
    let mut state = core
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_active(&state)?;

    let Some(index) = state
        .entries
        .iter()
        .position(|entry| entry.notification_id == notification_id)
    else {
        return Ok(false);
    };
    let removed = state.entries[index].clone();
    let candidate: Vec<_> = state
        .entries
        .iter()
        .enumerate()
        .filter(|(current, _)| *current != index)
        .map(|(_, entry)| entry.clone())
        .collect();
    validate_feed(&candidate)?;
    let data = update_data(
        NotificationUpdateType::Removed,
        std::slice::from_ref(&removed),
    )?;
    let previous = std::mem::replace(&mut state.entries, candidate);
    if let Err(error) =
        runtime.dispatch_signal(SIGNAL_PERSISTENT_NOTIFICATIONS_UPDATED, data, context)
    {
        state.entries = previous;
        return Err(error);
    }
    Ok(true)
}

fn dismiss_all_with_runtime(
    core: &NotificationCore,
    runtime: &Runtime,
    context: Context,
) -> Result<usize> {
    let mut state = core
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_active(&state)?;

    let removed_count = state.entries.len();
    let data = update_data(NotificationUpdateType::Removed, &state.entries)?;
    let previous = std::mem::take(&mut state.entries);
    if let Err(error) =
        runtime.dispatch_signal(SIGNAL_PERSISTENT_NOTIFICATIONS_UPDATED, data, context)
    {
        state.entries = previous;
        return Err(error);
    }
    Ok(removed_count)
}

fn validate_feed(entries: &[Notification]) -> Result<()> {
    if entries.len() > MAX_NOTIFICATIONS {
        return Err(Error::InvalidInput(format!(
            "notification feed exceeds {MAX_NOTIFICATIONS} entries"
        )));
    }
    let encoded = serde_json::to_vec(entries)
        .map_err(|_| Error::InvalidInput("notification feed is not JSON serializable".into()))?;
    if encoded.len() > MAX_NOTIFICATION_FEED_BYTES {
        return Err(Error::InvalidInput(format!(
            "notification feed exceeds {MAX_NOTIFICATION_FEED_BYTES} encoded bytes"
        )));
    }
    Ok(())
}

fn update_data(
    update_type: NotificationUpdateType,
    notifications: &[Notification],
) -> Result<Map<String, Value>> {
    let mut keyed_notifications = Map::new();
    for notification in notifications {
        let encoded = serde_json::to_value(notification)
            .map_err(|_| Error::InvalidInput("notification is not JSON serializable".into()))?;
        keyed_notifications.insert(notification.notification_id.clone(), encoded);
    }

    let mut data = Map::new();
    data.insert(
        "type".into(),
        serde_json::to_value(update_type)
            .map_err(|_| Error::InvalidInput("notification update type is invalid".into()))?,
    );
    data.insert("notifications".into(), Value::Object(keyed_notifications));

    let encoded = serde_json::to_vec(&data)
        .map_err(|_| Error::InvalidInput("notification update is not JSON serializable".into()))?;
    if encoded.len() >= MAX_PRIVATE_SIGNAL_BYTES {
        return Err(Error::InvalidInput(format!(
            "notification update must be smaller than {MAX_PRIVATE_SIGNAL_BYTES} encoded bytes"
        )));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::MemoryStore;
    use serde_json::Value;
    use std::sync::Arc;
    use tokio::sync::broadcast::error::TryRecvError;

    fn runtime() -> (Runtime, Arc<MemoryStore>) {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).expect("create runtime");
        (runtime, store)
    }

    fn service_call(service: &str, data: Value) -> ServiceCall {
        ServiceCall {
            domain: DOMAIN.into(),
            service: service.into(),
            data: data.as_object().cloned().unwrap_or_default(),
            target: Map::new(),
            context: Context::default(),
        }
    }

    fn call(runtime: &Runtime, service: &str, data: Value) -> Result<ServiceResult> {
        runtime.call_service(service_call(service, data))
    }

    fn next_signal(receiver: &mut broadcast::Receiver<Event>) -> Event {
        receiver.try_recv().expect("private notification signal")
    }

    fn notification_map(event: &Event) -> &Map<String, Value> {
        event.data["notifications"]
            .as_object()
            .expect("keyed notification update")
    }

    #[test]
    fn service_create_update_and_get_shapes_preserve_source_order() {
        let (runtime, store) = runtime();
        let integration = NotificationIntegration::start(runtime.clone()).expect("integration");
        let manager = integration.manager();
        let mut signals = runtime.subscribe_signals();
        let mut public_events = runtime.subscribe();

        let result = call(
            &runtime,
            SERVICE_CREATE,
            json!({"message":"z message","title":"Z title","notification_id":"z"}),
        )
        .expect("create z");
        assert!(result.response.is_none());
        let added_z = next_signal(&mut signals);
        assert_eq!(added_z.event_type, SIGNAL_PERSISTENT_NOTIFICATIONS_UPDATED);
        assert_eq!(added_z.data["type"], "added");
        assert_eq!(notification_map(&added_z)["z"]["message"], "z message");

        call(
            &runtime,
            SERVICE_CREATE,
            json!({"message":"a message","notification_id":"a"}),
        )
        .expect("create a");
        let added_a = next_signal(&mut signals);
        assert_eq!(added_a.data["type"], "added");
        assert_eq!(notification_map(&added_a)["a"]["title"], Value::Null);

        let before_update = manager.list().expect("read notifications");
        assert_eq!(
            before_update
                .iter()
                .map(|notification| notification.notification_id.as_str())
                .collect::<Vec<_>>(),
            ["z", "a"]
        );
        let z_created_at = before_update[0].created_at;

        call(
            &runtime,
            SERVICE_CREATE,
            json!({"message":"updated z","title":"new title","notification_id":"z"}),
        )
        .expect("update z through create service");
        let updated_z = next_signal(&mut signals);
        assert_eq!(updated_z.data["type"], "updated");
        assert_eq!(notification_map(&updated_z)["z"]["message"], "updated z");

        let listed = manager.list().expect("read updated feed");
        assert_eq!(listed[0].notification_id, "z");
        assert_eq!(listed[0].message, "updated z");
        assert!(listed[0].created_at > z_created_at);
        assert_eq!(listed[1].notification_id, "a");
        let wire_list = serde_json::to_value(&listed).expect("serialize public notification list");
        assert_eq!(wire_list[0]["notification_id"], "z");
        assert_eq!(wire_list[0]["title"], "new title");
        assert!(wire_list[0]["created_at"].as_str().is_some());

        let typed_update = manager
            .update("z", "typed update", None)
            .expect("typed update API");
        assert_eq!(typed_update.message, "typed update");
        let typed_update_event = next_signal(&mut signals);
        assert_eq!(typed_update_event.data["type"], "updated");
        assert_eq!(
            manager.list().expect("list after typed update")[0].notification_id,
            "z"
        );
        assert!(matches!(public_events.try_recv(), Err(TryRecvError::Empty)));
        assert!(store.events().expect("read durable events").is_empty());
    }

    #[test]
    fn snapshot_subscribes_atomically_and_receives_private_updates() {
        let (runtime, _store) = runtime();
        let integration = NotificationIntegration::start(runtime.clone()).expect("integration");
        let manager = integration.manager();
        call(
            &runtime,
            SERVICE_CREATE,
            json!({"message":"existing","notification_id":"before"}),
        )
        .expect("initial create");

        let (snapshot, mut receiver) = manager
            .snapshot_and_subscribe()
            .expect("atomic snapshot and subscribe");
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].notification_id, "before");

        call(
            &runtime,
            SERVICE_CREATE,
            json!({"message":"after","notification_id":"after"}),
        )
        .expect("create after subscribe");
        let event = next_signal(&mut receiver);
        assert_eq!(event.event_type, SIGNAL_PERSISTENT_NOTIFICATIONS_UPDATED);
        assert_eq!(event.data["type"], "added");
        assert_eq!(notification_map(&event)["after"]["message"], "after");
    }

    #[test]
    fn dismiss_and_dismiss_all_publish_removed_entries_and_noop_rules() {
        let (runtime, _store) = runtime();
        let integration = NotificationIntegration::start(runtime.clone()).expect("integration");
        let manager = integration.manager();
        let mut signals = runtime.subscribe_signals();

        for id in ["one", "two"] {
            call(
                &runtime,
                SERVICE_CREATE,
                json!({"message":id,"notification_id":id}),
            )
            .expect("create notification");
            let _ = next_signal(&mut signals);
        }

        call(&runtime, SERVICE_DISMISS, json!({"notification_id":"one"})).expect("dismiss one");
        let removed = next_signal(&mut signals);
        assert_eq!(removed.data["type"], "removed");
        assert_eq!(notification_map(&removed)["one"]["message"], "one");
        assert_eq!(manager.list().expect("list").len(), 1);

        assert!(!manager.dismiss("missing").expect("missing dismissal"));
        assert!(matches!(signals.try_recv(), Err(TryRecvError::Empty)));

        call(&runtime, SERVICE_DISMISS_ALL, json!({})).expect("dismiss all");
        let removed_all = next_signal(&mut signals);
        assert_eq!(removed_all.data["type"], "removed");
        assert_eq!(notification_map(&removed_all).len(), 1);
        assert!(manager.list().expect("empty feed").is_empty());

        call(&runtime, SERVICE_DISMISS_ALL, json!({})).expect("dismiss all empty feed");
        let removed_empty = next_signal(&mut signals);
        assert_eq!(removed_empty.data["type"], "removed");
        assert!(notification_map(&removed_empty).is_empty());
    }

    #[test]
    fn invalid_service_data_and_oversized_updates_leave_feed_unchanged() {
        let (runtime, _store) = runtime();
        let integration = NotificationIntegration::start(runtime.clone()).expect("integration");
        let manager = integration.manager();
        let mut signals = runtime.subscribe_signals();

        assert!(call(&runtime, SERVICE_CREATE, json!({"message":[]})).is_err());
        assert!(call(&runtime, SERVICE_CREATE, json!({"title":"missing message"})).is_err());
        assert!(
            call(
                &runtime,
                SERVICE_CREATE,
                json!({"message":"ok","unexpected":true})
            )
            .is_err()
        );
        assert!(
            call(
                &runtime,
                SERVICE_CREATE,
                json!({"message":"ok","title":null})
            )
            .is_err()
        );
        assert!(manager.list().expect("empty list").is_empty());
        assert!(matches!(signals.try_recv(), Err(TryRecvError::Empty)));

        // cv.string accepts empty strings, so the native port preserves them.
        let empty = manager
            .create("", Some(""), Some(""))
            .expect("empty strings remain valid string values");
        assert_eq!(empty.message, "");
        assert_eq!(empty.title.as_deref(), Some(""));
        assert_eq!(empty.notification_id, "");
        let _ = next_signal(&mut signals);

        let too_long = "x".repeat(MAX_MESSAGE_BYTES + 1);
        assert!(manager.create(&too_long, None, Some("new")).is_err());
        let too_long_title = "t".repeat(MAX_TITLE_BYTES + 1);
        assert!(
            manager
                .create("valid message", Some(&too_long_title), Some("new"))
                .is_err()
        );
        let too_long_id = "i".repeat(MAX_NOTIFICATION_ID_BYTES + 1);
        assert!(
            manager
                .create("valid message", None, Some(&too_long_id))
                .is_err()
        );
        assert_eq!(manager.list().expect("read after invalid").len(), 1);
        assert!(matches!(signals.try_recv(), Err(TryRecvError::Empty)));

        // Per-field limits are not enough: the aggregate feed is also capped.
        let message = "m".repeat(MAX_MESSAGE_BYTES);
        for index in 0..14 {
            manager
                .create(&message, None, Some(&format!("bulk-{index}")))
                .expect("feed still below aggregate limit");
            let _ = next_signal(&mut signals);
        }
        let before_reject = manager.list().expect("list before aggregate reject");
        assert_eq!(before_reject.len(), 15);
        assert!(
            manager
                .create(&message, None, Some("aggregate-overflow"))
                .is_err()
        );
        assert_eq!(
            manager.list().expect("list after aggregate reject"),
            before_reject
        );
        assert!(matches!(signals.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn service_scalar_coercion_matches_pinned_cv_string() {
        let (runtime, _store) = runtime();
        let integration = NotificationIntegration::start(runtime.clone()).unwrap();
        call(
            &runtime,
            SERVICE_CREATE,
            json!({"message":3,"title":false,"notification_id":42}),
        )
        .unwrap();
        let entries = integration.manager().list().unwrap();
        assert_eq!(entries[0].message, "3");
        assert_eq!(entries[0].title.as_deref(), Some("False"));
        assert_eq!(entries[0].notification_id, "42");
        for (json, expected) in [
            ("1.0", "1.0"),
            ("1e-5", "1e-05"),
            ("1e16", "1e+16"),
            ("-0.0", "-0.0"),
        ] {
            let value: Value = serde_json::from_str(json).unwrap();
            assert_eq!(coerce_string(&value, "message").unwrap(), expected);
        }
        call(&runtime, SERVICE_DISMISS, json!({"notification_id":42})).unwrap();
        assert!(integration.manager().list().unwrap().is_empty());
    }

    #[test]
    fn entry_limit_rejects_without_eviction() {
        let (runtime, _store) = runtime();
        let integration = NotificationIntegration::start(runtime).expect("integration");
        let manager = integration.manager();
        for index in 0..MAX_NOTIFICATIONS {
            manager
                .create("message", None, Some(&format!("id-{index}")))
                .expect("entry below limit");
        }
        assert_eq!(
            manager.list().expect("list at limit").len(),
            MAX_NOTIFICATIONS
        );
        assert!(manager.create("extra", None, Some("extra")).is_err());
        assert_eq!(
            manager.list().expect("list after reject").len(),
            MAX_NOTIFICATIONS
        );
    }

    #[test]
    fn conflicting_service_registration_is_rejected_before_partial_setup() {
        let (runtime, _store) = runtime();
        let legacy: ServiceHandler = Arc::new(|_, call| {
            Ok(ServiceResult {
                changed_states: Vec::new(),
                response: None,
                context: call.context.clone(),
            })
        });
        runtime
            .register_service(DOMAIN, SERVICE_DISMISS, json!({"legacy":true}), legacy)
            .expect("register conflicting service");

        assert!(NotificationIntegration::start(runtime.clone()).is_err());
        let services = runtime.services();
        let domain = services[DOMAIN].as_object().expect("service domain");
        assert_eq!(domain.len(), 1);
        assert!(domain.contains_key(SERVICE_DISMISS));
        assert!(!domain.contains_key(SERVICE_CREATE));
        assert!(!domain.contains_key(SERVICE_DISMISS_ALL));
    }

    #[test]
    fn shutdown_and_drop_unregister_services_and_deactivate_manager_clones() {
        let (runtime, _store) = runtime();
        let mut integration = NotificationIntegration::start(runtime.clone()).expect("integration");
        let manager = integration.manager();
        manager
            .create("before shutdown", None, Some("one"))
            .expect("create notification");

        integration.shutdown().expect("shutdown");
        assert!(runtime.services().get(DOMAIN).is_none());
        assert_eq!(manager.list().expect("retained read handle").len(), 1);
        assert!(manager.create("after shutdown", None, Some("two")).is_err());
        assert!(manager.snapshot_and_subscribe().is_err());
        assert!(
            call(
                &runtime,
                SERVICE_CREATE,
                json!({"message":"service removed"})
            )
            .is_err()
        );

        let second = NotificationIntegration::start(runtime.clone()).expect("restart services");
        let second_manager = second.manager();
        assert!(second_manager.list().expect("fresh instance").is_empty());
        drop(second);
        assert!(runtime.services().get(DOMAIN).is_none());
        assert!(
            second_manager
                .create("after drop", None, Some("three"))
                .is_err()
        );
    }

    #[test]
    fn restart_with_same_store_does_not_restore_notifications_or_retain_writer_lease() {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).expect("first Runtime");
        let mut integration = NotificationIntegration::start(runtime.clone()).expect("integration");
        let manager = integration.manager();
        manager
            .create("ephemeral", Some("title"), Some("ephemeral-id"))
            .expect("create notification");

        integration.shutdown().expect("shutdown");
        assert_eq!(manager.list().expect("old manager snapshot").len(), 1);
        drop(integration);
        drop(runtime);

        // Keep the old, inactive manager clone alive while reacquiring the same
        // Store. It must no longer retain Runtime's single-writer lease.
        let restarted_runtime = Runtime::new(store).expect("reconstruct Runtime over same Store");
        let restarted = NotificationIntegration::start(restarted_runtime).expect("new feed");
        assert!(
            restarted
                .manager()
                .list()
                .expect("new feed list")
                .is_empty()
        );
    }
}
