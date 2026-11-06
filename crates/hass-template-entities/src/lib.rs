//! Native, state-reactive template entities for the supported Home Assistant
//! `template:` YAML subset.
//!
//! Configuration is deserialized directly into [`TemplateBlock`] values. The
//! integration evaluates templates against the shared runtime and updates the
//! resulting entity states when source states change. Action-backed controls
//! call the shared script executor and never claim a device changed unless the
//! explicitly configured `optimistic` behavior requests that assumption.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::Duration;

use chrono::Utc;
use hass_core::{Context, Error, Event, Runtime, ServiceCall, ServiceHandler, ServiceResult};
use hass_entity::{EntityHandle, EntityLifecycle, EntitySnapshot, EntitySpec};
use hass_registry::{ProviderEntityInfo, Registries};
use hass_script::{run_sequence, validate_sequence};
use hass_template::TemplateEngine;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Number, Value, json};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use uuid::Uuid;

/// Result type shared with the native runtime.
pub type Result<T> = hass_core::Result<T>;

const OWNER_PREFIX: &str = "template-entities";
const ENTITY_ID_MAP_KEY: &str = "hass-template-entities.entity-ids.v1";
const MAX_ENTITIES: usize = 256;
const MAX_ATTRIBUTES: usize = 64;
const EVENT_DRAIN_LIMIT: usize = 1024;
const ACTION_QUEUE_CAPACITY: usize = 32;

/// One item under the root `template:` configuration.
///
/// Home Assistant blocks can contain several domains in one mapping. This
/// port currently implements state-based sensors and native controls only;
/// trigger-backed blocks and the other template domains are rejected by
/// [`validate`]. A domain can contain either one mapping or a list of entity
/// mappings, matching Home Assistant's `ensure_list` YAML schema.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TemplateBlock {
    #[serde(deserialize_with = "deserialize_one_or_many")]
    pub sensor: Vec<SensorConfig>,
    #[serde(deserialize_with = "deserialize_one_or_many")]
    pub binary_sensor: Vec<BinarySensorConfig>,
    #[serde(deserialize_with = "deserialize_one_or_many")]
    pub switch: Vec<SwitchConfig>,
    #[serde(deserialize_with = "deserialize_one_or_many")]
    pub number: Vec<NumberConfig>,
    #[serde(deserialize_with = "deserialize_one_or_many")]
    pub select: Vec<SelectConfig>,
    #[serde(deserialize_with = "deserialize_one_or_many")]
    pub button: Vec<ButtonConfig>,
    /// Parsed so unsupported trigger blocks receive an intentional validation
    /// error instead of being silently treated as an empty block.
    pub triggers: Option<Vec<Value>>,
    /// Block-level trigger actions are not part of the current port.
    pub actions: Option<Vec<Value>>,
    /// Block-level conditions are only valid for trigger-backed blocks.
    pub conditions: Option<Vec<Value>>,
    /// Trigger-only variables and identity are recognized for useful errors.
    pub trigger_variables: Option<Map<String, Value>>,
    pub unique_id: Option<String>,
    pub variables: Option<Map<String, Value>>,
}

macro_rules! entity_config {
    ($name:ident { $($fields:tt)* }) => {
        #[derive(Clone, Debug, Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            #[serde(default)]
            pub name: Option<String>,
            #[serde(default)]
            pub unique_id: Option<String>,
            #[serde(default)]
            pub default_entity_id: Option<String>,
            #[serde(default)]
            pub availability: Option<String>,
            #[serde(default)]
            pub icon: Option<String>,
            #[serde(default)]
            pub picture: Option<String>,
            #[serde(default)]
            pub variables: Map<String, Value>,
            #[serde(default)]
            pub attributes: Option<AttributeConfig>,
            #[serde(default)]
            pub conditions: Option<Value>,
            $($fields)*
        }

        impl $name {
            fn common(&self) -> EntityCommon {
                EntityCommon {
                    name: self.name.clone(),
                    unique_id: self.unique_id.clone(),
                    default_entity_id: self.default_entity_id.clone(),
                    availability: self.availability.clone(),
                    icon: self.icon.clone(),
                    picture: self.picture.clone(),
                    variables: self.variables.clone(),
                    attributes: self.attributes.clone(),
                }
            }
        }
    };
}

// Sensor entity configuration (`state` is required by the upstream schema).
entity_config!(SensorConfig {
    pub state: String,
    #[serde(default)]
    pub device_class: Option<String>,
    #[serde(default)]
    pub state_class: Option<String>,
    #[serde(default)]
    pub unit_of_measurement: Option<String>,
});

// Binary sensor entity configuration (`state` is required upstream).
entity_config!(BinarySensorConfig {
    pub state: String,
    #[serde(default)]
    pub device_class: Option<String>,
    #[serde(default)]
    pub unit_of_measurement: Option<String>,
});

// Switch entity configuration.
entity_config!(SwitchConfig {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub turn_on: Option<Vec<Value>>,
    #[serde(default)]
    pub turn_off: Option<Vec<Value>>,
    #[serde(default)]
    pub optimistic: Option<bool>,
});

// Number entity configuration.
entity_config!(NumberConfig {
    #[serde(default)]
    pub state: Option<String>,
    pub set_value: Vec<Value>,
    #[serde(default)]
    pub min: Option<Value>,
    #[serde(default)]
    pub max: Option<Value>,
    #[serde(default)]
    pub step: Option<Value>,
    #[serde(default)]
    pub unit_of_measurement: Option<String>,
    #[serde(default)]
    pub device_class: Option<String>,
    #[serde(default)]
    pub optimistic: Option<bool>,
});

// Select entity configuration.
entity_config!(SelectConfig {
    pub options: String,
    #[serde(default)]
    pub select_option: Option<Vec<Value>>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub optimistic: Option<bool>,
});

// Button entity configuration (`press` is required upstream; an empty
// sequence is valid and still records the press time).
entity_config!(ButtonConfig {
    pub press: Vec<Value>,
    #[serde(default)]
    pub device_class: Option<String>,
});

/// Configured attributes can be a map of templates or one template that
/// returns an object.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum AttributeConfig {
    Template(String),
    PerAttribute(BTreeMap<String, String>),
}

fn deserialize_one_or_many<'de, D, T>(deserializer: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany<T> {
        Many(Vec<T>),
        One(T),
    }

    match OneOrMany::deserialize(deserializer)? {
        OneOrMany::Many(items) => Ok(items),
        OneOrMany::One(item) => Ok(vec![item]),
    }
}

/// Validate the supported subset of Home Assistant's modern `template:`
/// configuration without reading or mutating Runtime state.
pub fn validate(blocks: &[TemplateBlock]) -> Result<()> {
    if blocks.is_empty() {
        return Ok(());
    }

    let mut count = 0usize;
    let mut identities = HashSet::new();
    for (block_index, block) in blocks.iter().enumerate() {
        if block.triggers.is_some()
            || block.actions.is_some()
            || block.conditions.is_some()
            || block.trigger_variables.is_some()
            || block.unique_id.is_some()
            || block
                .variables
                .as_ref()
                .is_some_and(|values| !values.is_empty())
        {
            return invalid(format!(
                "template block {block_index} uses trigger-based fields; trigger blocks are unsupported"
            ));
        }
        if block.sensor.is_empty()
            && block.binary_sensor.is_empty()
            && block.switch.is_empty()
            && block.number.is_empty()
            && block.select.is_empty()
            && block.button.is_empty()
        {
            return invalid(format!(
                "template block {block_index} must configure at least one supported entity"
            ));
        }
        count = count
            .saturating_add(block.sensor.len())
            .saturating_add(block.binary_sensor.len())
            .saturating_add(block.switch.len())
            .saturating_add(block.number.len())
            .saturating_add(block.select.len())
            .saturating_add(block.button.len());
        if count > MAX_ENTITIES {
            return invalid(format!(
                "template configuration exceeds the {MAX_ENTITIES} entity limit"
            ));
        }

        for config in &block.sensor {
            validate_common(
                "sensor",
                &config.common(),
                config.conditions.as_ref(),
                &mut identities,
            )?;
            validate_template("sensor.state", &config.state)?;
            validate_optional_label("sensor.device_class", config.device_class.as_deref())?;
            validate_optional_label("sensor.state_class", config.state_class.as_deref())?;
            validate_optional_label(
                "sensor.unit_of_measurement",
                config.unit_of_measurement.as_deref(),
            )?;
        }
        for config in &block.binary_sensor {
            validate_common(
                "binary_sensor",
                &config.common(),
                config.conditions.as_ref(),
                &mut identities,
            )?;
            validate_template("binary_sensor.state", &config.state)?;
            validate_optional_label("binary_sensor.device_class", config.device_class.as_deref())?;
            validate_optional_label(
                "binary_sensor.unit_of_measurement",
                config.unit_of_measurement.as_deref(),
            )?;
        }
        for config in &block.switch {
            validate_common(
                "switch",
                &config.common(),
                config.conditions.as_ref(),
                &mut identities,
            )?;
            if let Some(state) = &config.state {
                validate_template("switch.state", state)?;
            }
            validate_optional_action("switch.turn_on", config.turn_on.as_deref())?;
            validate_optional_action("switch.turn_off", config.turn_off.as_deref())?;
        }
        for config in &block.number {
            validate_common(
                "number",
                &config.common(),
                config.conditions.as_ref(),
                &mut identities,
            )?;
            if let Some(state) = &config.state {
                validate_template("number.state", state)?;
            }
            validate_action("number.set_value", &config.set_value)?;
            validate_numeric_template("number.min", config.min.as_ref())?;
            validate_numeric_template("number.max", config.max.as_ref())?;
            validate_numeric_template("number.step", config.step.as_ref())?;
            validate_optional_label(
                "number.unit_of_measurement",
                config.unit_of_measurement.as_deref(),
            )?;
            validate_optional_label("number.device_class", config.device_class.as_deref())?;
        }
        for config in &block.select {
            validate_common(
                "select",
                &config.common(),
                config.conditions.as_ref(),
                &mut identities,
            )?;
            validate_template("select.options", &config.options)?;
            if let Some(state) = &config.state {
                validate_template("select.state", state)?;
            }
            validate_optional_action("select.select_option", config.select_option.as_deref())?;
        }
        for config in &block.button {
            validate_common(
                "button",
                &config.common(),
                config.conditions.as_ref(),
                &mut identities,
            )?;
            validate_action("button.press", &config.press)?;
            validate_optional_label("button.device_class", config.device_class.as_deref())?;
        }
    }
    Ok(())
}

