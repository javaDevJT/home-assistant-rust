//! Native WebSocket subscriptions backed by the runtime's real event streams.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use hass_config::ConfigEntries;
use hass_core::{Error, Event, Result, Runtime, State};
use hass_template::TemplateEngine;
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;

const MAX_SUBSCRIPTIONS: usize = 32;
const MAX_EVENTS_PER_DRAIN: usize = 1024;
const MAX_EVENTS_PER_SUBSCRIPTION_DRAIN: usize = 128;
const EVENT_MATCH_ALL: &str = "*";

/// The WebSocket response associated with one recognized command.
///
/// Subscription acknowledgments are intentionally separate from their initial
/// event envelopes. The transport can send the result first, followed by the
/// event list, while keeping the same command ID on both.
pub(crate) struct SubscriptionCommandResult {
    pub result: Value,
    pub events: Vec<Value>,
}

/// Per-connection subscriptions to public runtime, config, and template data.
pub(crate) struct NativeSubscriptions {
    runtime: Runtime,
    config_entries: Option<ConfigEntries>,
    template_engine: Option<TemplateEngine>,
    subscriptions: BTreeMap<u64, ActiveSubscription>,
    drain_cursor: usize,
    allow_state_read: bool,
    notifications: Option<hass_notifications::NotificationManager>,
    zones: Option<hass_zone::ZoneManager>,
    persons: Option<hass_person::PersonManager>,
}

enum ActiveSubscription {
    Persons {
        receiver: broadcast::Receiver<Event>,
    },
    Zones {
        receiver: broadcast::Receiver<Event>,
    },
    Notifications {
        receiver: broadcast::Receiver<Event>,
    },
    Events {
        receiver: broadcast::Receiver<Event>,
        event_type: String,
        allow_state_read: bool,
    },
    Entities {
        receiver: broadcast::Receiver<Event>,
        entity_ids: Option<HashSet<String>>,
        filter: EntityFilter,
    },
    ConfigEntries {
        receiver: broadcast::Receiver<Event>,
        type_filter: Option<HashSet<String>>,
        visible_entries: HashMap<String, (String, Value)>,
    },
    ConfigFlowProgress {
        receiver: broadcast::Receiver<Event>,
        active_flow_ids: HashSet<String>,
    },
    Template {
        receiver: broadcast::Receiver<Event>,
        template: String,
        variables: Map<String, Value>,
        listeners: Value,
        last_result: Option<String>,
        last_error: Option<String>,
        report_errors: bool,
    },
}

#[derive(Default)]
struct EntityFilter {
    deny_all: bool,
    include_entities: HashSet<String>,
    include_domains: HashSet<String>,
    include_globs: Vec<String>,
    exclude_entities: HashSet<String>,
    exclude_domains: HashSet<String>,
    exclude_globs: Vec<String>,
}

impl NativeSubscriptions {
    pub(crate) fn new(
        runtime: Runtime,
        config_entries: Option<ConfigEntries>,
        template_engine: Option<TemplateEngine>,
    ) -> Self {
        Self {
            runtime,
            config_entries,
            template_engine,
            subscriptions: BTreeMap::new(),
            drain_cursor: 0,
            allow_state_read: true,
            notifications: None,
            zones: None,
            persons: None,
        }
    }

    pub(crate) fn set_state_read_access(&mut self, allowed: bool) {
        self.allow_state_read = allowed;
    }

    pub(crate) fn set_notifications(
        &mut self,
        manager: Option<hass_notifications::NotificationManager>,
    ) {
        self.notifications = manager;
    }

    pub(crate) fn set_zones(&mut self, manager: Option<hass_zone::ZoneManager>) {
        self.zones = manager;
    }

    pub(crate) fn set_persons(&mut self, manager: Option<hass_person::PersonManager>) {
        self.persons = manager;
    }

    /// Handle a subscription-related WebSocket command, if this manager owns it.
    pub(crate) fn handle_command(
        &mut self,
        command: &Value,
    ) -> Result<Option<SubscriptionCommandResult>> {
        let object = command
            .as_object()
            .ok_or_else(|| Error::InvalidInput("WebSocket command must be an object".into()))?;
        let command_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::InvalidInput("WebSocket command type must be a string".into()))?;

