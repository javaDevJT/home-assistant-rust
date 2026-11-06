//! Native in-process helper entities.
//!
//! This is an intentionally strict subset of Home Assistant's helper YAML. For
//! example:
//!
//! ```yaml
//! input_boolean:
//!   guest_mode:
//!     name: Guest mode
//!     icon: mdi:account-group
//!     initial: false
//! input_number:
//!   target_temperature:
//!     name: Target temperature
//!     min: 15
//!     max: 30
//!     step: 0.5
//!     initial: 21
//!     unit_of_measurement: "°C"
//!     mode: box
//! input_text:
//!   welcome_message:
//!     initial: Hello
//!     min: 0
//!     max: 80
//! input_select:
//!   house_mode:
//!     options: [home, away, vacation]
//!     initial: home
//! counter:
//!   arrivals:
//!     initial: 0
//!     step: 1
//!     minimum: 0
//!     maximum: 500
//! ```
//!
//! Supported service data is deliberately small: `value` for set services,
//! `option` for `input_select.select_option`, and optional `cycle` for select
//! next/previous. Every service call needs explicit `entity_id` targets. These
//! helpers only store state in the Rust runtime; they do not control devices.

use hass_core::{Context, Error, Result, Runtime, ServiceCall, ServiceHandler, ServiceResult};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

/// All configured native helpers, keyed by object ID in each domain.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Helpers {
    #[serde(default)]
    pub input_boolean: BTreeMap<String, InputBooleanConfig>,
    #[serde(default)]
    pub input_number: BTreeMap<String, InputNumberConfig>,
    #[serde(default)]
    pub input_text: BTreeMap<String, InputTextConfig>,
    #[serde(default)]
    pub input_select: BTreeMap<String, InputSelectConfig>,
    #[serde(default)]
    pub counter: BTreeMap<String, CounterConfig>,
}

/// Configuration for one `input_boolean` helper.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputBooleanConfig {
    pub name: Option<String>,
    pub icon: Option<String>,
    pub initial: Option<bool>,
}

/// `slider` and `box` are the supported input number presentation modes.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NumberMode {
    #[default]
    Slider,
    Box,
}

/// Configuration for one `input_number` helper.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputNumberConfig {
    pub name: Option<String>,
    pub icon: Option<String>,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub initial: Option<f64>,
    pub unit_of_measurement: Option<String>,
    pub mode: NumberMode,
}

impl Default for InputNumberConfig {
    fn default() -> Self {
        Self {
            name: None,
            icon: None,
            min: 0.0,
            max: 100.0,
            step: 1.0,
            initial: None,
            unit_of_measurement: None,
            mode: NumberMode::Slider,
        }
    }
}

/// `text` and `password` are the supported input text presentation modes.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TextMode {
    #[default]
    Text,
    Password,
}

/// Configuration for one `input_text` helper.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputTextConfig {
    pub name: Option<String>,
    pub icon: Option<String>,
    pub initial: Option<String>,
    pub min: usize,
    pub max: usize,
    pub unit_of_measurement: Option<String>,
    pub mode: TextMode,
}

impl Default for InputTextConfig {
    fn default() -> Self {
        Self {
            name: None,
            icon: None,
            initial: None,
            min: 0,
            max: 100,
            unit_of_measurement: None,
            mode: TextMode::Text,
        }
    }
}

/// Configuration for one `input_select` helper.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputSelectConfig {
    pub name: Option<String>,
    pub icon: Option<String>,
    pub options: Vec<String>,
    pub initial: Option<String>,
}

/// Configuration for one `counter` helper.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CounterConfig {
    pub name: Option<String>,
    pub icon: Option<String>,
    pub initial: i64,
    pub step: i64,
    pub minimum: Option<i64>,
    pub maximum: Option<i64>,
}

impl Default for CounterConfig {
    fn default() -> Self {
        Self {
            name: None,
            icon: None,
            initial: 0,
            step: 1,
            minimum: None,
            maximum: None,
        }
    }
}