fn validate_common(
    domain: &str,
    common: &EntityCommon,
    conditions: Option<&Value>,
    identities: &mut HashSet<String>,
) -> Result<()> {
    if let Some(name) = &common.name {
        validate_template(&format!("{domain}.name"), name)?;
    }
    for (field, template) in [
        ("availability", &common.availability),
        ("icon", &common.icon),
        ("picture", &common.picture),
    ] {
        if let Some(template) = template {
            validate_template(&format!("{domain}.{field}"), template)?;
        }
    }
    if let Some(unique_id) = &common.unique_id {
        if unique_id.trim().is_empty() || unique_id.len() > 255 {
            return invalid(format!("{domain}.unique_id must contain 1 to 255 bytes"));
        }
        let key = format!("{domain}:unique:{unique_id}");
        if !identities.insert(key) {
            return invalid(format!("duplicate unique_id in template {domain} entities"));
        }
    }
    if let Some(entity_id) = &common.default_entity_id {
        let Some((entity_domain, object_id)) = entity_id.split_once('.') else {
            return invalid(format!(
                "{domain}.default_entity_id must be a full entity ID"
            ));
        };
        if entity_domain != domain || !valid_object_id(object_id) {
            return invalid(format!(
                "{domain}.default_entity_id must be a valid {domain} entity ID"
            ));
        }
    }
    if common.unique_id.is_none()
        && let Some(identity) = common.default_entity_id.as_ref().or(common.name.as_ref())
    {
        let key = format!("{domain}:configured:{identity}");
        if !identities.insert(key) {
            return invalid(format!(
                "duplicate template {domain} identity without unique_id"
            ));
        }
    }
    if conditions.is_some() {
        return invalid(format!(
            "{domain}.conditions requires a trigger-based template block, which is unsupported"
        ));
    }
    if common.variables.len() > MAX_ATTRIBUTES {
        return invalid(format!(
            "{domain}.variables exceeds {MAX_ATTRIBUTES} entries"
        ));
    }
    for (key, value) in &common.variables {
        if key.is_empty() || key.len() > 255 {
            return invalid(format!("{domain}.variables contains an invalid key"));
        }
        validate_json_size(value, &format!("{domain}.variables.{key}"))?;
    }
    if let Some(attributes) = &common.attributes {
        match attributes {
            AttributeConfig::Template(template) => {
                validate_template(&format!("{domain}.attributes"), template)?
            }
            AttributeConfig::PerAttribute(values) => {
                if values.len() > MAX_ATTRIBUTES {
                    return invalid(format!(
                        "{domain}.attributes exceeds {MAX_ATTRIBUTES} entries"
                    ));
                }
                for (key, template) in values {
                    if key.trim().is_empty() || key.len() > 255 {
                        return invalid(format!("{domain}.attributes contains an invalid key"));
                    }
                    if blocked_attributes(domain).contains(&key.as_str()) {
                        return invalid(format!(
                            "{domain}.attributes cannot set the protected '{key}' attribute"
                        ));
                    }
                    validate_template(&format!("{domain}.attributes.{key}"), template)?;
                }
            }
        }
    }
    Ok(())
}

fn validate_template(field: &str, source: &str) -> Result<()> {
    if source.len() > 64 * 1024 {
        return invalid(format!("{field} exceeds the 64 KiB template limit"));
    }
    Ok(())
}

fn validate_optional_label(field: &str, value: Option<&str>) -> Result<()> {
    if value.is_some_and(|value| value.trim().is_empty() || value.len() > 255) {
        return invalid(format!("{field} must contain 1 to 255 bytes"));
    }
    Ok(())
}

fn validate_numeric_template(field: &str, value: Option<&Value>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    match value {
        Value::Number(number) if number.as_f64().is_some_and(f64::is_finite) => Ok(()),
        Value::String(template) => validate_template(field, template),
        _ => invalid(format!("{field} must be a number or template string")),
    }
}

fn validate_action(field: &str, sequence: &[Value]) -> Result<()> {
    let sequence = Value::Array(sequence.to_vec());
    validate_sequence(&sequence)
        .map_err(|error| Error::InvalidInput(format!("invalid {field} action sequence: {error}")))
}

fn validate_optional_action(field: &str, sequence: Option<&[Value]>) -> Result<()> {
    if let Some(sequence) = sequence {
        validate_action(field, sequence)?;
    }
    Ok(())
}

fn validate_json_size(value: &Value, field: &str) -> Result<()> {
    let size = serde_json::to_vec(value)
        .map_err(|_| Error::InvalidInput(format!("{field} is not JSON compatible")))?
        .len();
    if size > 64 * 1024 {
        return invalid(format!("{field} exceeds 64 KiB"));
    }
    Ok(())
}

fn valid_object_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidInput(message.into()))
}

#[derive(Clone, Debug)]
struct EntityCommon {
    name: Option<String>,
    unique_id: Option<String>,
    default_entity_id: Option<String>,
    availability: Option<String>,
    icon: Option<String>,
    picture: Option<String>,
    variables: Map<String, Value>,
    attributes: Option<AttributeConfig>,
}

#[derive(Clone, Debug)]
struct PendingEntity {
    domain: &'static str,
    config_key: String,
    common: EntityCommon,
    kind: EntityKind,
}

#[derive(Clone, Debug)]
enum EntityKind {
    Sensor {
        state: String,
        device_class: Option<String>,
        state_class: Option<String>,
        unit_of_measurement: Option<String>,
    },
    BinarySensor {
        state: String,
        device_class: Option<String>,
        unit_of_measurement: Option<String>,
    },
    Switch {
        state: Option<String>,
        turn_on: Option<Vec<Value>>,
        turn_off: Option<Vec<Value>>,
        optimistic: bool,
    },
    Number {
        state: Option<String>,
        set_value: Vec<Value>,
        min: Option<Value>,
        max: Option<Value>,
        step: Option<Value>,
        unit_of_measurement: Option<String>,
        device_class: Option<String>,
        optimistic: bool,
    },
    Select {
        state: Option<String>,
        options: String,
        select_option: Option<Vec<Value>>,
        optimistic: bool,
    },
    Button {
        press: Vec<Value>,
        device_class: Option<String>,
    },
}

impl EntityKind {
    fn state_template(&self) -> Option<&str> {
        match self {
            Self::Sensor { state, .. } | Self::BinarySensor { state, .. } => Some(state),
            Self::Switch { state, .. }
            | Self::Number { state, .. }
            | Self::Select { state, .. } => state.as_deref(),
            Self::Button { .. } => None,
        }
    }

    fn device_class(&self) -> Option<&str> {
        match self {
            Self::Sensor { device_class, .. }
            | Self::BinarySensor { device_class, .. }
            | Self::Number { device_class, .. }
            | Self::Button { device_class, .. } => device_class.as_deref(),
            Self::Switch { .. } | Self::Select { .. } => None,
        }
    }

    fn unit_of_measurement(&self) -> Option<&str> {
        match self {
            Self::Sensor {
                unit_of_measurement,
                ..
            }
            | Self::BinarySensor {
                unit_of_measurement,
                ..
            }
            | Self::Number {
                unit_of_measurement,
                ..
            } => unit_of_measurement.as_deref(),
            _ => None,
        }
    }