        match command_type {
            "person/subscribe" => self.subscribe_persons(object).map(Some),
            "zone/subscribe" => self.subscribe_zones(object).map(Some),
            "persistent_notification/subscribe" => self.subscribe_notifications(object).map(Some),
            "subscribe_events" => self.subscribe_events(object).map(Some),
            "subscribe_entities" => self.subscribe_entities(object).map(Some),
            "unsubscribe_events" => self.unsubscribe_events(object).map(Some),
            "config_entries/subscribe" => self.subscribe_config_entries(object).map(Some),
            "config_entries/flow/subscribe" => {
                self.subscribe_config_flow_progress(object).map(Some)
            }
            "config_entries/flow/progress" => {
                let entries = self.require_config_entries()?;
                Ok(Some(SubscriptionCommandResult {
                    result: json!(config_flow_progress(entries)?),
                    events: Vec::new(),
                }))
            }
            "render_template" => self.subscribe_template(object).map(Some),
            other if is_subscription_command(other) => Err(Error::InvalidInput(format!(
                "unsupported subscription command {other:?}"
            ))),
            _ => Ok(None),
        }
    }

    /// Drain queued notifications without allowing one busy client to monopolize
    /// the WebSocket task. A lagged receiver is removed and reported as an error.
    pub(crate) fn drain_events(&mut self) -> Result<Vec<Value>> {
        if self.subscriptions.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<u64> = self.subscriptions.keys().copied().collect();
        let start = self.drain_cursor % ids.len();
        self.drain_cursor = (start + 1) % ids.len();
        let ordered_ids: Vec<u64> = ids[start..]
            .iter()
            .chain(ids[..start].iter())
            .copied()
            .collect();

        let mut output = Vec::new();
        let mut per_subscription = HashMap::<u64, usize>::new();
        let mut total = 0usize;
        let mut remove = Vec::new();
        let mut lag_error = None;

        while total < MAX_EVENTS_PER_DRAIN {
            let mut made_progress = false;
            for id in &ordered_ids {
                if total >= MAX_EVENTS_PER_DRAIN {
                    break;
                }
                if per_subscription.get(id).copied().unwrap_or_default()
                    >= MAX_EVENTS_PER_SUBSCRIPTION_DRAIN
                {
                    continue;
                }

                let receive = self
                    .subscriptions
                    .get_mut(id)
                    .map(ActiveSubscription::try_recv);
                let Some(receive) = receive else {
                    continue;
                };

                match receive {
                    Ok(event) => {
                        made_progress = true;
                        total += 1;
                        *per_subscription.entry(*id).or_default() += 1;
                        self.forward_event(*id, &event, &mut output)?;
                    }
                    Err(broadcast::error::TryRecvError::Empty) => {}
                    Err(broadcast::error::TryRecvError::Closed) => remove.push(*id),
                    Err(broadcast::error::TryRecvError::Lagged(missed)) => {
                        remove.push(*id);
                        if lag_error.is_none() {
                            lag_error = Some(Error::Service(format!(
                                "subscription {id} fell behind and missed {missed} events"
                            )));
                        }
                    }
                }
            }

            if !made_progress {
                break;
            }
        }

        remove.sort_unstable();
        remove.dedup();
        for id in remove {
            self.subscriptions.remove(&id);
        }
        if let Some(error) = lag_error {
            return Err(error);
        }
        Ok(output)
    }

    pub(crate) fn len(&self) -> usize {
        self.subscriptions.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn subscribe_events(
        &mut self,
        object: &Map<String, Value>,
    ) -> Result<SubscriptionCommandResult> {
        let id = command_id(object)?;
        self.ensure_capacity(id)?;
        let event_type = optional_string(object, "event_type")?
            .unwrap_or(EVENT_MATCH_ALL)
            .to_owned();
        let receiver = self.runtime.subscribe();
        self.subscriptions.insert(
            id,
            ActiveSubscription::Events {
                receiver,
                event_type,
                allow_state_read: self.allow_state_read,
            },
        );
        Ok(success_result(Vec::new()))
    }

    fn subscribe_persons(
        &mut self,
        object: &Map<String, Value>,
    ) -> Result<SubscriptionCommandResult> {
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "id" | "type"))
        {
            return Err(Error::InvalidInput(
                "Invalid person subscription command".into(),
            ));
        }
        let id = command_id(object)?;
        self.ensure_capacity(id)?;
        let manager = self
            .persons
            .as_ref()
            .ok_or_else(|| Error::NotFound("Person integration is not configured".into()))?;
        let (snapshot, receiver) = manager.snapshot_and_subscribe()?;
        let initial = snapshot
            .storage
            .into_iter()
            .map(|person| json!({"change_type":"added","person_id":person.id,"item":person}))
            .collect::<Vec<_>>();
        self.subscriptions
            .insert(id, ActiveSubscription::Persons { receiver });
        Ok(success_result(vec![event_envelope(id, json!(initial))]))
    }

    fn subscribe_zones(
        &mut self,
        object: &Map<String, Value>,
    ) -> Result<SubscriptionCommandResult> {
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "id" | "type"))
        {
            return Err(Error::InvalidInput(
                "Invalid zone subscription command".into(),
            ));
        }
        let id = command_id(object)?;
        self.ensure_capacity(id)?;
        let manager = self
            .zones
            .as_ref()
            .ok_or_else(|| Error::NotFound("Zone integration is not configured".into()))?;
        let (snapshot, receiver) = manager.snapshot_and_subscribe()?;
        let initial = snapshot
            .into_iter()
            .map(|zone| json!({"change_type":"added","zone_id":zone.id,"item":zone}))
            .collect::<Vec<_>>();
        self.subscriptions
            .insert(id, ActiveSubscription::Zones { receiver });
        Ok(success_result(vec![event_envelope(id, json!(initial))]))
    }

    fn subscribe_notifications(
        &mut self,
        object: &Map<String, Value>,
    ) -> Result<SubscriptionCommandResult> {
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "id" | "type"))
        {
            return Err(Error::InvalidInput(
                "Invalid notification subscription command".into(),
            ));
        }
        let id = command_id(object)?;
        self.ensure_capacity(id)?;
        let manager = self
            .notifications
            .as_ref()
            .ok_or_else(|| Error::NotFound("Notification integration is not configured".into()))?;
        let (snapshot, receiver) = manager.snapshot_and_subscribe()?;
        let initial = snapshot
            .into_iter()
            .map(|notification| {
                let id = notification.notification_id.clone();
                serde_json::to_value(notification)
                    .map(|value| (id, value))
                    .map_err(|_| Error::Service("Cannot serialize notification".into()))
            })
            .collect::<Result<Map<String, Value>>>()?;
        self.subscriptions
            .insert(id, ActiveSubscription::Notifications { receiver });
        Ok(success_result(vec![event_envelope(
            id,
            json!({"type":"current","notifications":initial}),
        )]))
    }

    fn subscribe_entities(
        &mut self,
        object: &Map<String, Value>,
    ) -> Result<SubscriptionCommandResult> {
        let id = command_id(object)?;
        self.ensure_capacity(id)?;
        let entity_ids = parse_entity_ids(object.get("entity_ids"))?;
        let mut filter = EntityFilter::parse(object)?;
        filter.deny_all = !self.allow_state_read;
        let (states, receiver) = self.runtime.subscribe_with_states();
        let initial_states = states
            .iter()
            .filter(|state| entity_matches(state.entity_id.as_str(), &entity_ids, &filter))
            .map(|state| (state.entity_id.clone(), compressed_state(state)))
            .collect::<BTreeMap<_, _>>();
        let initial_event = event_envelope(id, json!({ "a": initial_states }));

        self.subscriptions.insert(
            id,
            ActiveSubscription::Entities {
                receiver,
                entity_ids,
                filter,
            },
        );
        Ok(success_result(vec![initial_event]))
    }

    fn unsubscribe_events(
        &mut self,
        object: &Map<String, Value>,
    ) -> Result<SubscriptionCommandResult> {
        let _command_id = command_id(object)?;
        let subscription = object
            .get("subscription")
            .and_then(Value::as_u64)
            .filter(|id| *id > 0)
            .ok_or_else(|| Error::InvalidInput("subscription must be a positive integer".into()))?;
        if self.subscriptions.remove(&subscription).is_none() {
            return Err(Error::NotFound(format!(
                "subscription {subscription} not found"
            )));
        }
        Ok(success_result(Vec::new()))
    }

    fn subscribe_config_entries(
        &mut self,
        object: &Map<String, Value>,
    ) -> Result<SubscriptionCommandResult> {
        let id = command_id(object)?;
        self.ensure_capacity(id)?;
        let entries = self.require_config_entries()?.clone();
        let type_filter = parse_type_filter(object.get("type_filter"))?;
        // Subscribe before reading the snapshot; changes during the read remain queued.
        let receiver = self.runtime.subscribe();
        let current = entries.list_entries(None);
        let mut visible_entries = HashMap::new();
        let mut initial_entries = Vec::new();
        for entry in current {
            if type_filter
                .as_ref()
                .is_some_and(|types| !types.contains(&entry.integration_type))
            {
                continue;
            }
            let public_entry = entry.public_json();
            visible_entries.insert(
                entry.entry_id.clone(),
                (entry.integration_type.clone(), public_entry.clone()),
            );
            initial_entries.push(json!({"type": null, "entry": public_entry}));
        }

        self.subscriptions.insert(
            id,
            ActiveSubscription::ConfigEntries {
                receiver,
                type_filter,
                visible_entries,
            },
        );
        Ok(success_result(vec![event_envelope(
            id,
            json!(initial_entries),
        )]))
    }

    fn subscribe_config_flow_progress(
        &mut self,
        object: &Map<String, Value>,
    ) -> Result<SubscriptionCommandResult> {
        let id = command_id(object)?;
        self.ensure_capacity(id)?;
        let entries = self.require_config_entries()?.clone();
        // ConfigEntries emits only the narrowly scoped flow-progress signal here.
        // This private channel is never used for public subscribe_events.
        let receiver = self.runtime.subscribe_signals();
        let progress = config_flow_progress(&entries)?;
        let active_flow_ids = progress
            .iter()
            .filter_map(|flow| flow.get("flow_id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        let initial = progress
            .iter()
            .filter_map(|flow| flow_progress_event_item(flow, None))
            .collect::<Vec<_>>();

        self.subscriptions.insert(
            id,
            ActiveSubscription::ConfigFlowProgress {
                receiver,
                active_flow_ids,
            },
        );
        Ok(success_result(vec![event_envelope(id, json!(initial))]))
    }

    fn subscribe_template(
        &mut self,
        object: &Map<String, Value>,
    ) -> Result<SubscriptionCommandResult> {
        let id = command_id(object)?;
        self.ensure_capacity(id)?;
        let template = required_string(object, "template")?.to_owned();
        let variables = match object.get("variables") {
            Some(Value::Object(variables)) => variables.clone(),
            Some(_) => {
                return Err(Error::InvalidInput(
                    "template variables must be an object".into(),
                ));
            }
            None => Map::new(),
        };
        if let Some(timeout) = object.get("timeout")
            && !timeout.is_number()
        {
            return Err(Error::InvalidInput(
                "template timeout must be a number".into(),
            ));
        }
        if let Some(strict) = object.get("strict")
            && !strict.is_boolean()
        {
            return Err(Error::InvalidInput(
                "template strict must be a boolean".into(),
            ));
        }
        let report_errors = object
            .get("report_errors")
            .map(|value| {
                value
                    .as_bool()
                    .ok_or_else(|| Error::InvalidInput("report_errors must be a boolean".into()))
            })
            .transpose()?
            .unwrap_or(false);
        let explicit_entities = parse_entity_ids(object.get("entity_ids"))?;
        let listeners = template_listeners(&template, &explicit_entities);
        let engine = self.template_engine.as_ref().ok_or_else(|| {
            Error::InvalidInput("render_template is unavailable without a template engine".into())
        })?;
        let (_states, receiver) = self.runtime.subscribe_with_states();
        let initial_render = engine.render(&template, &variables);
        let (last_result, last_error, initial_events) = match initial_render {
            Ok(rendered) => (
                Some(rendered.clone()),
                None,
                vec![event_envelope(
                    id,
                    json!({ "result": rendered, "listeners": listeners }),
                )],
            ),
            Err(error) => {
                let message = error.to_string();
                let events = if report_errors {
                    vec![event_envelope(
                        id,
                        json!({ "error": message, "level": "ERROR" }),
                    )]
                } else {
                    Vec::new()
                };
                (None, Some(message), events)
            }
        };

        self.subscriptions.insert(
            id,
            ActiveSubscription::Template {
                receiver,
                template,
                variables,
                listeners,
                last_result,
                last_error,
                report_errors,
            },
        );
        Ok(success_result(initial_events))
    }

    fn ensure_capacity(&self, id: u64) -> Result<()> {
        if self.subscriptions.contains_key(&id) {
            return Err(Error::InvalidInput(format!(
                "subscription ID {id} is already active"
            )));
        }
        if self.subscriptions.len() >= MAX_SUBSCRIPTIONS {
            return Err(Error::Service(format!(
                "at most {MAX_SUBSCRIPTIONS} subscriptions are allowed per connection"
            )));
        }
        Ok(())
    }

    fn require_config_entries(&self) -> Result<&ConfigEntries> {
        self.config_entries
            .as_ref()
            .ok_or_else(|| Error::InvalidInput("config entry subscriptions are unavailable".into()))
    }

    fn forward_event(&mut self, id: u64, event: &Event, output: &mut Vec<Value>) -> Result<()> {
        let config_entries = self.config_entries.clone();
        let template_engine = self.template_engine.clone();
        let Some(subscription) = self.subscriptions.get_mut(&id) else {
            return Ok(());
        };
        match subscription {
            ActiveSubscription::Persons { .. } => {
                if event.event_type == hass_person::PERSON_COLLECTION_UPDATED_SIGNAL
                    && let Some(changes) = event.data.get("changes")
                {
                    output.push(event_envelope(id, changes.clone()));
                }
            }
            ActiveSubscription::Zones { .. } => {
                if event.event_type == hass_zone::ZONE_COLLECTION_UPDATED_SIGNAL
                    && let Some(changes) = event.data.get("changes")
                {
                    output.push(event_envelope(id, changes.clone()));
                }
            }
            ActiveSubscription::Notifications { .. } => {
                if event.event_type == hass_notifications::SIGNAL_PERSISTENT_NOTIFICATIONS_UPDATED {
                    output.push(event_envelope(id, Value::Object(event.data.clone())));
                }
            }
            ActiveSubscription::Events {
                event_type,
                allow_state_read,
                ..
            } => {
                if event.event_type == "state_changed" && !*allow_state_read {
                    return Ok(());
                }
                if event_type == EVENT_MATCH_ALL || event_type == &event.event_type {
                    let event = serde_json::to_value(event).map_err(|error| {
                        Error::InvalidInput(format!("cannot encode runtime event: {error}"))
                    })?;
                    output.push(event_envelope(id, event));
                }
            }
            ActiveSubscription::Entities {
                entity_ids, filter, ..
            } => {
                if event.event_type != "state_changed" {
                    return Ok(());
                }
                let Some(entity_id) = event.data.get("entity_id").and_then(Value::as_str) else {
                    return Ok(());
                };
                if !entity_matches(entity_id, entity_ids, filter) {
                    return Ok(());
                }
                if let Some(diff) = compressed_state_diff(event)? {
                    output.push(event_envelope(id, diff));
                }
            }
            ActiveSubscription::ConfigEntries {
                type_filter,
                visible_entries,
                ..
            } => {
                if event.event_type != "config_entry_changed" {
                    return Ok(());
                }
                let Some(change) = event.data.get("type").and_then(Value::as_str) else {
                    return Ok(());
                };
                let Some(entry) = event.data.get("entry").and_then(Value::as_object) else {
                    return Ok(());
                };
                let Some(entry_id) = entry.get("entry_id").and_then(Value::as_str) else {
                    return Ok(());
                };
                let entries = config_entries.as_ref();
                let current = entries.and_then(|entries| entries.get_entry(entry_id));
                let integration_type = current
                    .as_ref()
                    .map(|entry| entry.integration_type.clone())
                    .or_else(|| {
                        visible_entries
                            .get(entry_id)
                            .map(|(integration_type, _)| integration_type.clone())
                    })
                    .or_else(|| {
                        let domain = entry.get("domain").and_then(Value::as_str)?;
                        entries?
                            .driver(domain)
                            .map(|driver| driver.integration_type().to_owned())
                    });
                let included = type_filter.as_ref().is_none_or(|filter| {
                    integration_type
                        .as_ref()
                        .is_some_and(|integration_type| filter.contains(integration_type))
                });
                if change == "removed" {
                    let was_visible = visible_entries.contains_key(entry_id);
                    visible_entries.remove(entry_id);
                    if !was_visible || !included {
                        return Ok(());
                    }
                    output.push(event_envelope(
                        id,
                        json!([{ "type": change, "entry": Value::Object(entry.clone()) }]),
                    ));
                    return Ok(());
                }
                if !included {
                    visible_entries.remove(entry_id);
                    return Ok(());
                }
                // Prefer the current redacted entry to avoid an old queued
                // snapshot rolling a client back after the initial response.
                let public_entry = current
                    .as_ref()
                    .map(|entry| entry.public_json())
                    .unwrap_or_else(|| Value::Object(entry.clone()));
                let integration_type = integration_type.unwrap_or_default();
                if visible_entries
                    .get(entry_id)
                    .is_some_and(|(_, previous)| previous == &public_entry)
                {
                    return Ok(());
                }
                visible_entries.insert(
                    entry_id.to_owned(),
                    (integration_type, public_entry.clone()),
                );
                output.push(event_envelope(
                    id,
                    json!([{ "type": change, "entry": public_entry }]),
                ));
            }
            ActiveSubscription::ConfigFlowProgress {
                active_flow_ids, ..
            } => {
                if event.event_type != "config_flow_progress" {
                    return Ok(());
                }
                let Some(flow_id) = event.data.get("flow_id").and_then(Value::as_str) else {
                    return Ok(());
                };
                let entries = config_entries.ok_or_else(|| {
                    Error::InvalidInput("config flow subscription lost its manager".into())
                })?;
                let active = flow_progress(&entries)?
                    .into_iter()
                    .find(|flow| flow.get("flow_id").and_then(Value::as_str) == Some(flow_id));
                match (active, active_flow_ids.contains(flow_id)) {
                    (Some(progress), false) => {
                        active_flow_ids.insert(flow_id.to_owned());
                        if let Some(item) = flow_progress_event_item(&progress, Some("added")) {
                            output.push(event_envelope(id, json!([item])));
                        }
                    }
                    (None, true) => {
                        active_flow_ids.remove(flow_id);
                        output.push(event_envelope(
                            id,
                            json!([{ "type": "removed", "flow_id": flow_id }]),
                        ));
                    }
                    _ => {}
                }
            }
            ActiveSubscription::Template {
                template,
                variables,
                listeners,
                last_result,
                last_error,
                report_errors,
                ..
            } => {
                if event.event_type != "state_changed" {
                    return Ok(());
                }
                let Some(engine) = template_engine.as_ref() else {
                    return Err(Error::InvalidInput(
                        "render_template engine disappeared during subscription".into(),
                    ));
                };
                match engine.render(template, variables) {
                    Ok(rendered) => {
                        let changed = last_result.as_ref() != Some(&rendered);
                        *last_result = Some(rendered.clone());
                        *last_error = None;
                        if changed {
                            output.push(event_envelope(
                                id,
                                json!({ "result": rendered, "listeners": listeners }),
                            ));
                        }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let changed = last_error.as_ref() != Some(&message);
                        *last_error = Some(message.clone());
                        if *report_errors && changed {
                            output.push(event_envelope(
                                id,
                                json!({ "error": message, "level": "ERROR" }),
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl ActiveSubscription {
    fn try_recv(&mut self) -> std::result::Result<Event, broadcast::error::TryRecvError> {
        match self {
            Self::Events { receiver, .. }
            | Self::Notifications { receiver }
            | Self::Zones { receiver }
            | Self::Persons { receiver }
            | Self::Entities { receiver, .. }
            | Self::ConfigEntries { receiver, .. }
            | Self::ConfigFlowProgress { receiver, .. }
            | Self::Template { receiver, .. } => receiver.try_recv(),
        }
    }
}

fn success_result(events: Vec<Value>) -> SubscriptionCommandResult {
    SubscriptionCommandResult {
        result: Value::Null,
        events,
    }
}

fn event_envelope(id: u64, event: Value) -> Value {
    json!({ "id": id, "type": "event", "event": event })
}

fn command_id(object: &Map<String, Value>) -> Result<u64> {
    object
        .get("id")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| Error::InvalidInput("command ID must be a positive integer".into()))
}

fn required_string<'a>(object: &'a Map<String, Value>, field: &str) -> Result<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidInput(format!("{field} must be a string")))
}

fn optional_string<'a>(object: &'a Map<String, Value>, field: &str) -> Result<Option<&'a str>> {
    match object.get(field) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(Error::InvalidInput(format!("{field} must be a string"))),
    }
}

pub(crate) fn is_subscription_command(command_type: &str) -> bool {
    command_type.starts_with("subscribe_")
        || matches!(
            command_type,
            "unsubscribe_events" | "config_entries/flow/progress"
        )
        || command_type.ends_with("/subscribe")
        || command_type == "render_template"
}

fn parse_entity_ids(value: Option<&Value>) -> Result<Option<HashSet<String>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| Error::InvalidInput("entity_ids must be an array of strings".into()))?;
    let mut entity_ids = HashSet::new();
    for value in values {
        let entity_id = value
            .as_str()
            .ok_or_else(|| Error::InvalidInput("entity_ids must contain only strings".into()))?;
        if !valid_entity_id(entity_id) {
            return Err(Error::InvalidInput(format!(
                "invalid entity ID {entity_id:?}"
            )));
        }
        entity_ids.insert(entity_id.to_owned());
    }
    if entity_ids.is_empty() {
        Ok(None)
    } else {
        Ok(Some(entity_ids))
    }
}

fn valid_entity_id(entity_id: &str) -> bool {
    let Some((domain, object_id)) = entity_id.split_once('.') else {
        return false;
    };
    !domain.is_empty()
        && !object_id.is_empty()
        && !object_id.contains('.')
        && domain.bytes().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == b'_'
        })
        && object_id.bytes().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == b'_'
        })
}