impl Helpers {
    /// Validate object IDs and every supported per-helper option.
    pub fn validate(&self) -> Result<()> {
        for (domain, helpers) in [
            (
                "input_boolean",
                self.input_boolean.keys().collect::<Vec<_>>(),
            ),
            ("input_number", self.input_number.keys().collect()),
            ("input_text", self.input_text.keys().collect()),
            ("input_select", self.input_select.keys().collect()),
            ("counter", self.counter.keys().collect()),
        ] {
            for object_id in helpers {
                validate_object_id(domain, object_id)?;
            }
        }

        for (object_id, config) in &self.input_boolean {
            validate_metadata("input_boolean", object_id, &config.name, &config.icon, None)?;
        }
        for (object_id, config) in &self.input_number {
            validate_metadata(
                "input_number",
                object_id,
                &config.name,
                &config.icon,
                config.unit_of_measurement.as_deref(),
            )?;
            if !config.min.is_finite()
                || !config.max.is_finite()
                || !config.step.is_finite()
                || config.min >= config.max
                || config.step <= 0.0
            {
                return invalid(format!(
                    "input_number.{object_id} requires finite min < max and a finite positive step"
                ));
            }
            if let Some(initial) = config.initial
                && (!initial.is_finite() || initial < config.min || initial > config.max)
            {
                return invalid(format!(
                    "input_number.{object_id}.initial must be finite and within min..=max"
                ));
            }
        }
        for (object_id, config) in &self.input_text {
            validate_metadata(
                "input_text",
                object_id,
                &config.name,
                &config.icon,
                config.unit_of_measurement.as_deref(),
            )?;
            if config.min > config.max || config.max > 255 {
                return invalid(format!(
                    "input_text.{object_id} requires 0 <= min <= max <= 255"
                ));
            }
            if let Some(initial) = &config.initial
                && !text_length_valid(initial, config.min, config.max)
            {
                return invalid(format!(
                    "input_text.{object_id}.initial length must be within min..=max"
                ));
            }
            if config.initial.is_none() && !text_length_valid("", config.min, config.max) {
                return invalid(format!(
                    "input_text.{object_id} requires initial when the empty default is outside min..=max"
                ));
            }
        }
        for (object_id, config) in &self.input_select {
            validate_metadata("input_select", object_id, &config.name, &config.icon, None)?;
            if config.options.is_empty() {
                return invalid(format!(
                    "input_select.{object_id}.options must not be empty"
                ));
            }
            let mut unique = HashSet::with_capacity(config.options.len());
            for option in &config.options {
                if option.trim().is_empty() || option.chars().count() > 255 {
                    return invalid(format!(
                        "input_select.{object_id} options must be nonempty and at most 255 characters"
                    ));
                }
                if !unique.insert(option) {
                    return invalid(format!(
                        "input_select.{object_id}.options must contain unique values"
                    ));
                }
            }
            if let Some(initial) = &config.initial
                && !config.options.contains(initial)
            {
                return invalid(format!(
                    "input_select.{object_id}.initial must match one configured option"
                ));
            }
        }
        for (object_id, config) in &self.counter {
            validate_metadata("counter", object_id, &config.name, &config.icon, None)?;
            if config.step <= 0
                || config
                    .minimum
                    .is_some_and(|minimum| config.initial < minimum)
                || config
                    .maximum
                    .is_some_and(|maximum| config.initial > maximum)
                || config
                    .minimum
                    .zip(config.maximum)
                    .is_some_and(|(min, max)| min > max)
            {
                return invalid(format!(
                    "counter.{object_id} requires a positive step and an initial value within its optional bounds"
                ));
            }
        }
        Ok(())
    }
}