    fn state_class(&self) -> Option<&str> {
        match self {
            Self::Sensor { state_class, .. } => state_class.as_deref(),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct EntityDefinition {
    entity_id: String,
    handle: EntityHandle,
    domain: &'static str,
    common: EntityCommon,
    kind: EntityKind,
}

impl EntityDefinition {
    fn current(&self) -> Option<Self> {
        self.handle.current_entity_id().map(|entity_id| Self {
            entity_id,
            ..self.clone()
        })
    }
}

fn flatten_blocks(blocks: &[TemplateBlock]) -> Vec<PendingEntity> {
    let mut indices = HashMap::<&'static str, usize>::new();
    let mut pending = Vec::new();
    let mut add = |domain: &'static str, common: EntityCommon, kind: EntityKind| {
        let index = indices.entry(domain).or_default();
        let config_key = entity_id_map_key(domain, *index, &common);
        *index += 1;
        pending.push(PendingEntity {
            domain,
            config_key,
            common,
            kind,
        });
    };

    for block in blocks {
        for config in &block.sensor {
            add(
                "sensor",
                config.common(),
                EntityKind::Sensor {
                    state: config.state.clone(),
                    device_class: config.device_class.clone(),
                    state_class: config.state_class.clone(),
                    unit_of_measurement: config.unit_of_measurement.clone(),
                },
            );
        }
        for config in &block.binary_sensor {
            add(
                "binary_sensor",
                config.common(),
                EntityKind::BinarySensor {
                    state: config.state.clone(),
                    device_class: config.device_class.clone(),
                    unit_of_measurement: config.unit_of_measurement.clone(),
                },
            );
        }
        for config in &block.switch {
            add(
                "switch",
                config.common(),
                EntityKind::Switch {
                    state: config.state.clone(),
                    turn_on: config.turn_on.clone(),
                    turn_off: config.turn_off.clone(),
                    optimistic: config.optimistic.unwrap_or(false),
                },
            );
        }
        for config in &block.number {
            add(
                "number",
                config.common(),
                EntityKind::Number {
                    state: config.state.clone(),
                    set_value: config.set_value.clone(),
                    min: config.min.clone(),
                    max: config.max.clone(),
                    step: config.step.clone(),
                    unit_of_measurement: config.unit_of_measurement.clone(),
                    device_class: config.device_class.clone(),
                    optimistic: config.optimistic.unwrap_or(false),
                },
            );
        }
        for config in &block.select {
            add(
                "select",
                config.common(),
                EntityKind::Select {
                    state: config.state.clone(),
                    options: config.options.clone(),
                    select_option: config.select_option.clone(),
                    optimistic: config.optimistic.unwrap_or(false),
                },
            );
        }
        for config in &block.button {
            add(
                "button",
                config.common(),
                EntityKind::Button {
                    press: config.press.clone(),
                    device_class: config.device_class.clone(),
                },
            );
        }
    }
    pending
}

fn entity_id_map_key(domain: &str, index: usize, common: &EntityCommon) -> String {
    if let Some(entity_id) = &common.default_entity_id {
        format!("{domain}:entity_id:{entity_id}")
    } else if let Some(name) = &common.name {
        format!("{domain}:name:{name}")
    } else {
        format!("{domain}:default:{index}")
    }
}

fn default_name(domain: &str) -> &'static str {
    match domain {
        "sensor" => "Template Sensor",
        "binary_sensor" => "Template Binary Sensor",
        "switch" => "Template Switch",
        "number" => "Template Number",
        "select" => "Template Select",
        "button" => "Template Button",
        _ => "Template Entity",
    }
}

fn static_name(common: &EntityCommon, domain: &str) -> Option<String> {
    common
        .name
        .as_ref()
        .filter(|value| !value.contains("{{") && !value.contains("{%"))
        .cloned()
        .or_else(|| Some(default_name(domain).to_owned()))
}

fn render_name(engine: &TemplateEngine, common: &EntityCommon, domain: &str) -> String {
    let Some(source) = common.name.as_deref() else {
        return default_name(domain).to_owned();
    };
    let vars = render_variables(engine, &common.variables).unwrap_or_default();
    render_string(engine, source, &vars).unwrap_or_else(|_| default_name(domain).to_owned())
}

fn slugify(input: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in input.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator && !output.is_empty() {
                output.push('_');
            }
            output.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    if output.is_empty() {
        "template_entity".to_owned()
    } else {
        output
    }
}

fn render_typed(
    engine: &TemplateEngine,
    source: &str,
    variables: &Map<String, Value>,
) -> Result<Value> {
    let trimmed = source.trim();
    let has_jinja = ["{{", "{%", "{#"]
        .iter()
        .any(|delimiter| source.contains(delimiter));
    if !has_jinja {
        return Ok(Value::String(source.to_owned()));
    }
    if trimmed.starts_with("{{")
        && trimmed.ends_with("}}")
        && let Ok(value) = engine.render_value(source, variables)
    {
        return Ok(value);
    }
    engine.render(source, variables).map(Value::String)
}

fn render_string(
    engine: &TemplateEngine,
    source: &str,
    variables: &Map<String, Value>,
) -> Result<String> {
    let value = render_typed(engine, source, variables)?;
    match value {
        Value::String(value) => Ok(value),
        Value::Null => Ok(String::new()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(value.to_string()),
        Value::Array(_) | Value::Object(_) => Err(Error::InvalidInput(
            "template value cannot be used as a string".into(),
        )),
    }
}

fn render_variables(
    engine: &TemplateEngine,
    variables: &Map<String, Value>,
) -> Result<Map<String, Value>> {
    let mut rendered = Map::new();
    for (key, value) in variables {
        let value = match value {
            Value::String(source) => render_typed(engine, source, variables)?,
            other => other.clone(),
        };
        rendered.insert(key.clone(), value);
    }
    Ok(rendered)
}

fn render_entity(
    runtime: &Runtime,
    engine: &TemplateEngine,
    entity: &EntityDefinition,
) -> StateWrite {
    let previous = runtime.get_state(&entity.entity_id);
    let mut attributes = previous
        .as_ref()
        .map(|state| state.attributes.clone())
        .unwrap_or_default();
    let variables = match render_variables(engine, &entity.common.variables) {
        Ok(variables) => variables,
        Err(error) => {
            warn!(entity_id = %entity.entity_id, %error, "template entity variables could not be rendered");
            return StateWrite {
                state: unavailable_state(&entity.kind, entity.common.availability.is_some()),
                attributes,
            };
        }
    };

    set_base_attributes(engine, entity, &variables, &mut attributes);
    match render_standard_attributes(engine, entity, &variables, &mut attributes) {
        Ok(()) => {}
        Err(error) => {
            warn!(entity_id = %entity.entity_id, %error, "template entity attributes could not be rendered");
        }
    }
    match render_custom_attributes(engine, entity, &variables, &mut attributes) {
        Ok(()) => {}
        Err(error) => {
            warn!(entity_id = %entity.entity_id, %error, "template entity custom attributes could not be rendered");
        }
    }

    if !render_availability(engine, entity, &variables) {
        return StateWrite {
            state: "unavailable".into(),
            attributes,
        };
    }
    let state = match entity.kind.state_template() {
        Some(source) => render_typed(engine, source, &variables)
            .and_then(|value| normalize_state(entity, value)),
        None => Ok(previous
            .map(|state| state.state)
            .unwrap_or_else(|| "unknown".into())),
    };
    let state = match state {
        Ok(state) => state,
        Err(error) => {
            warn!(entity_id = %entity.entity_id, %error, "template entity state could not be rendered");
            unavailable_state(&entity.kind, entity.common.availability.is_some())
        }
    };
    StateWrite { state, attributes }
}

struct StateWrite {
    state: String,
    attributes: Map<String, Value>,
}

fn unavailable_state(kind: &EntityKind, has_availability_template: bool) -> String {
    match kind {
        // Upstream drops the native value on a state-template error. When a
        // separate availability template exists, that template remains the
        // authority for availability and the empty native value is `unknown`.
        EntityKind::Sensor { .. }
        | EntityKind::BinarySensor { .. }
        | EntityKind::Switch { .. }
        | EntityKind::Number { .. }
        | EntityKind::Select { .. }
            if has_availability_template =>
        {
            "unknown".into()
        }
        _ => "unavailable".into(),
    }
}

fn set_base_attributes(
    engine: &TemplateEngine,
    entity: &EntityDefinition,
    variables: &Map<String, Value>,
    attributes: &mut Map<String, Value>,
) {
    let name = match entity.common.name.as_deref() {
        Some(source) => render_string(engine, source, variables),
        None => Ok(default_name(entity.domain).to_owned()),
    };
    match name {
        Ok(name) => {
            attributes.insert("friendly_name".into(), Value::String(name));
        }
        Err(error) => {
            debug!(entity_id = %entity.entity_id, %error, "template entity name kept its previous value");
            attributes
                .entry("friendly_name")
                .or_insert_with(|| Value::String(default_name(entity.domain).to_owned()));
        }
    }

    if let Some(source) = entity.common.icon.as_deref() {
        match render_string(engine, source, variables) {
            Ok(icon) => {
                attributes.insert("icon".into(), Value::String(icon));
            }
            Err(error) => {
                debug!(entity_id = %entity.entity_id, %error, "template entity icon kept its previous value");
            }
        }
    }
    if let Some(source) = entity.common.picture.as_deref() {
        match render_string(engine, source, variables) {
            Ok(picture) => {
                attributes.insert("entity_picture".into(), Value::String(picture));
            }
            Err(error) => {
                debug!(entity_id = %entity.entity_id, %error, "template entity picture kept its previous value");
            }
        }
    }
}

fn render_standard_attributes(
    engine: &TemplateEngine,
    entity: &EntityDefinition,
    variables: &Map<String, Value>,
    attributes: &mut Map<String, Value>,
) -> Result<()> {
    if let Some(device_class) = entity.kind.device_class() {
        attributes.insert("device_class".into(), Value::String(device_class.into()));
    }
    if let Some(state_class) = entity.kind.state_class() {
        attributes.insert("state_class".into(), Value::String(state_class.into()));
    }
    if let Some(unit) = entity.kind.unit_of_measurement() {
        attributes.insert("unit_of_measurement".into(), Value::String(unit.into()));
    }
    if matches!(
        &entity.kind,
        EntityKind::Switch {
            optimistic: true,
            ..
        } | EntityKind::Number {
            optimistic: true,
            ..
        } | EntityKind::Select {
            optimistic: true,
            ..
        }
    ) {
        attributes.insert("assumed_state".into(), Value::Bool(true));
    }
    match &entity.kind {
        EntityKind::Number { min, max, step, .. } => {
            let minimum = render_number_option(engine, min.as_ref(), 0.0, variables)?;
            let maximum = render_number_option(engine, max.as_ref(), 100.0, variables)?;
            let step = render_number_option(engine, step.as_ref(), 1.0, variables)?;
            if minimum > maximum || step <= 0.0 {
                return invalid("number min/max/step values are inconsistent");
            }
            insert_number_attribute(attributes, "min", minimum)?;
            insert_number_attribute(attributes, "max", maximum)?;
            insert_number_attribute(attributes, "step", step)?;
        }
        EntityKind::Select { options, .. } => {
            let options = render_typed(engine, options, variables)?;
            let options = match options {
                Value::Array(values) => values
                    .into_iter()
                    .map(|value| match value {
                        Value::String(value) => Ok(Value::String(value)),
                        _ => Err(Error::InvalidInput("select options must be strings".into())),
                    })
                    .collect::<Result<Vec<_>>>()?,
                Value::String(value) => value
                    .lines()
                    .map(|line| Value::String(line.to_owned()))
                    .collect(),
                _ => return invalid("select options template must return a list of strings"),
            };
            if options.len() > MAX_ATTRIBUTES {
                return invalid(format!("select options exceed {MAX_ATTRIBUTES} entries"));
            }
            attributes.insert("options".into(), Value::Array(options));
        }
        EntityKind::Sensor { .. }
        | EntityKind::BinarySensor { .. }
        | EntityKind::Switch { .. }
        | EntityKind::Button { .. } => {}
    }
    Ok(())
}

fn render_number_option(
    engine: &TemplateEngine,
    value: Option<&Value>,
    default: f64,
    variables: &Map<String, Value>,
) -> Result<f64> {
    let Some(value) = value else {
        return Ok(default);
    };
    let value = match value {
        Value::String(source) => render_typed(engine, source, variables)?,
        value => value.clone(),
    };
    let number = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(value) => value.parse::<f64>().ok(),
        _ => None,
    }
    .filter(|number| number.is_finite())
    .ok_or_else(|| Error::InvalidInput("number min/max/step template is not numeric".into()))?;
    Ok(number)
}

fn insert_number_attribute(
    attributes: &mut Map<String, Value>,
    key: &str,
    value: f64,
) -> Result<()> {
    let number = Number::from_f64(value)
        .ok_or_else(|| Error::InvalidInput("non-finite number entity attribute".into()))?;
    attributes.insert(key.into(), Value::Number(number));
    Ok(())
}

fn render_custom_attributes(
    engine: &TemplateEngine,
    entity: &EntityDefinition,
    variables: &Map<String, Value>,
    attributes: &mut Map<String, Value>,
) -> Result<()> {
    let Some(config) = &entity.common.attributes else {
        return Ok(());
    };
    let mut rendered = match config {
        AttributeConfig::PerAttribute(values) => {
            let mut rendered = Map::new();
            for (key, source) in values {
                rendered.insert(key.clone(), render_typed(engine, source, variables)?);
            }
            rendered
        }
        AttributeConfig::Template(source) => match render_typed(engine, source, variables)? {
            Value::Object(values) => values,
            Value::String(text) => {
                serde_json::from_str::<Map<String, Value>>(&text).map_err(|error| {
                    Error::InvalidInput(format!(
                        "attributes template must return an object: {error}"
                    ))
                })?
            }
            _ => return invalid("attributes template must return an object"),
        },
    };
    if rendered.len() > MAX_ATTRIBUTES {
        return invalid(format!(
            "rendered attributes exceed {MAX_ATTRIBUTES} entries"
        ));
    }
    for key in blocked_attributes(entity.domain) {
        if rendered.remove(*key).is_some() {
            debug!(entity_id = %entity.entity_id, attribute = key, "blocked template entity attribute was ignored");
        }
    }
    attributes.extend(rendered);
    Ok(())
}

fn blocked_attributes(domain: &str) -> &'static [&'static str] {
    match domain {
        "sensor" => &[
            "device_class",
            "state_class",
            "unit_of_measurement",
            "state",
        ],
        "binary_sensor" => &["device_class", "unit_of_measurement", "state"],
        "switch" | "button" => &["device_class", "state"],
        "number" => &[
            "device_class",
            "unit_of_measurement",
            "min",
            "max",
            "step",
            "state",
        ],
        "select" => &["options", "state"],
        _ => &[],
    }
}