fn parse_type_filter(value: Option<&Value>) -> Result<Option<HashSet<String>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| Error::InvalidInput("type_filter must be an array of strings".into()))?;
    let mut types = HashSet::new();
    for value in values {
        let value = value
            .as_str()
            .ok_or_else(|| Error::InvalidInput("type_filter must contain only strings".into()))?;
        types.insert(value.to_owned());
    }
    if types.is_empty() {
        Ok(None)
    } else {
        Ok(Some(types))
    }
}

fn parse_string_list(object: &Map<String, Value>, name: &str) -> Result<Vec<String>> {
    let Some(value) = object.get(name) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| Error::InvalidInput(format!("{name} must be an array of strings")))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::InvalidInput(format!("{name} must contain only strings")))
        })
        .collect()
}

impl EntityFilter {
    fn parse(object: &Map<String, Value>) -> Result<Self> {
        let include = parse_filter_group(object.get("include"), "include")?;
        let exclude = parse_filter_group(object.get("exclude"), "exclude")?;
        Ok(Self {
            include_entities: include.entities,
            deny_all: false,
            include_domains: include.domains,
            include_globs: include.globs,
            exclude_entities: exclude.entities,
            exclude_domains: exclude.domains,
            exclude_globs: exclude.globs,
        })
    }