/// Initialize configured helpers without replacing any persisted value, then
/// register their in-process services.
pub fn setup(runtime: &Runtime, helpers: &Helpers) -> Result<()> {
    helpers.validate()?;
    let registry = Arc::new(build_registry(helpers));

    // Preflight every restored value before changing any entity. The runtime
    // API has per-state atomic writes but no multi-state transaction.
    let mut init = Vec::with_capacity(registry.len());
    for (entity_id, helper) in registry.iter() {
        let state = match runtime.get_state(entity_id) {
            Some(current) => {
                validate_state(helper, &current.state)?;
                current.state
            }
            None => initial_state(helper),
        };
        init.push((entity_id.clone(), state, attributes(helper, entity_id)));
    }
    for (entity_id, state, attributes) in init {
        runtime.set_state(&entity_id, &state, attributes, Context::default(), false)?;
    }

    let lock = Arc::new(Mutex::new(()));
    if !helpers.input_boolean.is_empty() {
        for (service, action) in [
            ("turn_on", Action::BooleanOn),
            ("turn_off", Action::BooleanOff),
            ("toggle", Action::BooleanToggle),
        ] {
            register(
                runtime,
                "input_boolean",
                service,
                action,
                "input_boolean",
                Arc::clone(&registry),
                Arc::clone(&lock),
            )?;
        }
        // Generic services operate only on known input_boolean helpers and use
        // the same mutation path directly, without recursive call_service.
        for (service, action) in [
            ("turn_on", Action::BooleanOn),
            ("turn_off", Action::BooleanOff),
            ("toggle", Action::BooleanToggle),
        ] {
            register(
                runtime,
                "homeassistant",
                service,
                action,
                "input_boolean",
                Arc::clone(&registry),
                Arc::clone(&lock),
            )?;
        }
    }
    if !helpers.input_number.is_empty() {
        for (service, action) in [
            ("set_value", Action::NumberSet),
            ("increment", Action::NumberIncrement),
            ("decrement", Action::NumberDecrement),
        ] {
            register(
                runtime,
                "input_number",
                service,
                action,
                "input_number",
                Arc::clone(&registry),
                Arc::clone(&lock),
            )?;
        }
    }
    if !helpers.input_text.is_empty() {
        register(
            runtime,
            "input_text",
            "set_value",
            Action::TextSet,
            "input_text",
            Arc::clone(&registry),
            Arc::clone(&lock),
        )?;
    }
    if !helpers.input_select.is_empty() {
        for (service, action) in [
            ("select_option", Action::SelectOption),
            ("select_next", Action::SelectNext),
            ("select_previous", Action::SelectPrevious),
        ] {
            register(
                runtime,
                "input_select",
                service,
                action,
                "input_select",
                Arc::clone(&registry),
                Arc::clone(&lock),
            )?;
        }
    }
    if !helpers.counter.is_empty() {
        for (service, action) in [
            ("increment", Action::CounterIncrement),
            ("decrement", Action::CounterDecrement),
            ("reset", Action::CounterReset),
            ("set_value", Action::CounterSet),
        ] {
            register(
                runtime,
                "counter",
                service,
                action,
                "counter",
                Arc::clone(&registry),
                Arc::clone(&lock),
            )?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
enum Helper {
    InputBoolean(InputBooleanConfig),
    InputNumber(InputNumberConfig),
    InputText(InputTextConfig),
    InputSelect(InputSelectConfig),
    Counter(CounterConfig),
}

fn build_registry(helpers: &Helpers) -> BTreeMap<String, Helper> {
    let mut result = BTreeMap::new();
    result.extend(helpers.input_boolean.iter().map(|(id, cfg)| {
        (
            format!("input_boolean.{id}"),
            Helper::InputBoolean(cfg.clone()),
        )
    }));
    result.extend(helpers.input_number.iter().map(|(id, cfg)| {
        (
            format!("input_number.{id}"),
            Helper::InputNumber(cfg.clone()),
        )
    }));
    result.extend(
        helpers
            .input_text
            .iter()
            .map(|(id, cfg)| (format!("input_text.{id}"), Helper::InputText(cfg.clone()))),
    );
    result.extend(helpers.input_select.iter().map(|(id, cfg)| {
        (
            format!("input_select.{id}"),
            Helper::InputSelect(cfg.clone()),
        )
    }));
    result.extend(
        helpers
            .counter
            .iter()
            .map(|(id, cfg)| (format!("counter.{id}"), Helper::Counter(cfg.clone()))),
    );
    result
}

fn register(
    runtime: &Runtime,
    domain: &'static str,
    service: &'static str,
    action: Action,
    helper_domain: &'static str,
    registry: Arc<BTreeMap<String, Helper>>,
    lock: Arc<Mutex<()>>,
) -> Result<()> {
    let description =
        json!({ "description": format!("Apply {service} to configured {helper_domain} helpers") });
    let handler: ServiceHandler = Arc::new(move |runtime, call| {
        execute_service(
            runtime,
            call,
            (domain, service),
            helper_domain,
            action,
            &registry,
            &lock,
        )
    });
    runtime.register_service(domain, service, description, handler)
}

#[derive(Clone, Copy, Debug)]
enum Action {
    BooleanOn,
    BooleanOff,
    BooleanToggle,
    NumberSet,
    NumberIncrement,
    NumberDecrement,
    TextSet,
    SelectOption,
    SelectNext,
    SelectPrevious,
    CounterIncrement,
    CounterDecrement,
    CounterReset,
    CounterSet,
}

#[derive(Clone, Debug)]
enum Operation {
    Boolean(bool),
    Toggle,
    NumberSet(f64),
    NumberIncrement,
    NumberDecrement,
    TextSet(String),
    SelectOption(String),
    SelectNext { previous: bool, cycle: bool },
    CounterIncrement,
    CounterDecrement,
    CounterReset,
    CounterSet(i64),
}

fn execute_service(
    runtime: &Runtime,
    call: &ServiceCall,
    (expected_domain, expected_service): (&'static str, &'static str),
    helper_domain: &'static str,
    action: Action,
    registry: &BTreeMap<String, Helper>,
    lock: &Mutex<()>,
) -> Result<ServiceResult> {
    if call.domain != expected_domain || call.service != expected_service {
        return invalid("helper service handler received a mismatched service call");
    }
    let (targets, payload) = parse_targets(call, registry)?;
    let operation = parse_operation(action, &payload)?;
    let _guard = lock
        .lock()
        .map_err(|_| Error::Service("helper service lock was poisoned".into()))?;

    // Compute and validate every outcome while holding the shared lock, then
    // write. A bad value or target in a batch cannot partially mutate helpers.
    let mut plan = Vec::with_capacity(targets.len());
    for entity_id in &targets {
        let helper = registry
            .get(entity_id)
            .ok_or_else(|| Error::NotFound(format!("unknown helper entity {entity_id}")))?;
        let domain = entity_id
            .split_once('.')
            .map(|(domain, _)| domain)
            .unwrap_or("");
        if domain != helper_domain {
            return invalid(format!(
                "service {}.{} only accepts {helper_domain} helper entities",
                call.domain, call.service
            ));
        }
        let state = runtime
            .get_state(entity_id)
            .ok_or_else(|| Error::NotFound(format!("helper entity {entity_id} has no state")))?;
        let next = apply_operation(helper, &state.state, &operation, entity_id)?;
        plan.push((entity_id.clone(), next, attributes(helper, entity_id)));
    }

    let mut changed_states = Vec::new();
    for (entity_id, state, attributes) in plan {
        let result =
            runtime.set_state(&entity_id, &state, attributes, call.context.clone(), false)?;
        if result.created || result.changed {
            changed_states.push(result.state);
        }
    }
    Ok(ServiceResult {
        changed_states,
        response: None,
        context: call.context.clone(),
    })
}

fn parse_targets(
    call: &ServiceCall,
    registry: &BTreeMap<String, Helper>,
) -> Result<(Vec<String>, Map<String, Value>)> {
    for key in call.target.keys() {
        if key != "entity_id" {
            return invalid(format!(
                "helper services do not support target field '{key}'"
            ));
        }
    }
    let target_entity = call.target.get("entity_id");
    let data_entity = call.data.get("entity_id");
    if target_entity.is_some() && data_entity.is_some() {
        return invalid("specify helper entity_id in target or service data, not both");
    }
    let entities = target_entity.or(data_entity).ok_or_else(|| {
        Error::InvalidInput("helper services require explicit entity_id targets".into())
    })?;
    let targets = match entities {
        Value::String(entity_id) => vec![entity_id.clone()],
        Value::Array(items) => {
            if items.is_empty() {
                return invalid("helper entity_id target list must not be empty");
            }
            let mut ids = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(entity_id) => ids.push(entity_id.clone()),
                    _ => return invalid("helper entity_id lists must contain only strings"),
                }
            }
            ids
        }
        _ => return invalid("helper entity_id must be a string or list of strings"),
    };
    if targets.is_empty() || targets.iter().any(|id| id.trim().is_empty()) {
        return invalid("helper entity_id targets must not be empty");
    }
    let mut unique = HashSet::with_capacity(targets.len());
    for entity_id in &targets {
        if !unique.insert(entity_id) {
            return invalid(format!("duplicate helper target {entity_id}"));
        }
        if !registry.contains_key(entity_id) {
            return invalid(format!("unknown configured helper entity {entity_id}"));
        }
    }
    let mut payload = call.data.clone();
    payload.remove("entity_id");
    Ok((targets, payload))
}

fn parse_operation(action: Action, data: &Map<String, Value>) -> Result<Operation> {
    match action {
        Action::BooleanOn => {
            require_no_data(data)?;
            Ok(Operation::Boolean(true))
        }
        Action::BooleanOff => {
            require_no_data(data)?;
            Ok(Operation::Boolean(false))
        }
        Action::BooleanToggle => {
            require_no_data(data)?;
            Ok(Operation::Toggle)
        }
        Action::NumberSet => {
            only_data_fields(data, &["value"])?;
            let value = data
                .get("value")
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite())
                .ok_or_else(|| {
                    Error::InvalidInput(
                        "input_number.set_value requires a finite numeric value".into(),
                    )
                })?;
            Ok(Operation::NumberSet(value))
        }
        Action::NumberIncrement => {
            require_no_data(data)?;
            Ok(Operation::NumberIncrement)
        }
        Action::NumberDecrement => {
            require_no_data(data)?;
            Ok(Operation::NumberDecrement)
        }
        Action::TextSet => {
            only_data_fields(data, &["value"])?;
            let value = data.get("value").and_then(Value::as_str).ok_or_else(|| {
                Error::InvalidInput("input_text.set_value requires a string value".into())
            })?;
            Ok(Operation::TextSet(value.to_owned()))
        }
        Action::SelectOption => {
            only_data_fields(data, &["option"])?;
            let value = data.get("option").and_then(Value::as_str).ok_or_else(|| {
                Error::InvalidInput("input_select.select_option requires a string option".into())
            })?;
            Ok(Operation::SelectOption(value.to_owned()))
        }
        Action::SelectNext | Action::SelectPrevious => {
            only_data_fields(data, &["cycle"])?;
            let cycle = match data.get("cycle") {
                Some(Value::Bool(cycle)) => *cycle,
                Some(_) => return invalid("input_select cycle must be a boolean"),
                None => true,
            };
            Ok(Operation::SelectNext {
                previous: matches!(action, Action::SelectPrevious),
                cycle,
            })
        }
        Action::CounterIncrement => {
            require_no_data(data)?;
            Ok(Operation::CounterIncrement)
        }
        Action::CounterDecrement => {
            require_no_data(data)?;
            Ok(Operation::CounterDecrement)
        }
        Action::CounterReset => {
            require_no_data(data)?;
            Ok(Operation::CounterReset)
        }
        Action::CounterSet => {
            only_data_fields(data, &["value"])?;
            let value = data.get("value").and_then(Value::as_i64).ok_or_else(|| {
                Error::InvalidInput("counter.set_value requires an integer value".into())
            })?;
            Ok(Operation::CounterSet(value))
        }
    }
}