fn render_availability(
    engine: &TemplateEngine,
    entity: &EntityDefinition,
    variables: &Map<String, Value>,
) -> bool {
    let Some(source) = entity.common.availability.as_deref() else {
        return true;
    };
    match render_typed(engine, source, variables).and_then(value_as_bool) {
        Ok(available) => available,
        Err(error) => {
            // Pinned Home Assistant behavior leaves the entity available when
            // an availability template itself fails to evaluate.
            warn!(entity_id = %entity.entity_id, %error, "template availability expression failed; retaining availability");
            true
        }
    }
}

fn value_as_bool(value: Value) -> Result<bool> {
    match value {
        Value::Bool(value) => Ok(value),
        Value::Number(value) => match value.as_f64() {
            Some(value) => Ok(value != 0.0),
            None => invalid("availability expression is not a finite number"),
        },
        Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(true),
            "false" | "no" | "off" | "0" | "" => Ok(false),
            _ => invalid("availability expression is not boolean"),
        },
        _ => invalid("availability expression is not boolean"),
    }
}

fn normalize_state(entity: &EntityDefinition, value: Value) -> Result<String> {
    match &entity.kind {
        EntityKind::Sensor {
            device_class,
            state_class,
            unit_of_measurement,
            ..
        } => normalize_sensor_state(value, device_class, state_class, unit_of_measurement),
        EntityKind::BinarySensor { .. } | EntityKind::Switch { .. } => {
            normalize_binary_state(value)
        }
        EntityKind::Number { .. } => normalize_number_state(value),
        EntityKind::Select { .. } => value_as_string(value),
        EntityKind::Button { .. } => Ok("unknown".into()),
    }
}

fn normalize_sensor_state(
    value: Value,
    device_class: &Option<String>,
    state_class: &Option<String>,
    unit: &Option<String>,
) -> Result<String> {
    let state = value_as_string(value)?;
    if state.is_empty() || state.eq_ignore_ascii_case("none") {
        return Ok("unknown".into());
    }
    if matches!(state.as_str(), "unknown" | "unavailable") {
        return Ok(state);
    }
    let numeric_class = device_class.as_deref().is_some_and(|class| {
        matches!(
            class,
            "apparent_power"
                | "aqi"
                | "atmospheric_pressure"
                | "battery"
                | "carbon_dioxide"
                | "carbon_monoxide"
                | "current"
                | "data_rate"
                | "data_size"
                | "distance"
                | "duration"
                | "energy"
                | "energy_storage"
                | "frequency"
                | "gas"
                | "humidity"
                | "illuminance"
                | "monetary"
                | "nitrogen_dioxide"
                | "nitrogen_monoxide"
                | "nitrous_oxide"
                | "ozone"
                | "ph"
                | "pm1"
                | "pm10"
                | "pm25"
                | "power"
                | "power_factor"
                | "precipitation"
                | "precipitation_intensity"
                | "pressure"
                | "reactive_power"
                | "signal_strength"
                | "sound_pressure"
                | "speed"
                | "sulphur_dioxide"
                | "temperature"
                | "volatile_organic_compounds"
                | "volatile_organic_compounds_parts"
                | "voltage"
                | "volume"
                | "volume_flow_rate"
                | "volume_storage"
                | "water"
                | "weight"
        )
    });
    if unit.is_some() || state_class.is_some() || numeric_class {
        let parsed = state
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite());
        if parsed.is_none() {
            return invalid("numeric template sensor state is not numeric");
        }
    }
    Ok(state)
}

fn normalize_binary_state(value: Value) -> Result<String> {
    match value {
        Value::Bool(value) => Ok(if value { "on" } else { "off" }.into()),
        Value::Number(value) => match value.as_i64() {
            Some(0) => Ok("off".into()),
            Some(1) => Ok("on".into()),
            _ => invalid("binary template state must be boolean or 0/1"),
        },
        Value::Null => Ok("unknown".into()),
        Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => Ok("on".into()),
            "off" | "false" | "no" | "0" => Ok("off".into()),
            "unknown" => Ok("unknown".into()),
            "unavailable" => Ok("unavailable".into()),
            _ => invalid("binary template state must be on/off or boolean"),
        },
        _ => invalid("binary template state must be boolean or text"),
    }
}

fn normalize_number_state(value: Value) -> Result<String> {
    let state = value_as_string(value)?;
    if state.is_empty() || state.eq_ignore_ascii_case("none") {
        return Ok("unknown".into());
    }
    if matches!(state.as_str(), "unknown" | "unavailable") {
        return Ok(state);
    }
    if !state.parse::<f64>().ok().is_some_and(f64::is_finite) {
        return invalid("number template state is not numeric");
    }
    Ok(state)
}

fn value_as_string(value: Value) -> Result<String> {
    match value {
        Value::Null => Ok("unknown".into()),
        Value::String(value) => Ok(value),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(value.to_string()),
        Value::Array(_) | Value::Object(_) => Err(Error::InvalidInput(
            "entity state template returned a complex value".into(),
        )),
    }
}

fn write_entities(
    runtime: &Runtime,
    engine: &TemplateEngine,
    entities: &[EntityDefinition],
    context: Context,
) -> Result<()> {
    for entity in entities {
        let Some(entity) = entity.current() else {
            continue;
        };
        let rendered = render_entity(runtime, engine, &entity);
        entity.handle.publish(EntitySnapshot {
            available: rendered.state != "unavailable",
            state: Some(rendered.state),
            attributes: rendered.attributes,
            context: context.clone(),
        })?;
    }
    Ok(())
}

#[derive(Clone)]
struct ActionDispatcher {
    runtime: Runtime,
    active: Arc<AtomicBool>,
    cancel: watch::Sender<bool>,
    running: Arc<(Mutex<usize>, Condvar)>,
}

impl ActionDispatcher {
    fn new(runtime: Runtime, active: Arc<AtomicBool>) -> Self {
        let (cancel, _) = watch::channel(false);
        Self {
            runtime,
            active,
            cancel,
            running: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    fn run(
        &self,
        sequence: &[Value],
        variables: Map<String, Value>,
        context: Context,
    ) -> Result<()> {
        if !self.active.load(Ordering::Acquire) {
            return Err(Error::Service(
                "template entity integration is shutting down".into(),
            ));
        }
        let guard = self.begin_action()?;
        let runtime = self.runtime.clone();
        let sequence = Value::Array(sequence.to_vec());
        let cancel = self.cancel.subscribe();
        let (reply, result) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("hass-template-action".into())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| {
                        Error::Service(format!("cannot start template action runtime: {error}"))
                    })
                    .and_then(|runtime_handle| {
                        runtime_handle
                            .block_on(run_sequence(runtime, sequence, variables, context, cancel))
                    });
                let _ = reply.send(result);
            })
            .map_err(|error| Error::Service(format!("cannot start template action: {error}")))?;
        let result = result
            .recv()
            .map_err(|_| Error::Service("template action worker stopped unexpectedly".into()))?;
        worker
            .join()
            .map_err(|_| Error::Service("template action worker panicked".into()))?;
        drop(guard);
        result
    }

    fn begin_action(&self) -> Result<ActiveActionGuard> {
        let (running, _) = &*self.running;
        let mut count = running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.active.load(Ordering::Acquire) {
            return Err(Error::Service(
                "template entity integration is shutting down".into(),
            ));
        }
        if *count >= ACTION_QUEUE_CAPACITY {
            return Err(Error::Service(
                "template entity action concurrency limit reached".into(),
            ));
        }
        *count += 1;
        Ok(ActiveActionGuard {
            running: Arc::clone(&self.running),
        })
    }

    async fn shutdown(&self) -> Result<()> {
        self.cancel_now();
        let running = Arc::clone(&self.running);
        tokio::task::spawn_blocking(move || {
            let (count, changed) = &*running;
            let mut count = count
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while *count > 0 {
                count = changed
                    .wait(count)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        })
        .await
        .map_err(|error| Error::Service(format!("joining template actions failed: {error}")))
    }

    fn cancel_now(&self) {
        self.active.store(false, Ordering::Release);
        self.cancel.send_replace(true);
    }
}

struct ActiveActionGuard {
    running: Arc<(Mutex<usize>, Condvar)>,
}

impl Drop for ActiveActionGuard {
    fn drop(&mut self) {
        let (count, changed) = &*self.running;
        let mut count = count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *count = count.saturating_sub(1);
        changed.notify_all();
    }
}

/// Running state-based template entities and their action services.
pub struct TemplateEntities {
    runtime: Runtime,
    owner: String,
    active: Arc<AtomicBool>,
    cancel: watch::Sender<bool>,
    updates: Option<JoinHandle<Result<()>>>,
    actions: ActionDispatcher,
    handles: Vec<EntityHandle>,
    lifecycle: EntityLifecycle,
    owns_lifecycle: bool,
}

impl TemplateEntities {
    /// Validate, register, and start the configured state-based template
    /// entities. The call returns after their initial states are written.
    pub async fn start(runtime: Runtime, blocks: Vec<TemplateBlock>) -> Result<Self> {
        let registries = Registries::new(runtime.clone())?;
        Self::start_with_registries(runtime, blocks, registries).await
    }