    fn is_empty(&self) -> bool {
        self.include_entities.is_empty()
            && self.include_domains.is_empty()
            && self.include_globs.is_empty()
            && self.exclude_entities.is_empty()
            && self.exclude_domains.is_empty()
            && self.exclude_globs.is_empty()
    }

    fn matches(&self, entity_id: &str) -> bool {
        if self.deny_all {
            return false;
        }
        if self.is_empty() {
            return true;
        }
        let domain = entity_id.split_once('.').map_or("", |(domain, _)| domain);
        let include_glob_match = self
            .include_globs
            .iter()
            .any(|pattern| glob_matches(pattern, entity_id));
        let exclude_glob_match = self
            .exclude_globs
            .iter()
            .any(|pattern| glob_matches(pattern, entity_id));
        let have_include = !self.include_entities.is_empty()
            || !self.include_domains.is_empty()
            || !self.include_globs.is_empty();
        let have_exclude = !self.exclude_entities.is_empty()
            || !self.exclude_domains.is_empty()
            || !self.exclude_globs.is_empty();

        if have_include && !have_exclude {
            return self.include_entities.contains(entity_id)
                || self.include_domains.contains(domain)
                || include_glob_match;
        }
        if !have_include {
            return !self.exclude_entities.contains(entity_id)
                && !self.exclude_domains.contains(domain)
                && !exclude_glob_match;
        }
        if !self.include_domains.is_empty() || !self.include_globs.is_empty() {
            return self.include_entities.contains(entity_id)
                || (!self.exclude_entities.contains(entity_id)
                    && (include_glob_match
                        || (self.include_domains.contains(domain) && !exclude_glob_match)));
        }
        if !self.exclude_domains.is_empty() || !self.exclude_globs.is_empty() {
            if self.exclude_domains.contains(domain) || exclude_glob_match {
                return self.include_entities.contains(entity_id);
            }
            return self.include_entities.contains(entity_id)
                || !self.exclude_entities.contains(entity_id);
        }
        self.include_entities.contains(entity_id)
    }
}