fn require_no_data(data: &Map<String, Value>) -> Result<()> {
    only_data_fields(data, &[])
}

fn only_data_fields(data: &Map<String, Value>, allowed: &[&str]) -> Result<()> {
    if let Some(key) = data.keys().find(|key| !allowed.contains(&key.as_str())) {
        return invalid(format!("unsupported helper service data field '{key}'"));
    }
    for field in allowed {
        // `value` and `option` are required; `cycle` is optional.
        if *field != "cycle" && !data.contains_key(*field) {
            return invalid(format!("helper service is missing required '{field}' data"));
        }
    }
    Ok(())
}

fn apply_operation(
    helper: &Helper,
    current: &str,
    operation: &Operation,
    entity_id: &str,
) -> Result<String> {
    match (helper, operation) {
        (Helper::InputBoolean(_), Operation::Boolean(value)) => {
            Ok(if *value { "on" } else { "off" }.into())
        }
        (Helper::InputBoolean(_), Operation::Toggle) => match current {
            "on" => Ok("off".into()),
            "off" => Ok("on".into()),
            _ => invalid(format!("{entity_id} has invalid boolean state '{current}'")),
        },
        (Helper::InputNumber(config), operation) => {
            let current = parse_number_state(current, entity_id)?;
            let value = match operation {
                Operation::NumberSet(value) => *value,
                Operation::NumberIncrement => current + config.step,
                Operation::NumberDecrement => current - config.step,
                _ => return invalid(format!("service does not apply to {entity_id}")),
            };
            if !value.is_finite() || value < config.min || value > config.max {
                return invalid(format!(
                    "requested value for {entity_id} must be finite and within {}..={}",
                    config.min, config.max
                ));
            }
            Ok(format_number(value))
        }
        (Helper::InputText(config), Operation::TextSet(value)) => {
            if !text_length_valid(value, config.min, config.max) {
                return invalid(format!(
                    "value for {entity_id} must contain {}..={} characters",
                    config.min, config.max
                ));
            }
            Ok(value.clone())
        }
        (Helper::InputSelect(config), Operation::SelectOption(option)) => {
            if !config.options.contains(option) {
                return invalid(format!("'{option}' is not an option for {entity_id}"));
            }
            Ok(option.clone())
        }
        (Helper::InputSelect(config), Operation::SelectNext { previous, cycle }) => {
            let index = config
                .options
                .iter()
                .position(|option| option == current)
                .ok_or_else(|| {
                    Error::InvalidInput(format!("{entity_id} has an unknown select state"))
                })?;
            let last = config.options.len() - 1;
            let next = if *previous {
                if index == 0 {
                    if *cycle { last } else { index }
                } else {
                    index - 1
                }
            } else if index == last {
                if *cycle { 0 } else { index }
            } else {
                index + 1
            };
            Ok(config.options[next].clone())
        }
        (Helper::Counter(config), operation) => {
            let current = parse_counter_state(current, entity_id)?;
            let value = match operation {
                Operation::CounterIncrement => current.checked_add(config.step),
                Operation::CounterDecrement => current.checked_sub(config.step),
                Operation::CounterReset => Some(config.initial),
                Operation::CounterSet(value) => Some(*value),
                _ => return invalid(format!("service does not apply to {entity_id}")),
            }
            .ok_or_else(|| {
                Error::InvalidInput(format!("counter arithmetic overflow for {entity_id}"))
            })?;
            if config.minimum.is_some_and(|minimum| value < minimum)
                || config.maximum.is_some_and(|maximum| value > maximum)
            {
                return invalid(format!(
                    "requested value for {entity_id} exceeds configured bounds"
                ));
            }
            Ok(value.to_string())
        }
        _ => invalid(format!("service does not apply to {entity_id}")),
    }
}