    /// Start template entities using the application's shared registries.
    /// This is the application integration path; [`Self::start`] is useful for
    /// isolated callers that do not already own a registry manager.
    pub async fn start_with_registries(
        runtime: Runtime,
        blocks: Vec<TemplateBlock>,
        registries: Registries,
    ) -> Result<Self> {
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries)?;
        Self::start_internal(runtime, blocks, lifecycle, true).await
    }

    /// Start with the application's shared entity lifecycle manager.
    pub async fn start_with_lifecycle(
        runtime: Runtime,
        blocks: Vec<TemplateBlock>,
        lifecycle: EntityLifecycle,
    ) -> Result<Self> {
        Self::start_internal(runtime, blocks, lifecycle, false).await
    }

    async fn start_internal(
        runtime: Runtime,
        blocks: Vec<TemplateBlock>,
        lifecycle: EntityLifecycle,
        owns_lifecycle: bool,
    ) -> Result<Self> {
        validate(&blocks)?;
        let (_, mut events) = runtime.subscribe_with_states();
        let engine = TemplateEngine::new(runtime.clone());
        let pending = flatten_blocks(&blocks);
        let entities = allocate_entity_ids(&runtime, &lifecycle, &engine, pending)?;
        let (ordered_entities, dependents) = build_refresh_plan(entities)?;
        let handles = ordered_entities
            .iter()
            .map(|entity| entity.handle.clone())
            .collect();
        let entity_ids = ordered_entities
            .iter()
            .map(|entity| entity.entity_id.clone())
            .collect::<HashSet<_>>();
        let entities = Arc::new(
            ordered_entities
                .iter()
                .cloned()
                .map(|entity| (entity.entity_id.clone(), entity))
                .collect::<HashMap<_, _>>(),
        );
        let owner = format!("{OWNER_PREFIX}-{}", Uuid::new_v4().simple());
        let active = Arc::new(AtomicBool::new(true));
        let actions = ActionDispatcher::new(runtime.clone(), Arc::clone(&active));
        if let Err(error) =
            register_entity_services(&runtime, &owner, Arc::clone(&entities), actions.clone())
        {
            active.store(false, Ordering::Release);
            let _ = runtime.unregister_entity_services(&owner);
            let _ = actions.shutdown().await;
            return Err(error);
        }
        if let Err(error) = write_entities(&runtime, &engine, &ordered_entities, Context::default())
        {
            active.store(false, Ordering::Release);
            let _ = runtime.unregister_entity_services(&owner);
            let _ = actions.shutdown().await;
            return Err(error);
        }
        let event_runtime = runtime.clone();
        let event_entities = ordered_entities;
        let event_dependents = dependents;
        let uses_clock = event_entities.iter().any(entity_uses_clock);
        let (cancel, mut cancel_rx) = watch::channel(false);
        let updates = tokio::spawn(async move {
            let period = Duration::from_secs(60);
            let mut clock = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let event = tokio::select! {
                    changed = cancel_rx.changed() => {
                        if changed.is_err() || *cancel_rx.borrow() {
                            return Ok(());
                        }
                        continue;
                    }
                    event = events.recv() => Some(event),
                    _ = clock.tick(), if uses_clock => None,
                };
                let (mut refresh_all, mut sources, mut context) = match event {
                    None => (true, BTreeSet::new(), Context::default()),
                    Some(Ok(event)) if event.event_type == "entity_registry_updated" => {
                        (true, BTreeSet::new(), event.context)
                    }
                    Some(Ok(event)) => {
                        let Some(entity_id) = state_changed_entity_id(&event) else {
                            continue;
                        };
                        if entity_ids.contains(entity_id) {
                            (false, BTreeSet::from([entity_id.to_owned()]), event.context)
                        } else {
                            (true, BTreeSet::new(), event.context)
                        }
                    }
                    Some(Err(broadcast::error::RecvError::Lagged(_))) => {
                        (true, BTreeSet::new(), Context::default())
                    }
                    Some(Err(broadcast::error::RecvError::Closed)) => return Ok(()),
                };
                let mut drained = 0usize;
                loop {
                    if drained >= EVENT_DRAIN_LIMIT {
                        break;
                    }
                    match events.try_recv() {
                        Ok(event) => {
                            if event.event_type == "entity_registry_updated" {
                                refresh_all = true;
                                context = event.context;
                                drained += 1;
                                continue;
                            }
                            if let Some(entity_id) = state_changed_entity_id(&event) {
                                if entity_ids.contains(entity_id) {
                                    sources.insert(entity_id.to_owned());
                                } else {
                                    refresh_all = true;
                                }
                                context = event.context;
                            }
                            drained += 1;
                        }
                        Err(broadcast::error::TryRecvError::Lagged(_)) => {
                            refresh_all = true;
                            context = Context::default();
                            break;
                        }
                        Err(broadcast::error::TryRecvError::Empty)
                        | Err(broadcast::error::TryRecvError::Closed) => break,
                    }
                }
                let definitions = if refresh_all {
                    event_entities.clone()
                } else {
                    let mut affected = BTreeSet::new();
                    for source in &sources {
                        collect_descendants(source, &event_dependents, &mut affected);
                    }
                    event_entities
                        .iter()
                        .filter(|entity| affected.contains(&entity.entity_id))
                        .cloned()
                        .collect::<Vec<_>>()
                };
                if definitions.is_empty() {
                    continue;
                }
                if let Err(error) = write_entities(
                    &event_runtime,
                    &TemplateEngine::new(event_runtime.clone()),
                    &definitions,
                    context,
                ) {
                    warn!(%error, "template entity refresh failed");
                }
                tokio::task::yield_now().await;
            }
        });

        Ok(Self {
            runtime,
            owner,
            active,
            cancel,
            updates: Some(updates),
            actions,
            handles,
            lifecycle,
            owns_lifecycle,
        })
    }

    /// Cancel event refreshes and action runs, unregister the provider, and
    /// join all owned work before returning.
    pub async fn shutdown(mut self) -> Result<()> {
        self.active.store(false, Ordering::Release);
        self.cancel.send_replace(true);
        let unregister_result = self.runtime.unregister_entity_services(&self.owner);
        let update_result = if let Some(updates) = self.updates.take() {
            match updates.await {
                Ok(result) => result,
                Err(error) => Err(Error::Service(format!(
                    "joining template refresh task failed: {error}"
                ))),
            }
        } else {
            Ok(())
        };
        let action_result = self.actions.shutdown().await;
        let mut lifecycle_result = if self.owns_lifecycle {
            self.lifecycle.shutdown().await
        } else {
            Ok(())
        };
        for handle in &self.handles {
            if let Err(error) = handle.release()
                && lifecycle_result.is_ok()
            {
                lifecycle_result = Err(error);
            }
        }
        let unregister_result = unregister_result.map(|_| ());
        update_result
            .and(action_result)
            .and(unregister_result)
            .and(lifecycle_result)
    }
}

impl Drop for TemplateEntities {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        self.cancel.send_replace(true);
        let _ = self.runtime.unregister_entity_services(&self.owner);
        self.actions.cancel_now();
        if let Some(updates) = self.updates.take() {
            updates.abort();
        }
    }
}

fn state_changed_entity_id(event: &Event) -> Option<&str> {
    (event.event_type == "state_changed")
        .then(|| event.data.get("entity_id").and_then(Value::as_str))
        .flatten()
}

type EntityDependents = HashMap<String, BTreeSet<String>>;
type RefreshPlan = (Vec<EntityDefinition>, EntityDependents);

fn build_refresh_plan(entities: Vec<EntityDefinition>) -> Result<RefreshPlan> {
    let entities_by_id = entities
        .iter()
        .cloned()
        .map(|entity| (entity.entity_id.clone(), entity))
        .collect::<HashMap<_, _>>();
    let entity_ids = entities_by_id.keys().cloned().collect::<BTreeSet<_>>();
    let mut dependents = HashMap::<String, BTreeSet<String>>::new();
    let mut indegree = entity_ids
        .iter()
        .map(|entity_id| (entity_id.clone(), 0usize))
        .collect::<HashMap<_, _>>();

    for entity in &entities {
        let sources = entity_template_sources(entity);
        for source_id in &entity_ids {
            if !sources
                .iter()
                .any(|source| references_entity(source, source_id))
            {
                continue;
            }
            if source_id == &entity.entity_id {
                return invalid(format!(
                    "template entity {} references its own state; reactive cycles are unsupported",
                    entity.entity_id
                ));
            }
            if dependents
                .entry(source_id.clone())
                .or_default()
                .insert(entity.entity_id.clone())
            {
                *indegree
                    .get_mut(&entity.entity_id)
                    .expect("every entity has an indegree entry") += 1;
            }
        }
    }

    let mut ready = indegree
        .iter()
        .filter_map(|(entity_id, count)| (*count == 0).then_some(entity_id.clone()))
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::with_capacity(entities.len());
    while let Some(entity_id) = ready.pop_first() {
        let entity = entities_by_id
            .get(&entity_id)
            .expect("topological order contains a configured entity")
            .clone();
        ordered.push(entity);
        if let Some(children) = dependents.get(&entity_id) {
            for child in children {
                let count = indegree
                    .get_mut(child)
                    .expect("every dependent has an indegree entry");
                *count -= 1;
                if *count == 0 {
                    ready.insert(child.clone());
                }
            }
        }
    }
    if ordered.len() != entities.len() {
        return invalid("template entity dependency cycle detected");
    }
    Ok((ordered, dependents))
}

fn collect_descendants(
    source: &str,
    dependents: &HashMap<String, BTreeSet<String>>,
    output: &mut BTreeSet<String>,
) {
    let mut pending = dependents
        .get(source)
        .into_iter()
        .flat_map(|children| children.iter().cloned())
        .collect::<Vec<_>>();
    while let Some(entity_id) = pending.pop() {
        if !output.insert(entity_id.clone()) {
            continue;
        }
        if let Some(children) = dependents.get(&entity_id) {
            pending.extend(children.iter().cloned());
        }
    }
}

fn entity_uses_clock(entity: &EntityDefinition) -> bool {
    entity_template_sources(entity).iter().any(|source| {
        let source = source.to_ascii_lowercase();
        source.contains("now(")
            || source.contains("now (")
            || source.contains("utcnow(")
            || source.contains("utcnow (")
    })
}