struct FilterGroup {
    entities: HashSet<String>,
    domains: HashSet<String>,
    globs: Vec<String>,
}

fn parse_filter_group(value: Option<&Value>, name: &str) -> Result<FilterGroup> {
    let mut group = FilterGroup {
        entities: HashSet::new(),
        domains: HashSet::new(),
        globs: Vec::new(),
    };
    let Some(value) = value else {
        return Ok(group);
    };
    let object = value
        .as_object()
        .ok_or_else(|| Error::InvalidInput(format!("{name} must be an object")))?;
    for entity in parse_string_list(object, "entities")? {
        if !valid_entity_id(&entity) {
            return Err(Error::InvalidInput(format!("invalid entity ID {entity:?}")));
        }
        group.entities.insert(entity);
    }
    group.domains.extend(parse_string_list(object, "domains")?);
    group.globs = parse_string_list(object, "entity_globs")?;
    Ok(group)
}

fn entity_matches(
    entity_id: &str,
    entity_ids: &Option<HashSet<String>>,
    filter: &EntityFilter,
) -> bool {
    entity_ids
        .as_ref()
        .is_none_or(|entity_ids| entity_ids.contains(entity_id))
        && filter.matches(entity_id)
}

/// Small `fnmatch`-style matcher for the `*`, `?`, and character classes
/// accepted by Home Assistant's include/exclude filter schema.
fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut memo = HashMap::new();
    glob_matches_at(pattern, value, 0, 0, &mut memo)
}