fn initial_state(helper: &Helper) -> String {
    match helper {
        Helper::InputBoolean(config) => if config.initial.unwrap_or(false) {
            "on"
        } else {
            "off"
        }
        .into(),
        Helper::InputNumber(config) => format_number(config.initial.unwrap_or(config.min)),
        Helper::InputText(config) => config.initial.clone().unwrap_or_default(),
        Helper::InputSelect(config) => config
            .initial
            .clone()
            .unwrap_or_else(|| config.options[0].clone()),
        Helper::Counter(config) => config.initial.to_string(),
    }
}

fn validate_state(helper: &Helper, state: &str) -> Result<()> {
    match helper {
        Helper::InputBoolean(_) if matches!(state, "on" | "off") => Ok(()),
        Helper::InputNumber(config) => {
            let value = parse_number_state(state, "restored input_number")?;
            if value < config.min || value > config.max {
                return invalid("restored input_number state is outside the configured bounds");
            }
            Ok(())
        }
        Helper::InputText(config) if text_length_valid(state, config.min, config.max) => Ok(()),
        Helper::InputSelect(config) if config.options.iter().any(|option| option == state) => {
            Ok(())
        }
        Helper::Counter(config) => {
            let value = parse_counter_state(state, "restored counter")?;
            if config.minimum.is_some_and(|minimum| value < minimum)
                || config.maximum.is_some_and(|maximum| value > maximum)
            {
                return invalid("restored counter state is outside the configured bounds");
            }
            Ok(())
        }
        _ => invalid(format!(
            "restored helper state '{state}' is invalid for its configuration"
        )),
    }
}