fn entity_template_sources(entity: &EntityDefinition) -> Vec<String> {
    let mut sources = Vec::new();
    for source in [
        entity.common.name.as_deref(),
        entity.common.availability.as_deref(),
        entity.common.icon.as_deref(),
        entity.common.picture.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        sources.push(source.to_owned());
    }
    for value in entity.common.variables.values() {
        if let Some(source) = value.as_str() {
            sources.push(source.to_owned());
        }
    }
    match &entity.common.attributes {
        Some(AttributeConfig::Template(source)) => sources.push(source.clone()),
        Some(AttributeConfig::PerAttribute(values)) => {
            sources.extend(values.values().cloned());
        }
        None => {}
    }
    match &entity.kind {
        EntityKind::Sensor { state, .. } | EntityKind::BinarySensor { state, .. } => {
            sources.push(state.clone());
        }
        EntityKind::Switch { state, .. } => sources.extend(state.iter().cloned()),
        EntityKind::Number {
            state,
            min,
            max,
            step,
            ..
        } => {
            sources.extend(state.iter().cloned());
            for value in [min, max, step].into_iter().flatten() {
                if let Some(source) = value.as_str() {
                    sources.push(source.to_owned());
                }
            }
        }
        EntityKind::Select { state, options, .. } => {
            sources.extend(state.iter().cloned());
            sources.push(options.clone());
        }
        EntityKind::Button { .. } => {}
    }
    sources
}

fn references_entity(source: &str, entity_id: &str) -> bool {
    source.match_indices(entity_id).any(|(start, _)| {
        let end = start + entity_id.len();
        let is_entity_character = |byte: u8| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_uppercase()
                || byte.is_ascii_digit()
                || byte == b'_'
                || byte == b'.'
        };
        let before_is_boundary = start == 0 || !is_entity_character(source.as_bytes()[start - 1]);
        let after_is_boundary = end == source.len() || !is_entity_character(source.as_bytes()[end]);
        before_is_boundary && after_is_boundary
    })
}

fn register_entity_services(
    runtime: &Runtime,
    owner: &str,
    entities: Arc<HashMap<String, EntityDefinition>>,
    actions: ActionDispatcher,
) -> Result<()> {
    let mut services = BTreeSet::new();
    for entity in entities.values() {
        match entity.kind {
            EntityKind::Switch { .. } => {
                services.insert(("switch", "turn_on"));
                services.insert(("switch", "turn_off"));
                services.insert(("switch", "toggle"));
            }
            EntityKind::Number { .. } => {
                services.insert(("number", "set_value"));
            }
            EntityKind::Select { .. } => {
                services.insert(("select", "select_option"));
            }
            EntityKind::Button { .. } => {
                services.insert(("button", "press"));
            }
            EntityKind::Sensor { .. } | EntityKind::BinarySensor { .. } => {}
        }
    }

    for (domain, service) in services {
        let matcher_entities = Arc::clone(&entities);
        let matcher: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(move |entity_id| {
            matcher_entities.values().any(|entity| {
                entity.handle.current_entity_id().as_deref() == Some(entity_id)
                    && supports_service(entity, domain, service)
            })
        });
        let callback_entities = Arc::clone(&entities);
        let callback_actions = actions.clone();
        let handler: ServiceHandler = Arc::new(move |runtime, call| {
            execute_entity_service(
                runtime,
                call,
                domain,
                service,
                &callback_entities,
                &callback_actions,
            )
        });
        runtime.register_entity_service(
            owner,
            domain,
            service,
            json!({ "description": format!("Run configured template {domain} action") }),
            matcher,
            handler,
        )?;
    }
    Ok(())
}

fn supports_service(entity: &EntityDefinition, domain: &str, service: &str) -> bool {
    matches!(
        (&entity.kind, domain, service),
        (
            EntityKind::Switch { .. },
            "switch",
            "turn_on" | "turn_off" | "toggle"
        ) | (EntityKind::Number { .. }, "number", "set_value")
            | (EntityKind::Select { .. }, "select", "select_option")
            | (EntityKind::Button { .. }, "button", "press")
    )
}

fn execute_entity_service(
    runtime: &Runtime,
    call: &ServiceCall,
    domain: &str,
    service: &str,
    entities: &HashMap<String, EntityDefinition>,
    actions: &ActionDispatcher,
) -> Result<ServiceResult> {
    let entity_ids = parse_target_entity_ids(call)?;
    let mut plans = Vec::with_capacity(entity_ids.len());
    for entity_id in entity_ids {
        let entity = entities
            .values()
            .filter_map(EntityDefinition::current)
            .find(|entity| entity.entity_id == entity_id)
            .ok_or_else(|| Error::NotFound(format!("unknown template entity {entity_id}")))?;
        if !supports_service(&entity, domain, service) {
            return invalid(format!(
                "service {domain}.{service} is not supported by {entity_id}"
            ));
        }
        plans.push((
            entity.handle.clone(),
            plan_entity_action(runtime, call, &entity, service)?,
        ));
    }

    let mut changed_states = Vec::new();
    for (handle, plan) in plans {
        if !plan.sequence.is_empty() {
            actions.run(&plan.sequence, plan.variables, call.context.clone())?;
        }
        if let Some(state) = plan.write_state {
            let attributes = runtime
                .get_state(&plan.entity_id)
                .map(|current| current.attributes)
                .unwrap_or_default();
            let result = handle.publish(EntitySnapshot {
                state: Some(state),
                available: true,
                attributes,
                context: call.context.clone(),
            })?;
            if let Some(result) = result
                && (result.created || result.changed)
            {
                changed_states.push(result.state);
            }
        }
    }
    Ok(ServiceResult {
        changed_states,
        response: None,
        context: call.context.clone(),
    })
}

struct EntityActionPlan {
    entity_id: String,
    sequence: Vec<Value>,
    variables: Map<String, Value>,
    write_state: Option<String>,
}

fn plan_entity_action(
    runtime: &Runtime,
    call: &ServiceCall,
    entity: &EntityDefinition,
    service: &str,
) -> Result<EntityActionPlan> {
    let mut variables = entity.common.variables.clone();
    let mut write_state = None;
    let sequence = match &entity.kind {
        EntityKind::Switch {
            turn_on,
            turn_off,
            optimistic,
            ..
        } => {
            let current_is_on = runtime
                .get_state(&entity.entity_id)
                .is_some_and(|state| state.state == "on");
            let turn_on_action = match service {
                "turn_on" => true,
                "turn_off" => false,
                "toggle" => !current_is_on,
                _ => return invalid("invalid template switch service"),
            };
            if *optimistic {
                write_state = Some(if turn_on_action { "on" } else { "off" }.into());
            }
            if turn_on_action {
                turn_on.clone().unwrap_or_default()
            } else {
                turn_off.clone().unwrap_or_default()
            }
        }
        EntityKind::Number {
            set_value,
            optimistic,
            ..
        } => {
            let Some(value) = call.data.get("value").and_then(Value::as_f64) else {
                return invalid("number.set_value requires a numeric value");
            };
            if !value.is_finite() {
                return invalid("number.set_value value must be finite");
            }
            validate_number_target(runtime, &entity.entity_id, value)?;
            variables.insert(
                "value".into(),
                Number::from_f64(value).map(Value::Number).ok_or_else(|| {
                    Error::InvalidInput("number.set_value value is invalid".into())
                })?,
            );
            if *optimistic {
                write_state = Some(number_string(value));
            }
            set_value.clone()
        }
        EntityKind::Select {
            select_option,
            optimistic,
            ..
        } => {
            let Some(option) = call.data.get("option").and_then(Value::as_str) else {
                return invalid("select.select_option requires an option string");
            };
            validate_select_option(runtime, &entity.entity_id, option)?;
            variables.insert("option".into(), Value::String(option.to_owned()));
            if *optimistic {
                write_state = Some(option.to_owned());
            }
            select_option.clone().unwrap_or_default()
        }
        EntityKind::Button { press, .. } if service == "press" => {
            write_state = Some(Utc::now().to_rfc3339());
            press.clone()
        }
        _ => return invalid(format!("unsupported template entity service {service}")),
    };
    Ok(EntityActionPlan {
        entity_id: entity.entity_id.clone(),
        sequence,
        variables,
        write_state,
    })
}

fn parse_target_entity_ids(call: &ServiceCall) -> Result<Vec<String>> {
    let target = call
        .target
        .get("entity_id")
        .or_else(|| call.data.get("entity_id"))
        .ok_or_else(|| {
            Error::InvalidInput("template entity action requires entity_id target".into())
        })?;
    let entities = match target {
        Value::String(entity_id) => vec![entity_id.clone()],
        Value::Array(values) => values
            .iter()
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    Error::InvalidInput("entity_id target list must contain strings".into())
                })
            })
            .collect::<Result<Vec<_>>>()?,
        _ => return invalid("entity_id target must be a string or list of strings"),
    };
    if entities.is_empty() {
        return invalid("entity_id target list cannot be empty");
    }
    Ok(entities)
}

fn validate_number_target(runtime: &Runtime, entity_id: &str, value: f64) -> Result<()> {
    let Some(state) = runtime.get_state(entity_id) else {
        return Ok(());
    };
    let minimum = state.attributes.get("min").and_then(Value::as_f64);
    let maximum = state.attributes.get("max").and_then(Value::as_f64);
    if minimum.is_some_and(|minimum| value < minimum)
        || maximum.is_some_and(|maximum| value > maximum)
    {
        return invalid("number.set_value value is outside the template number range");
    }
    Ok(())
}

fn validate_select_option(runtime: &Runtime, entity_id: &str, option: &str) -> Result<()> {
    if let Some(options) = runtime
        .get_state(entity_id)
        .and_then(|state| state.attributes.get("options").cloned())
        .and_then(|value| value.as_array().cloned())
        && !options.iter().any(|value| value.as_str() == Some(option))
    {
        return invalid("select option is not in the template select options");
    }
    Ok(())
}