fn glob_matches_at(
    pattern: &[u8],
    value: &[u8],
    pattern_index: usize,
    value_index: usize,
    memo: &mut HashMap<(usize, usize), bool>,
) -> bool {
    if let Some(result) = memo.get(&(pattern_index, value_index)) {
        return *result;
    }
    let matched = if pattern_index == pattern.len() {
        value_index == value.len()
    } else if pattern[pattern_index] == b'*' {
        glob_matches_at(pattern, value, pattern_index + 1, value_index, memo)
            || (value_index < value.len()
                && glob_matches_at(pattern, value, pattern_index, value_index + 1, memo))
    } else if pattern[pattern_index] == b'?' {
        value_index < value.len()
            && glob_matches_at(pattern, value, pattern_index + 1, value_index + 1, memo)
    } else if pattern[pattern_index] == b'[' {
        match_class(pattern, value.get(value_index).copied()).is_some_and(|(next, matches)| {
            matches && glob_matches_at(pattern, value, next, value_index + 1, memo)
        })
    } else {
        value.get(value_index) == Some(&pattern[pattern_index])
            && glob_matches_at(pattern, value, pattern_index + 1, value_index + 1, memo)
    };
    memo.insert((pattern_index, value_index), matched);
    matched
}

fn match_class(pattern: &[u8], value: Option<u8>) -> Option<(usize, bool)> {
    let value = value?;
    let mut index = 1;
    let negate = pattern
        .get(index)
        .is_some_and(|byte| *byte == b'!' || *byte == b'^');
    if negate {
        index += 1;
    }
    let mut matched = false;
    let mut saw_value = false;
    while index < pattern.len() && pattern[index] != b']' {
        let start = pattern[index];
        if pattern.get(index + 1) == Some(&b'-')
            && pattern.get(index + 2).is_some_and(|byte| *byte != b']')
        {
            let end = pattern[index + 2];
            matched |= (start..=end).contains(&value);
            index += 3;
        } else {
            matched |= value == start;
            index += 1;
        }
        saw_value = true;
    }
    if index >= pattern.len() || !saw_value {
        return None;
    }
    Some((index + 1, if negate { !matched } else { matched }))
}

fn compressed_state(state: &State) -> Value {
    let context = if state.context.parent_id.is_none() && state.context.user_id.is_none() {
        Value::String(state.context.id.clone())
    } else {
        json!({
            "id": state.context.id,
            "parent_id": state.context.parent_id,
            "user_id": state.context.user_id,
        })
    };
    let last_changed = timestamp(state.last_changed);
    let mut compressed = json!({
        "s": state.state,
        "a": state.attributes,
        "c": context,
        "lc": last_changed,
    });
    let last_updated = timestamp(state.last_updated);
    if last_changed != last_updated {
        compressed["lu"] = json!(last_updated);
    }
    compressed
}

fn compressed_state_diff(event: &Event) -> Result<Option<Value>> {
    let entity_id = event
        .data
        .get("entity_id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidInput("state_changed event has no entity_id".into()))?;
    let old_state = parse_state(event.data.get("old_state"))?;
    let new_state = parse_state(event.data.get("new_state"))?;
    let Some(new_state) = new_state else {
        return Ok(Some(json!({ "r": [entity_id] })));
    };
    let Some(old_state) = old_state else {
        return Ok(Some(json!({
            "a": { (new_state.entity_id.clone()): compressed_state(&new_state) }
        })));
    };

    let mut additions = Map::new();
    let mut diff = Map::new();
    if old_state.state != new_state.state {
        additions.insert("s".into(), json!(new_state.state));
    }
    let old_changed = timestamp(old_state.last_changed);
    let new_changed = timestamp(new_state.last_changed);
    let old_updated = timestamp(old_state.last_updated);
    let new_updated = timestamp(new_state.last_updated);
    if old_changed != new_changed {
        additions.insert("lc".into(), json!(new_changed));
    } else if old_updated != new_updated {
        additions.insert("lu".into(), json!(new_updated));
    }
    if old_state.context.parent_id != new_state.context.parent_id {
        additions.insert(
            "c".into(),
            json!({ "parent_id": new_state.context.parent_id }),
        );
    }
    if old_state.context.user_id != new_state.context.user_id {
        if let Some(context) = additions.get_mut("c").and_then(Value::as_object_mut) {
            context.insert("user_id".into(), json!(new_state.context.user_id));
        } else {
            additions.insert("c".into(), json!({ "user_id": new_state.context.user_id }));
        }
    }
    if old_state.context.id != new_state.context.id {
        if let Some(context) = additions.get_mut("c").and_then(Value::as_object_mut) {
            context.insert("id".into(), json!(new_state.context.id));
        } else {
            additions.insert("c".into(), json!(new_state.context.id));
        }
    }
    if old_state.attributes != new_state.attributes {
        let changed_attributes = new_state
            .attributes
            .iter()
            .filter(|(key, value)| old_state.attributes.get(*key) != Some(*value))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Map<_, _>>();
        if !changed_attributes.is_empty() {
            additions.insert("a".into(), Value::Object(changed_attributes));
        }
        let removed_attributes = old_state
            .attributes
            .keys()
            .filter(|key| !new_state.attributes.contains_key(*key))
            .cloned()
            .map(Value::String)
            .collect::<Vec<_>>();
        if !removed_attributes.is_empty() {
            diff.insert("-".into(), json!({ "a": removed_attributes }));
        }
    }
    diff.insert("+".into(), Value::Object(additions));
    Ok(Some(
        json!({ "c": { (new_state.entity_id.clone()): diff } }),
    ))
}

fn parse_state(value: Option<&Value>) -> Result<Option<State>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| Error::InvalidInput(format!("invalid state_changed state: {error}"))),
    }
}

fn timestamp(value: chrono::DateTime<chrono::Utc>) -> f64 {
    value.timestamp() as f64 + value.timestamp_subsec_nanos() as f64 / 1_000_000_000.0
}

fn flow_progress(entries: &ConfigEntries) -> Result<Vec<Value>> {
    let result = entries.handle_ws(&json!({ "type": "config_entries/flow/progress" }))?;
    result
        .as_array()
        .cloned()
        .ok_or_else(|| Error::InvalidInput("flow progress API did not return an array".into()))
}

fn config_flow_progress(entries: &ConfigEntries) -> Result<Vec<Value>> {
    Ok(flow_progress(entries)?
        .into_iter()
        .filter(|flow| flow.get("type").and_then(Value::as_str) == Some("config_flow"))
        .collect())
}