fn attributes(helper: &Helper, entity_id: &str) -> Map<String, Value> {
    let (name, icon, unit): (&Option<String>, &Option<String>, Option<&str>) = match helper {
        Helper::InputBoolean(config) => (&config.name, &config.icon, None),
        Helper::InputNumber(config) => (
            &config.name,
            &config.icon,
            config.unit_of_measurement.as_deref(),
        ),
        Helper::InputText(config) => (
            &config.name,
            &config.icon,
            config.unit_of_measurement.as_deref(),
        ),
        Helper::InputSelect(config) => (&config.name, &config.icon, None),
        Helper::Counter(config) => (&config.name, &config.icon, None),
    };
    let fallback = entity_id
        .split_once('.')
        .map(|(_, object_id)| object_id)
        .unwrap_or(entity_id);
    let mut attrs = Map::new();
    attrs.insert(
        "friendly_name".into(),
        Value::String(name.as_deref().unwrap_or(fallback).to_owned()),
    );
    if let Some(icon) = icon {
        attrs.insert("icon".into(), Value::String(icon.clone()));
    }
    if let Some(unit) = unit {
        attrs.insert("unit_of_measurement".into(), Value::String(unit.to_owned()));
    }
    match helper {
        Helper::InputNumber(config) => {
            attrs.insert("min".into(), json!(config.min));
            attrs.insert("max".into(), json!(config.max));
            attrs.insert("step".into(), json!(config.step));
            attrs.insert(
                "mode".into(),
                Value::String(
                    match config.mode {
                        NumberMode::Slider => "slider",
                        NumberMode::Box => "box",
                    }
                    .into(),
                ),
            );
        }
        Helper::InputText(config) => {
            attrs.insert("min".into(), json!(config.min));
            attrs.insert("max".into(), json!(config.max));
            attrs.insert(
                "mode".into(),
                Value::String(
                    match config.mode {
                        TextMode::Text => "text",
                        TextMode::Password => "password",
                    }
                    .into(),
                ),
            );
        }
        Helper::InputSelect(config) => {
            attrs.insert("options".into(), json!(&config.options));
        }
        Helper::Counter(config) => {
            attrs.insert("initial".into(), json!(config.initial));
            attrs.insert("step".into(), json!(config.step));
            attrs.insert("restore".into(), Value::Bool(true));
            if let Some(minimum) = config.minimum {
                attrs.insert("minimum".into(), json!(minimum));
            }
            if let Some(maximum) = config.maximum {
                attrs.insert("maximum".into(), json!(maximum));
            }
        }
        Helper::InputBoolean(_) => {}
    }
    attrs
}