fn number_string(value: f64) -> String {
    Number::from_f64(value)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn allocate_entity_ids(
    runtime: &Runtime,
    lifecycle: &EntityLifecycle,
    engine: &TemplateEngine,
    pending: Vec<PendingEntity>,
) -> Result<Vec<EntityDefinition>> {
    let persisted: BTreeMap<String, String> = match runtime.get_data(ENTITY_ID_MAP_KEY)? {
        Some(value) => serde_json::from_value(value).map_err(|error| {
            Error::Storage(format!("invalid template entity ID metadata: {error}"))
        })?,
        None => BTreeMap::new(),
    };
    let mut specs = Vec::with_capacity(pending.len());
    for pending_entity in &pending {
        let rendered_name = render_name(engine, &pending_entity.common, pending_entity.domain);
        let suggested = pending_entity
            .common
            .default_entity_id
            .as_deref()
            .and_then(|entity_id| {
                entity_id
                    .split_once('.')
                    .map(|(_, object)| object.to_owned())
            })
            .unwrap_or_else(|| slugify(&rendered_name));
        let info = ProviderEntityInfo {
            original_name: static_name(&pending_entity.common, pending_entity.domain),
            original_icon: pending_entity
                .common
                .icon
                .as_ref()
                .filter(|icon| !icon.contains("{{") && !icon.contains("{%"))
                .cloned(),
            original_device_class: pending_entity.kind.device_class().map(str::to_owned),
            unit_of_measurement: pending_entity.kind.unit_of_measurement().map(str::to_owned),
            ..Default::default()
        };
        let mut spec = EntitySpec::new(
            pending_entity.domain,
            "template",
            "template",
            &pending_entity.config_key,
        )
        .with_suggested_object_id(suggested)
        .with_info(info);
        if let Some(unique_id) = &pending_entity.common.unique_id {
            spec = spec.with_unique_id(unique_id);
        } else if let Some(prior_id) = persisted.get(&pending_entity.config_key) {
            spec = spec.with_prior_entity_id(prior_id);
        }
        specs.push(spec);
    }
    let handles = lifecycle.register_provider_batch("template", specs)?;
    Ok(pending
        .into_iter()
        .zip(handles)
        .map(|(pending_entity, handle)| EntityDefinition {
            entity_id: handle.status().entity_id().to_owned(),
            handle,
            domain: pending_entity.domain,
            common: pending_entity.common,
            kind: pending_entity.kind,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::MemoryStore;
    use hass_registry::EntityRegistration;

    fn runtime() -> (Runtime, Arc<MemoryStore>) {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).unwrap();
        (runtime, store)
    }

    fn blocks(yaml: &str) -> Vec<TemplateBlock> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RootConfig {
            template: Vec<TemplateBlock>,
        }

        serde_yaml::from_str::<RootConfig>(yaml).unwrap().template
    }

    fn set(runtime: &Runtime, entity_id: &str, state: &str) {
        runtime
            .set_state(entity_id, state, Map::new(), Context::default(), false)
            .unwrap();
    }

    async fn wait_for_state(runtime: &Runtime, entity_id: &str, expected: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if runtime
                    .get_state(entity_id)
                    .is_some_and(|state| state.state == expected)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{entity_id} did not reach {expected:?}"));
    }

    fn call(domain: &str, service: &str, entity_id: &str, data: Value) -> ServiceCall {
        ServiceCall {
            domain: domain.into(),
            service: service.into(),
            data: data.as_object().cloned().unwrap_or_default(),
            target: json!({"entity_id": entity_id})
                .as_object()
                .cloned()
                .unwrap(),
            context: Context::default(),
        }
    }

    fn register_capture_service(runtime: &Runtime) -> Arc<Mutex<Vec<ServiceCall>>> {
        let calls = Arc::new(Mutex::new(Vec::<ServiceCall>::new()));
        let handler_calls = Arc::clone(&calls);
        runtime
            .register_service(
                "test",
                "capture",
                json!({"description": "Capture a template action"}),
                Arc::new(move |_runtime, call| {
                    handler_calls
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(call.clone());
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: None,
                        context: call.context.clone(),
                    })
                }),
            )
            .unwrap();
        calls
    }

    #[test]
    fn modern_root_schema_is_strict_and_trigger_blocks_are_explicitly_unsupported() {
        let parsed = blocks(
            "template:\n  - sensor:\n      - name: Room temperature\n        unique_id: room_temp\n        state: \"{{ states('sensor.room') }}\"\n",
        );
        validate(&parsed).unwrap();
        let single_entity: Vec<TemplateBlock> =
            serde_yaml::from_str("- sensor: {name: Single, state: 'literal'}\n").unwrap();
        assert_eq!(single_entity[0].sensor.len(), 1);
        assert!(
            serde_yaml::from_str::<Vec<TemplateBlock>>("- sensor:\n    - name: Missing state\n")
                .is_err()
        );
        assert!(serde_yaml::from_str::<Vec<TemplateBlock>>("- light: []\n").is_err());
        assert!(
            serde_yaml::from_str::<Vec<TemplateBlock>>(
                "- sensor:\n    - name: Strict\n      state: '{{ 1 }}'\n      made_up: true\n"
            )
            .is_err()
        );

        let trigger = blocks("template:\n  - triggers: []\n");
        assert!(
            validate(&trigger)
                .unwrap_err()
                .to_string()
                .contains("trigger-based")
        );
    }

    #[tokio::test]
    async fn template_sensor_uses_literal_metadata_reacts_and_persists_unique_id() {
        let (runtime, _store) = runtime();
        set(&runtime, "counter.presses", "0");
        let registry = Registries::new(runtime.clone()).unwrap();
        let config = blocks(
            "template:\n  - sensor:\n      - name: Doubled\n        unique_id: doubled-presses\n        state: \"{{ states('counter.presses') | int(0) * 2 }}\"\n        icon: mdi:counter\n        device_class: temperature\n        state_class: measurement\n        unit_of_measurement: '°C'\n        attributes:\n          source: \"{{ states('counter.presses') }}\"\n",
        );
        let entities =
            TemplateEntities::start_with_registries(runtime.clone(), config, registry.clone())
                .await
                .unwrap();

        let state = runtime.get_state("sensor.doubled").unwrap();
        assert_eq!(state.state, "0");
        assert_eq!(state.attributes["friendly_name"], "Doubled");
        assert_eq!(state.attributes["icon"], "mdi:counter");
        assert_eq!(state.attributes["device_class"], "temperature");
        assert_eq!(state.attributes["state_class"], "measurement");
        assert_eq!(state.attributes["unit_of_measurement"], "°C");
        assert_eq!(state.attributes["source"], "0");
        let entry = registry
            .entities()
            .into_iter()
            .find(|entry| entry.unique_id == "doubled-presses")
            .unwrap();
        assert_eq!(entry.entity_id, "sensor.doubled");

        set(&runtime, "counter.presses", "1");
        wait_for_state(&runtime, "sensor.doubled", "2").await;
        assert_eq!(
            runtime.get_state("sensor.doubled").unwrap().attributes["source"],
            "1"
        );
        entities.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn registry_overrides_rename_disable_and_remove_follow_live_provider() {
        let (runtime, _store) = runtime();
        set(&runtime, "sensor.source", "7");
        let registry = Registries::new(runtime.clone()).unwrap();
        let config = blocks(
            "template:\n  - sensor:\n      - name: Provider name\n        unique_id: lifecycle-sensor\n        icon: mdi:thermometer\n        device_class: temperature\n        state: \"{{ states('sensor.source') }}\"\n",
        );
        let entities =
            TemplateEntities::start_with_registries(runtime.clone(), config, registry.clone())
                .await
                .unwrap();
        registry
            .update_entity(
                "sensor.provider_name",
                hass_registry::EntityUpdate {
                    name: Some(Some("User name".into())),
                    icon: Some(Some("mdi:home".into())),
                    device_class: Some(Some("humidity".into())),
                    new_entity_id: Some("sensor.renamed_provider".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        wait_for_state(&runtime, "sensor.renamed_provider", "7").await;
        assert!(runtime.get_state("sensor.provider_name").is_none());
        let state = runtime.get_state("sensor.renamed_provider").unwrap();
        assert_eq!(state.attributes["friendly_name"], "User name");
        assert_eq!(state.attributes["icon"], "mdi:home");
        assert_eq!(state.attributes["device_class"], "humidity");
        set(&runtime, "sensor.source", "8");
        wait_for_state(&runtime, "sensor.renamed_provider", "8").await;
        registry
            .update_entity(
                "sensor.renamed_provider",
                hass_registry::EntityUpdate {
                    disabled_by: Some(Some("user".into())),
                    ..Default::default()
                },
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while runtime.get_state("sensor.renamed_provider").is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        set(&runtime, "sensor.source", "9");
        registry
            .update_entity(
                "sensor.renamed_provider",
                hass_registry::EntityUpdate {
                    disabled_by: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        wait_for_state(&runtime, "sensor.renamed_provider", "9").await;
        registry.remove_entity("sensor.renamed_provider").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while runtime.get_state("sensor.renamed_provider").is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        set(&runtime, "sensor.source", "10");
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(runtime.get_state("sensor.renamed_provider").is_none());
        assert!(runtime.get_state("sensor.provider_name").is_none());
        assert!(
            registry
                .entities()
                .iter()
                .all(|entry| entry.unique_id != "lifecycle-sensor")
        );
        entities.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn action_service_follows_registry_rename_and_disabled_identity() {
        let (runtime, _store) = runtime();
        let registry = Registries::new(runtime.clone()).unwrap();
        let captured = register_capture_service(&runtime);
        let config = blocks(
            "template:\n  - button:\n      - name: Managed button\n        unique_id: managed-button\n        press:\n          - action: test.capture\n            data: {control: managed_press}\n",
        );
        let entities =
            TemplateEntities::start_with_registries(runtime.clone(), config, registry.clone())
                .await
                .unwrap();
        registry
            .update_entity(
                "button.managed_button",
                hass_registry::EntityUpdate {
                    new_entity_id: Some("button.renamed_button".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        wait_for_state(&runtime, "button.renamed_button", "unknown").await;
        runtime
            .call_service(call("button", "press", "button.renamed_button", json!({})))
            .unwrap();
        assert_eq!(captured.lock().unwrap().len(), 1);
        assert!(runtime.get_state("button.managed_button").is_none());
        registry
            .update_entity(
                "button.renamed_button",
                hass_registry::EntityUpdate {
                    disabled_by: Some(Some("user".into())),
                    ..Default::default()
                },
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while runtime.get_state("button.renamed_button").is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let _ = runtime.call_service(call("button", "press", "button.renamed_button", json!({})));
        assert_eq!(
            captured.lock().unwrap().len(),
            1,
            "disabled providers must not receive actions"
        );
        entities.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn disabled_registry_entity_stays_absent_during_provider_setup() {
        let (runtime, _store) = runtime();
        let registry = Registries::new(runtime.clone()).unwrap();
        let mut registration = EntityRegistration::new("sensor", "template", "disabled-at-setup");
        registration.suggested_object_id = Some("disabled_at_setup".into());
        registration.disabled_by = Some("user".into());
        registry.get_or_create_entity(registration).unwrap();
        let config = blocks(
            "template:\n  - sensor:\n      - name: Disabled at setup\n        unique_id: disabled-at-setup\n        state: '42'\n",
        );
        let entities =
            TemplateEntities::start_with_registries(runtime.clone(), config, registry.clone())
                .await
                .unwrap();
        assert!(runtime.get_state("sensor.disabled_at_setup").is_none());
        registry
            .update_entity(
                "sensor.disabled_at_setup",
                hass_registry::EntityUpdate {
                    disabled_by: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        wait_for_state(&runtime, "sensor.disabled_at_setup", "42").await;
        entities.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn generated_entity_ids_without_unique_ids_survive_restart() {
        let (runtime, _store) = runtime();
        set(&runtime, "sensor.source", "first");
        let config = blocks(
            "template:\n  - sensor:\n      - name: Stable name\n        state: \"{{ states('sensor.source') }}\"\n",
        );
        let first = TemplateEntities::start(runtime.clone(), config.clone())
            .await
            .unwrap();
        assert_eq!(
            runtime.get_state("sensor.stable_name").unwrap().state,
            "first"
        );
        first.shutdown().await.unwrap();

        let second = TemplateEntities::start(runtime.clone(), config)
            .await
            .unwrap();
        assert_eq!(
            runtime.get_state("sensor.stable_name").unwrap().state,
            "first"
        );
        second.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn state_errors_missing_values_and_false_availability_never_become_zero() {
        let (runtime, _store) = runtime();
        set(&runtime, "sensor.value", "42");
        set(&runtime, "sensor.raw_numeric", "not-a-number");
        set(&runtime, "switch.guard", "off");
        let config = blocks(
            "template:\n  - sensor:\n      - name: Missing source\n        unique_id: missing-source\n        state: \"{{ states('sensor.absent') }}\"\n      - name: Broken expression\n        unique_id: broken-expression\n        state: '{{ 1 + }}'\n      - name: Gated value\n        unique_id: gated-value\n        state: \"{{ states('sensor.value') }}\"\n        availability: \"{{ is_state('switch.guard', 'on') }}\"\n      - name: Numeric source\n        unique_id: numeric-source\n        state: \"{{ states('sensor.raw_numeric') }}\"\n        unit_of_measurement: '°C'\n",
        );
        let entities = TemplateEntities::start(runtime.clone(), config)
            .await
            .unwrap();
        assert_eq!(
            runtime.get_state("sensor.missing_source").unwrap().state,
            "unknown"
        );
        assert_eq!(
            runtime.get_state("sensor.broken_expression").unwrap().state,
            "unavailable"
        );
        assert_eq!(
            runtime.get_state("sensor.gated_value").unwrap().state,
            "unavailable"
        );
        assert_eq!(
            runtime.get_state("sensor.numeric_source").unwrap().state,
            "unavailable"
        );

        set(&runtime, "sensor.value", "43");
        set(&runtime, "switch.guard", "on");
        wait_for_state(&runtime, "sensor.gated_value", "43").await;
        set(&runtime, "sensor.raw_numeric", "0");
        wait_for_state(&runtime, "sensor.numeric_source", "0").await;
        entities.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn derived_sensor_chains_refresh_in_dependency_order() {
        let (runtime, _store) = runtime();
        set(&runtime, "counter.source", "1");
        let config = blocks(
            "template:\n  - sensor:\n      - name: B downstream\n        unique_id: b-downstream\n        state: \"{{ states('sensor.a_upstream') }}\"\n      - name: A upstream\n        unique_id: a-upstream\n        state: \"{{ states('counter.source') | int(0) * 2 }}\"\n",
        );
        let entities = TemplateEntities::start(runtime.clone(), config)
            .await
            .unwrap();
        assert_eq!(runtime.get_state("sensor.a_upstream").unwrap().state, "2");
        assert_eq!(runtime.get_state("sensor.b_downstream").unwrap().state, "2");

        set(&runtime, "counter.source", "2");
        wait_for_state(&runtime, "sensor.b_downstream", "4").await;
        assert_eq!(runtime.get_state("sensor.a_upstream").unwrap().state, "4");
        entities.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn clock_templates_refresh_each_minute() {
        let (runtime, _store) = runtime();
        let config = blocks(
            "template:\n  - sensor:\n      - name: Clock sensor\n        unique_id: clock-sensor\n        state: \"{{ now() }}\"\n",
        );
        let entities = TemplateEntities::start(runtime.clone(), config)
            .await
            .unwrap();
        tokio::task::yield_now().await;
        let before = runtime.get_state("sensor.clock_sensor").unwrap();
        tokio::time::advance(Duration::from_secs(60)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let after = runtime.get_state("sensor.clock_sensor").unwrap();
        assert!(after.last_reported > before.last_reported);
        entities.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn switch_number_select_and_button_run_real_runtime_service_actions() {
        let (runtime, _store) = runtime();
        let captured = register_capture_service(&runtime);
        set(&runtime, "sensor.number_report", "3");
        set(&runtime, "sensor.select_report", "eco");
        let config = blocks(
            "template:\n  - switch:\n      - name: Test switch\n        unique_id: action-switch\n        state: 'off'\n        icon: mdi:toggle-switch\n        turn_on:\n          - action: test.capture\n            data: {control: switch_on}\n    number:\n      - name: Test number\n        unique_id: action-number\n        state: \"{{ states('sensor.number_report') }}\"\n        set_value:\n          - action: test.capture\n            data: {control: number_set, requested: \"{{ value }}\"}\n    select:\n      - name: Test select\n        unique_id: action-select\n        options: |\n          eco\n          comfort\n        state: 'eco'\n        select_option:\n          - action: test.capture\n            data: {control: select_option, requested: \"{{ option }}\"}\n    button:\n      - name: Test button\n        unique_id: action-button\n        press:\n          - action: test.capture\n            data: {control: button_press}\n",
        );
        let registry = Registries::new(runtime.clone()).unwrap();
        let entities = TemplateEntities::start_with_registries(
            runtime.clone(),
            config.clone(),
            registry.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            runtime.get_state("switch.test_switch").unwrap().state,
            "off"
        );
        assert_eq!(
            runtime.get_state("switch.test_switch").unwrap().attributes["icon"],
            "mdi:toggle-switch"
        );
        assert_eq!(runtime.get_state("number.test_number").unwrap().state, "3");
        assert_eq!(
            runtime.get_state("select.test_select").unwrap().state,
            "eco"
        );

        runtime
            .call_service(call("switch", "turn_on", "switch.test_switch", json!({})))
            .unwrap();
        runtime
            .call_service(call(
                "number",
                "set_value",
                "number.test_number",
                json!({"value": 8.5}),
            ))
            .unwrap();
        runtime
            .call_service(call(
                "select",
                "select_option",
                "select.test_select",
                json!({"option": "comfort"}),
            ))
            .unwrap();
        runtime
            .call_service(call("button", "press", "button.test_button", json!({})))
            .unwrap();

        let calls = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(calls.len(), 4);
        let controls = calls
            .iter()
            .map(|call| call.data["control"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            controls,
            BTreeSet::from(["switch_on", "number_set", "select_option", "button_press"])
        );
        assert_eq!(calls[1].data["requested"], 8.5);
        assert_eq!(calls[2].data["requested"], "comfort");
        assert_eq!(
            runtime.get_state("switch.test_switch").unwrap().state,
            "off"
        );
        assert_eq!(runtime.get_state("number.test_number").unwrap().state, "3");
        assert_eq!(
            runtime.get_state("select.test_select").unwrap().state,
            "eco"
        );
        assert!(
            runtime
                .get_state("button.test_button")
                .unwrap()
                .state
                .parse::<chrono::DateTime<Utc>>()
                .is_ok()
        );

        entities.shutdown().await.unwrap();
        let services = runtime.services();
        for (domain, service) in [
            ("switch", "turn_on"),
            ("number", "set_value"),
            ("select", "select_option"),
            ("button", "press"),
        ] {
            assert!(
                services
                    .get(domain)
                    .and_then(Value::as_object)
                    .is_none_or(|services| !services.contains_key(service))
            );
        }

        let restarted = TemplateEntities::start_with_registries(runtime.clone(), config, registry)
            .await
            .unwrap();
        runtime
            .call_service(call("button", "press", "button.test_button", json!({})))
            .unwrap();
        assert_eq!(captured.lock().unwrap().len(), 5);
        restarted.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_stops_refreshes_and_releases_runtime_store_lease() {
        let (runtime, store) = runtime();
        set(&runtime, "sensor.source", "first");
        let config = blocks(
            "template:\n  - sensor:\n      - name: Snapshot\n        unique_id: snapshot\n        state: \"{{ states('sensor.source') }}\"\n",
        );
        let entities = TemplateEntities::start(runtime.clone(), config)
            .await
            .unwrap();
        let previous = runtime.get_state("sensor.snapshot").unwrap();
        entities.shutdown().await.unwrap();
        set(&runtime, "sensor.source", "second");
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            runtime.get_state("sensor.snapshot").unwrap().last_updated,
            previous.last_updated
        );

        drop(runtime);
        let reopened = Runtime::new(store.clone()).unwrap();
        assert_eq!(
            reopened.get_state("sensor.snapshot").unwrap().state,
            "first"
        );
        drop(reopened);
        let _store_events = store.events().unwrap();
    }

    #[tokio::test]
    async fn drop_fallback_unregisters_services_and_releases_runtime_store_lease() {
        let (runtime, store) = runtime();
        let config = blocks(
            "template:\n  - button:\n      - name: Drop button\n        unique_id: drop-button\n        press: []\n",
        );
        let entities = TemplateEntities::start(runtime.clone(), config)
            .await
            .unwrap();
        assert!(
            runtime
                .services()
                .get("button")
                .and_then(Value::as_object)
                .is_some_and(|services| services.contains_key("press"))
        );

        drop(entities);
        assert!(
            runtime
                .services()
                .get("button")
                .and_then(Value::as_object)
                .is_none_or(|services| !services.contains_key("press"))
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(runtime);

        let reopened = Runtime::new(store).unwrap();
        assert!(reopened.get_state("button.drop_button").is_some());
    }

    #[tokio::test]
    async fn reactive_dependency_cycles_are_rejected_before_services_start() {
        let (runtime, _store) = runtime();
        let config = blocks(
            "template:\n  - sensor:\n      - name: A\n        unique_id: cycle-a\n        state: \"{{ states('sensor.b') }}\"\n      - name: B\n        unique_id: cycle-b\n        state: \"{{ states('sensor.a') }}\"\n",
        );
        let error = match TemplateEntities::start(runtime.clone(), config).await {
            Ok(_) => panic!("dependency cycle unexpectedly started"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("dependency cycle"));
        assert!(runtime.get_state("sensor.a").is_none());
        assert!(runtime.get_state("sensor.b").is_none());
    }
}