fn flow_progress_event_item(progress: &Value, change: Option<&str>) -> Option<Value> {
    let flow_id = progress.get("flow_id")?.as_str()?;
    let handler = progress.get("handler")?.as_str()?;
    let step_id = progress.get("step_id")?.as_str()?;
    let context = progress.get("context")?.clone();
    Some(json!({
        "type": change,
        "flow_id": flow_id,
        "flow": {
            "flow_id": flow_id,
            "handler": handler,
            "step_id": step_id,
            "context": context,
        }
    }))
}

fn template_listeners(template: &str, explicit_entity_ids: &Option<HashSet<String>>) -> Value {
    let mut entities = explicit_entity_ids
        .as_ref()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut domains = BTreeSet::new();
    let call_names = [
        "states",
        "is_state",
        "state_attr",
        "is_state_attr",
        "has_value",
    ];
    let bytes = template.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        let remaining = &template[offset..];
        let Some((_, name_len)) = call_names
            .iter()
            .find_map(|name| remaining.starts_with(name).then_some((*name, name.len())))
        else {
            offset += template[offset..].chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let mut cursor = offset + name_len;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'(') {
            offset += name_len;
            continue;
        }
        cursor += 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        let Some(quote @ (b'\'' | b'"')) = bytes.get(cursor).copied() else {
            offset = cursor;
            continue;
        };
        cursor += 1;
        let start = cursor;
        while cursor < bytes.len() && bytes[cursor] != quote {
            cursor += 1;
        }
        if cursor < bytes.len() {
            if let Ok(entity_id) = std::str::from_utf8(&bytes[start..cursor])
                && valid_entity_id(entity_id)
            {
                entities.insert(entity_id.to_owned());
            }
            offset = cursor + 1;
        } else {
            break;
        }
    }

    // Also recognize Home Assistant's `states.domain.entity.state` shorthand.
    let mut search_from = 0;
    while let Some(relative) = template[search_from..].find("states.") {
        let start = search_from + relative + "states.".len();
        let mut cursor = start;
        let mut parts = Vec::new();
        while cursor < bytes.len() {
            let part_start = cursor;
            while bytes
                .get(cursor)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
            {
                cursor += 1;
            }
            if cursor == part_start {
                break;
            }
            parts.push(&template[part_start..cursor]);
            if bytes.get(cursor) != Some(&b'.') {
                break;
            }
            cursor += 1;
        }
        if parts.len() >= 2 {
            let entity_id = format!("{}.{}", parts[0], parts[1]);
            if valid_entity_id(&entity_id) {
                entities.insert(entity_id);
            }
        } else if let Some(domain) = parts.first() {
            domains.insert((*domain).to_owned());
        }
        search_from = cursor.max(start);
    }

    let all = template.contains("states") && entities.is_empty() && domains.is_empty();
    let time = template.contains("now()") || template.contains("utcnow()");
    json!({
        "all": all,
        "domains": domains.into_iter().collect::<Vec<_>>(),
        "entities": entities.into_iter().collect::<Vec<_>>(),
        "time": time,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hass_config::{
        ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult,
        NewConfigEntry, SetupOutcome,
    };
    use hass_core::{Context, MemoryStore, Runtime};

    use super::*;

    fn runtime() -> Runtime {
        Runtime::new(Arc::new(MemoryStore::default())).unwrap()
    }

    fn set_state(runtime: &Runtime, entity_id: &str, state: &str, attributes: Map<String, Value>) {
        runtime
            .set_state(entity_id, state, attributes, Context::default(), false)
            .unwrap();
    }

    #[test]
    fn subscribe_events_forwards_public_events_but_never_private_signals() {
        let runtime = runtime();
        let mut subscriptions = NativeSubscriptions::new(runtime.clone(), None, None);
        subscriptions
            .handle_command(&json!({"id": 1, "type": "subscribe_events", "event_type": "*"}))
            .unwrap();

        runtime
            .dispatch_signal(
                "mqtt_message",
                json!({"payload": "sensitive"}).as_object().unwrap().clone(),
                Context::default(),
            )
            .unwrap();
        assert!(subscriptions.drain_events().unwrap().is_empty());

        runtime
            .fire_event("custom_event", Map::new(), Context::default())
            .unwrap();
        let events = subscriptions.drain_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"]["event_type"], "custom_event");
        assert!(!events[0].to_string().contains("sensitive"));
    }

    #[test]
    fn subscribe_entities_emits_compressed_initial_add_change_and_remove() {
        let runtime = runtime();
        set_state(&runtime, "sensor.room", "warm", Map::new());
        let mut subscriptions = NativeSubscriptions::new(runtime.clone(), None, None);
        let response = subscriptions
            .handle_command(&json!({"id": 7, "type": "subscribe_entities"}))
            .unwrap()
            .unwrap();
        assert_eq!(response.result, Value::Null);
        assert_eq!(response.events[0]["event"]["a"]["sensor.room"]["s"], "warm");
        assert_eq!(response.events[0]["id"], 7);

        let mut changed_attributes = Map::new();
        changed_attributes.insert("unit_of_measurement".into(), json!("C"));
        set_state(&runtime, "sensor.room", "hot", changed_attributes);
        let changed = subscriptions.drain_events().unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0]["event"]["c"]["sensor.room"]["+"]["s"], "hot");
        assert_eq!(
            changed[0]["event"]["c"]["sensor.room"]["+"]["a"]["unit_of_measurement"],
            "C"
        );

        runtime
            .remove_state("sensor.room", Context::default())
            .unwrap();
        let removed = subscriptions.drain_events().unwrap();
        assert_eq!(removed[0]["event"]["r"], json!(["sensor.room"]));
    }

    #[test]
    fn user_without_read_policy_receives_no_initial_or_live_state_data() {
        let runtime = runtime();
        set_state(&runtime, "sensor.private", "private-value", Map::new());
        let mut subscriptions = NativeSubscriptions::new(runtime.clone(), None, None);
        subscriptions.set_state_read_access(false);
        let initial = subscriptions
            .handle_command(&json!({"id":1,"type":"subscribe_entities"}))
            .unwrap()
            .unwrap();
        assert!(
            !serde_json::to_string(&initial.events)
                .unwrap()
                .contains("sensor.private")
        );
        subscriptions
            .handle_command(&json!({"id":2,"type":"subscribe_events","event_type":"state_changed"}))
            .unwrap();
        set_state(&runtime, "sensor.private", "new-private-value", Map::new());
        runtime
            .remove_state("sensor.private", Context::default())
            .unwrap();
        assert!(subscriptions.drain_events().unwrap().is_empty());
    }

    #[test]
    fn subscription_filters_capacity_unsubscribe_and_lag_cleanup_are_enforced() {
        let runtime = runtime();
        let mut subscriptions = NativeSubscriptions::new(runtime.clone(), None, None);
        for id in 1..=MAX_SUBSCRIPTIONS as u64 {
            subscriptions
                .handle_command(&json!({"id": id, "type": "subscribe_events"}))
                .unwrap();
        }
        assert!(
            subscriptions
                .handle_command(&json!({"id": 40, "type": "subscribe_events"}))
                .is_err()
        );
        subscriptions
            .handle_command(&json!({"id": 41, "type": "unsubscribe_events", "subscription": 1}))
            .unwrap();
        assert_eq!(subscriptions.len(), MAX_SUBSCRIPTIONS - 1);
        assert!(
            subscriptions
                .handle_command(
                    &json!({"id": 42, "type": "unsubscribe_events", "subscription": 999})
                )
                .is_err()
        );

        for index in 0..=1024 {
            runtime
                .fire_event(&format!("test_{index}"), Map::new(), Context::default())
                .unwrap();
        }
        assert!(subscriptions.drain_events().is_err());
        assert!(subscriptions.is_empty());
    }

    #[test]
    fn template_subscription_renders_initial_result_and_state_updates() {
        let runtime = runtime();
        set_state(&runtime, "sensor.temp", "18", Map::new());
        let engine = TemplateEngine::new(runtime.clone());
        let mut subscriptions = NativeSubscriptions::new(runtime.clone(), None, Some(engine));
        let response = subscriptions
            .handle_command(&json!({
                "id": 4,
                "type": "render_template",
                "template": "State is: {{ states('sensor.temp') }}"
            }))
            .unwrap()
            .unwrap();
        assert_eq!(response.events[0]["event"]["result"], "State is: 18");
        assert_eq!(
            response.events[0]["event"]["listeners"]["entities"],
            json!(["sensor.temp"])
        );

        set_state(&runtime, "sensor.temp", "19", Map::new());
        let updated = subscriptions.drain_events().unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0]["event"]["result"], "State is: 19");
    }

    struct FlowDriver;

    impl ConfigEntryDriver for FlowDriver {
        fn domain(&self) -> &str {
            "fixture"
        }

        fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<SetupOutcome> {
            Ok(SetupOutcome::Ready)
        }

        fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<()> {
            Ok(())
        }

        fn config_flow(&self, _request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
            Ok(ConfigFlowResult::Form {
                step_id: "pairing".into(),
                data_schema: None,
                errors: None,
                description_placeholders: None,
            })
        }
    }

    #[test]
    fn config_entry_subscription_redacts_private_fields_and_tracks_removal() {
        let runtime = runtime();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        entries.register_driver(Arc::new(FlowDriver)).unwrap();
        let mut subscriptions =
            NativeSubscriptions::new(runtime.clone(), Some(entries.clone()), None);
        let response = subscriptions
            .handle_command(&json!({"id": 2, "type": "config_entries/subscribe"}))
            .unwrap()
            .unwrap();
        assert_eq!(response.events[0]["event"], json!([]));

        let mut data = Map::new();
        data.insert("token".into(), json!("private-token"));
        let entry = entries
            .create_entry(NewConfigEntry::new("fixture", "Fixture", data))
            .unwrap();
        let added = subscriptions.drain_events().unwrap();
        assert_eq!(added[0]["event"][0]["type"], "added");
        assert_eq!(added[0]["event"][0]["entry"]["entry_id"], entry.entry_id);
        assert!(!added[0].to_string().contains("private-token"));

        entries.remove_entry(&entry.entry_id).unwrap();
        let removed = subscriptions.drain_events().unwrap();
        assert_eq!(removed[0]["event"][0]["type"], "removed");
        assert!(!removed[0].to_string().contains("private-token"));
    }

    #[test]
    fn config_flow_subscription_emits_progress_add_and_remove() {
        let runtime = runtime();
        let entries = ConfigEntries::new(runtime.clone()).unwrap();
        entries.register_driver(Arc::new(FlowDriver)).unwrap();
        let mut subscriptions =
            NativeSubscriptions::new(runtime.clone(), Some(entries.clone()), None);
        let response = subscriptions
            .handle_command(&json!({"id": 3, "type": "config_entries/flow/subscribe"}))
            .unwrap()
            .unwrap();
        assert_eq!(response.events[0]["event"], json!([]));

        let started = entries
            .init_config_flow("fixture", "dhcp", Map::new())
            .unwrap();
        let flow_id = started["flow_id"].as_str().unwrap();
        let added = subscriptions.drain_events().unwrap();
        assert_eq!(added[0]["event"][0]["type"], "added");
        assert_eq!(added[0]["event"][0]["flow_id"], flow_id);
        assert_eq!(added[0]["event"][0]["flow"]["step_id"], "pairing");

        entries.abort_flow(flow_id).unwrap();
        let removed = subscriptions.drain_events().unwrap();
        assert_eq!(
            removed[0]["event"][0],
            json!({ "type": "removed", "flow_id": flow_id })
        );
    }

    #[test]
    fn subscribe_entities_applies_pinned_include_exclude_filters() {
        let runtime = runtime();
        set_state(&runtime, "light.kitchen", "on", Map::new());
        set_state(&runtime, "light.porch", "off", Map::new());
        set_state(&runtime, "sensor.temp", "21", Map::new());
        let mut subscriptions = NativeSubscriptions::new(runtime, None, None);
        let response = subscriptions
            .handle_command(&json!({
                "id": 8,
                "type": "subscribe_entities",
                "include": {"domains": ["light"]},
                "exclude": {"entities": ["light.porch"]}
            }))
            .unwrap()
            .unwrap();
        assert!(
            response.events[0]["event"]["a"]
                .get("light.kitchen")
                .is_some()
        );
        assert!(
            response.events[0]["event"]["a"]
                .get("light.porch")
                .is_none()
        );
        assert!(
            response.events[0]["event"]["a"]
                .get("sensor.temp")
                .is_none()
        );
    }
}