fn validate_object_id(domain: &str, object_id: &str) -> Result<()> {
    if object_id.is_empty()
        || !object_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return invalid(format!(
            "{domain} object ID '{object_id}' must use lowercase ASCII letters, digits, and underscores"
        ));
    }
    Ok(())
}

fn validate_metadata(
    domain: &str,
    object_id: &str,
    name: &Option<String>,
    icon: &Option<String>,
    unit: Option<&str>,
) -> Result<()> {
    if name
        .as_deref()
        .is_some_and(|name| name.trim().is_empty() || name.chars().count() > 255)
    {
        return invalid(format!(
            "{domain}.{object_id}.name must be nonempty and at most 255 characters"
        ));
    }
    if icon
        .as_deref()
        .is_some_and(|icon| icon.trim().is_empty() || icon.chars().count() > 255)
    {
        return invalid(format!(
            "{domain}.{object_id}.icon must be nonempty and at most 255 characters"
        ));
    }
    if unit.is_some_and(|unit| unit.trim().is_empty() || unit.chars().count() > 255) {
        return invalid(format!(
            "{domain}.{object_id}.unit_of_measurement must be nonempty and at most 255 characters"
        ));
    }
    Ok(())
}

fn text_length_valid(value: &str, minimum: usize, maximum: usize) -> bool {
    let length = value.chars().count();
    length >= minimum && length <= maximum
}

fn parse_number_state(state: &str, entity_id: &str) -> Result<f64> {
    state
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| {
            Error::InvalidInput(format!("{entity_id} has invalid numeric state '{state}'"))
        })
}

fn parse_counter_state(state: &str, entity_id: &str) -> Result<i64> {
    state.parse::<i64>().map_err(|_| {
        Error::InvalidInput(format!("{entity_id} has invalid counter state '{state}'"))
    })
}

fn format_number(value: f64) -> String {
    if value == 0.0 {
        "0".into()
    } else {
        value.to_string()
    }
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidInput(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::MemoryStore;

    fn runtime(store: Arc<MemoryStore>) -> Runtime {
        Runtime::new(store).unwrap()
    }

    fn call(domain: &str, service: &str, target: Value, data: Value) -> ServiceCall {
        ServiceCall {
            domain: domain.into(),
            service: service.into(),
            data: data.as_object().cloned().unwrap_or_default(),
            target: target.as_object().cloned().unwrap_or_default(),
            context: Context::default(),
        }
    }

    #[test]
    fn initializes_missing_states_and_preserves_restored_values() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("helpers.sqlite");
        let first = Runtime::new(Arc::new(
            hass_storage::SqliteStore::open(&database).unwrap(),
        ))
        .unwrap();
        let helpers: Helpers = serde_yaml::from_str(
            "input_boolean:\n  guest_mode:\n    name: Guest mode\n    initial: false\n",
        )
        .unwrap();
        setup(&first, &helpers).unwrap();
        first
            .call_service(call(
                "input_boolean",
                "turn_on",
                json!({"entity_id": "input_boolean.guest_mode"}),
                json!({}),
            ))
            .unwrap();

        drop(first);
        let restarted = Runtime::new(Arc::new(
            hass_storage::SqliteStore::open(&database).unwrap(),
        ))
        .unwrap();
        setup(&restarted, &helpers).unwrap();
        let state = restarted.get_state("input_boolean.guest_mode").unwrap();
        assert_eq!(state.state, "on");
        assert_eq!(state.attributes["friendly_name"], "Guest mode");
    }

    #[test]
    fn rejects_unknown_or_mixed_targets_before_any_mutation() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        let helpers: Helpers =
            serde_yaml::from_str("input_boolean:\n  first: {}\n  second: {}\n").unwrap();
        setup(&runtime, &helpers).unwrap();

        let result = runtime.call_service(call(
            "input_boolean",
            "turn_on",
            json!({"entity_id": ["input_boolean.first", "input_boolean.missing"]}),
            json!({}),
        ));
        assert!(result.is_err());
        assert_eq!(
            runtime.get_state("input_boolean.first").unwrap().state,
            "off"
        );

        let result = runtime.call_service(call(
            "input_boolean",
            "turn_on",
            json!({"device_id": "device"}),
            json!({"entity_id": "input_boolean.first"}),
        ));
        assert!(result.is_err());
        assert_eq!(
            runtime.get_state("input_boolean.first").unwrap().state,
            "off"
        );
    }

    #[test]
    fn prevalidates_bounds_for_every_target_in_a_batch() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        let helpers: Helpers = serde_yaml::from_str(
            "input_number:\n  first:\n    min: 0\n    max: 2\n    initial: 1\n  second:\n    min: 0\n    max: 2\n    initial: 2\n",
        )
        .unwrap();
        setup(&runtime, &helpers).unwrap();

        let result = runtime.call_service(call(
            "input_number",
            "increment",
            json!({"entity_id": ["input_number.first", "input_number.second"]}),
            json!({}),
        ));
        assert!(result.is_err());
        assert_eq!(runtime.get_state("input_number.first").unwrap().state, "1");
        assert_eq!(runtime.get_state("input_number.second").unwrap().state, "2");
    }

    #[test]
    fn text_select_and_generic_boolean_services_use_real_helper_state() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        let helpers: Helpers = serde_yaml::from_str(
            "input_boolean:\n  guest_mode: {}\ninput_text:\n  note:\n    initial: ok\n    min: 1\n    max: 8\ninput_select:\n  mode:\n    options: [home, away]\n    initial: home\n",
        )
        .unwrap();
        setup(&runtime, &helpers).unwrap();

        runtime
            .call_service(call(
                "input_text",
                "set_value",
                json!({}),
                json!({"entity_id": "input_text.note", "value": "ready"}),
            ))
            .unwrap();
        assert_eq!(runtime.get_state("input_text.note").unwrap().state, "ready");

        runtime
            .call_service(call(
                "input_select",
                "select_next",
                json!({"entity_id": "input_select.mode"}),
                json!({"cycle": false}),
            ))
            .unwrap();
        assert_eq!(
            runtime.get_state("input_select.mode").unwrap().state,
            "away"
        );

        runtime
            .call_service(call(
                "homeassistant",
                "turn_on",
                json!({"entity_id": "input_boolean.guest_mode"}),
                json!({}),
            ))
            .unwrap();
        runtime
            .call_service(call(
                "homeassistant",
                "toggle",
                json!({"entity_id": "input_boolean.guest_mode"}),
                json!({}),
            ))
            .unwrap();
        assert_eq!(
            runtime.get_state("input_boolean.guest_mode").unwrap().state,
            "off"
        );
    }

    #[test]
    fn concurrent_counter_increments_are_serialized() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        let helpers: Helpers =
            serde_yaml::from_str("counter:\n  arrivals:\n    initial: 0\n    maximum: 400\n")
                .unwrap();
        setup(&runtime, &helpers).unwrap();

        let mut workers = Vec::new();
        for _ in 0..4 {
            let runtime = runtime.clone();
            workers.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    runtime
                        .call_service(call(
                            "counter",
                            "increment",
                            json!({"entity_id": "counter.arrivals"}),
                            json!({}),
                        ))
                        .unwrap();
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(runtime.get_state("counter.arrivals").unwrap().state, "400");
    }

    #[test]
    fn publishes_domain_metadata_and_rejects_unknown_nested_fields() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        let helpers: Helpers = serde_yaml::from_str(
            "input_number:\n  target:\n    name: Target\n    min: 10\n    max: 20\n    step: 0.5\n    unit_of_measurement: '%'\n",
        )
        .unwrap();
        setup(&runtime, &helpers).unwrap();
        let state = runtime.get_state("input_number.target").unwrap();
        assert_eq!(state.attributes["friendly_name"], "Target");
        assert_eq!(state.attributes["min"], 10.0);
        assert_eq!(state.attributes["max"], 20.0);
        assert_eq!(state.attributes["step"], 0.5);
        assert_eq!(state.attributes["unit_of_measurement"], "%");

        let malformed: std::result::Result<InputBooleanConfig, _> =
            serde_yaml::from_str("initial: true\nunknown: value\n");
        assert!(malformed.is_err());
    }
}
