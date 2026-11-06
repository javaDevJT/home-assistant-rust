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
//! `option` for `input_select.select_option`, `options` for `set_options`, and
//! optional `cycle` for select next/previous. Every service call needs explicit
//! `entity_id` targets. These helpers only store state in the Rust runtime; they
//! do not control devices.

use hass_core::{Context, Error, Result, Runtime, ServiceCall, ServiceHandler, ServiceResult};
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};

/// All configured native helpers, keyed by object ID in each domain.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Helpers {
    #[serde(default, deserialize_with = "deserialize_nullable_helper_map")]
    pub input_boolean: BTreeMap<String, InputBooleanConfig>,
    #[serde(default)]
    pub input_number: BTreeMap<String, InputNumberConfig>,
    #[serde(default, deserialize_with = "deserialize_nullable_helper_map")]
    pub input_text: BTreeMap<String, InputTextConfig>,
    #[serde(default)]
    pub input_select: BTreeMap<String, InputSelectConfig>,
    #[serde(default, deserialize_with = "deserialize_nullable_helper_map")]
    pub counter: BTreeMap<String, CounterConfig>,
}

/// Configuration for one `input_boolean` helper.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputBooleanConfig {
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_icon")]
    pub icon: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_bool")]
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
#[serde(deny_unknown_fields)]
pub struct InputNumberConfig {
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_icon")]
    pub icon: Option<String>,
    #[serde(deserialize_with = "deserialize_coerce_f64")]
    pub min: f64,
    #[serde(deserialize_with = "deserialize_coerce_f64")]
    pub max: f64,
    #[serde(
        default = "default_number_step",
        deserialize_with = "deserialize_coerce_f64"
    )]
    pub step: f64,
    #[serde(default, deserialize_with = "deserialize_optional_f64")]
    pub initial: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub unit_of_measurement: Option<String>,
    #[serde(default)]
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

fn default_number_step() -> f64 {
    1.0
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
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_icon")]
    pub icon: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub initial: Option<String>,
    #[serde(
        default = "default_text_min",
        deserialize_with = "deserialize_coerce_usize"
    )]
    pub min: usize,
    #[serde(
        default = "default_text_max",
        deserialize_with = "deserialize_coerce_usize"
    )]
    pub max: usize,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub unit_of_measurement: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub pattern: Option<String>,
    #[serde(default)]
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
            pattern: None,
            mode: TextMode::Text,
        }
    }
}

fn default_text_min() -> usize {
    0
}

fn default_text_max() -> usize {
    100
}

/// Configuration for one `input_select` helper.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputSelectConfig {
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_icon")]
    pub icon: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub options: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub initial: Option<String>,
}

/// Configuration for one `counter` helper.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CounterConfig {
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_icon")]
    pub icon: Option<String>,
    #[serde(default, deserialize_with = "deserialize_coerce_i64")]
    pub initial: i64,
    #[serde(
        default = "default_counter_step",
        deserialize_with = "deserialize_coerce_i64"
    )]
    pub step: i64,
    #[serde(default, deserialize_with = "deserialize_optional_nullable_i64")]
    pub minimum: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_optional_nullable_i64")]
    pub maximum: Option<i64>,
    #[serde(
        default = "default_counter_restore",
        deserialize_with = "deserialize_coerce_bool"
    )]
    pub restore: bool,
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
            restore: true,
        }
    }
}

fn default_counter_restore() -> bool {
    true
}

fn default_counter_step() -> i64 {
    1
}

fn deserialize_nullable_helper_map<'de, D, T>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    let configs = BTreeMap::<String, Option<T>>::deserialize(deserializer)?;
    Ok(configs
        .into_iter()
        .map(|(object_id, config)| (object_id, config.unwrap_or_default()))
        .collect())
}

struct CoerceString(String);
struct CoerceBool(bool);
struct CoerceF64(f64);
struct CoerceI64(i64);
struct CoerceUsize(usize);

impl<'de> Deserialize<'de> for CoerceString {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StringVisitor;

        impl Visitor<'_> for StringVisitor {
            type Value = CoerceString;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a scalar value that can be converted to a string")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(CoerceString(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
                Ok(CoerceString(value))
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
                Ok(CoerceString(if value { "True" } else { "False" }.into()))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
                Ok(CoerceString(value.to_string()))
            }

            fn visit_i128<E>(self, value: i128) -> std::result::Result<Self::Value, E> {
                Ok(CoerceString(value.to_string()))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
                Ok(CoerceString(value.to_string()))
            }

            fn visit_u128<E>(self, value: u128) -> std::result::Result<Self::Value, E> {
                Ok(CoerceString(value.to_string()))
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E> {
                let mut value = value.to_string();
                if !value.contains(['.', 'e', 'E']) {
                    value.push_str(".0");
                }
                Ok(CoerceString(value))
            }
        }

        deserializer.deserialize_any(StringVisitor)
    }
}

impl<'de> Deserialize<'de> for CoerceBool {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoolVisitor;

        impl Visitor<'_> for BoolVisitor {
            type Value = CoerceBool;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a boolean, number, or Home Assistant boolean string")
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
                Ok(CoerceBool(value))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
                Ok(CoerceBool(value != 0))
            }

            fn visit_i128<E>(self, value: i128) -> std::result::Result<Self::Value, E> {
                Ok(CoerceBool(value != 0))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
                Ok(CoerceBool(value != 0))
            }

            fn visit_u128<E>(self, value: u128) -> std::result::Result<Self::Value, E> {
                Ok(CoerceBool(value != 0))
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E> {
                Ok(CoerceBool(value != 0.0))
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value.trim().to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" | "on" | "enable" => Ok(CoerceBool(true)),
                    "0" | "false" | "no" | "off" | "disable" => Ok(CoerceBool(false)),
                    _ => Err(E::custom(format!("invalid boolean value {value}"))),
                }
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(&value)
            }
        }

        deserializer.deserialize_any(BoolVisitor)
    }
}

impl<'de> Deserialize<'de> for CoerceF64 {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct F64Visitor;

        impl Visitor<'_> for F64Visitor {
            type Value = CoerceF64;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a number or scalar string convertible to a number")
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E> {
                Ok(CoerceF64(value))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
                Ok(CoerceF64(value as f64))
            }

            fn visit_i128<E>(self, value: i128) -> std::result::Result<Self::Value, E> {
                Ok(CoerceF64(value as f64))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
                Ok(CoerceF64(value as f64))
            }

            fn visit_u128<E>(self, value: u128) -> std::result::Result<Self::Value, E> {
                Ok(CoerceF64(value as f64))
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
                Ok(CoerceF64(if value { 1.0 } else { 0.0 }))
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                value
                    .trim()
                    .parse::<f64>()
                    .map(CoerceF64)
                    .map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(&value)
            }
        }

        deserializer.deserialize_any(F64Visitor)
    }
}

impl<'de> Deserialize<'de> for CoerceI64 {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct I64Visitor;

        impl Visitor<'_> for I64Visitor {
            type Value = CoerceI64;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an integer or scalar convertible to an integer")
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
                Ok(CoerceI64(value))
            }

            fn visit_i128<E>(self, value: i128) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                i64::try_from(value)
                    .map(CoerceI64)
                    .map_err(|_| E::custom("integer is outside the supported range"))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                i64::try_from(value)
                    .map(CoerceI64)
                    .map_err(|_| E::custom("integer is outside the supported range"))
            }

            fn visit_u128<E>(self, value: u128) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                i64::try_from(value)
                    .map(CoerceI64)
                    .map_err(|_| E::custom("integer is outside the supported range"))
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.is_finite() && value >= i64::MIN as f64 && value < i64::MAX as f64 {
                    Ok(CoerceI64(value.trunc() as i64))
                } else {
                    Err(E::custom("number is outside the supported integer range"))
                }
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
                Ok(CoerceI64(i64::from(value)))
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                value
                    .trim()
                    .parse::<i64>()
                    .map(CoerceI64)
                    .map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(&value)
            }
        }

        deserializer.deserialize_any(I64Visitor)
    }
}

impl<'de> Deserialize<'de> for CoerceUsize {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        CoerceI64::deserialize(deserializer).and_then(|value| {
            usize::try_from(value.0)
                .map(CoerceUsize)
                .map_err(de::Error::custom)
        })
    }
}

fn deserialize_optional_string<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    CoerceString::deserialize(deserializer).map(|value| Some(value.0))
}

fn deserialize_optional_icon<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    CoerceString::deserialize(deserializer).map(|value| Some(value.0.trim().to_owned()))
}

fn deserialize_optional_bool<'de, D>(deserializer: D) -> std::result::Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    CoerceBool::deserialize(deserializer).map(|value| Some(value.0))
}

fn deserialize_coerce_bool<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    CoerceBool::deserialize(deserializer).map(|value| value.0)
}

fn deserialize_coerce_f64<'de, D>(deserializer: D) -> std::result::Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    CoerceF64::deserialize(deserializer).map(|value| value.0)
}

fn deserialize_optional_f64<'de, D>(deserializer: D) -> std::result::Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    CoerceF64::deserialize(deserializer).map(|value| Some(value.0))
}

fn deserialize_coerce_i64<'de, D>(deserializer: D) -> std::result::Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    CoerceI64::deserialize(deserializer).map(|value| value.0)
}

fn deserialize_optional_nullable_i64<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<CoerceI64>::deserialize(deserializer).map(|value| value.map(|value| value.0))
}

fn deserialize_coerce_usize<'de, D>(deserializer: D) -> std::result::Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    CoerceUsize::deserialize(deserializer).map(|value| value.0)
}

fn deserialize_string_list<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringListVisitor;

    impl<'de> Visitor<'de> for StringListVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a scalar string or a list of scalar strings")
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut values = Vec::new();
            while let Some(CoerceString(value)) = sequence.next_element()? {
                values.push(value);
            }
            Ok(values)
        }

        fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
            Ok(vec![value.to_owned()])
        }

        fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
            Ok(vec![value])
        }

        fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
            Ok(vec![if value { "True" } else { "False" }.into()])
        }

        fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
            Ok(vec![value.to_string()])
        }

        fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
            Ok(vec![value.to_string()])
        }

        fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E> {
            let mut value = value.to_string();
            if !value.contains(['.', 'e', 'E']) {
                value.push_str(".0");
            }
            Ok(vec![value])
        }
    }

    deserializer.deserialize_any(StringListVisitor)
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
                || config.step < 1e-9
            {
                return invalid(format!(
                    "input_number.{object_id} requires finite min < max and a finite step of at least 1e-9"
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
            if config.min > config.max || config.max == 0 || config.max > 255 {
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
        }
        for (object_id, config) in &self.input_select {
            validate_metadata("input_select", object_id, &config.name, &config.icon, None)?;
            if config.options.is_empty() {
                return invalid(format!(
                    "input_select.{object_id}.options must not be empty"
                ));
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
            if config.step < 0 {
                return invalid(format!(
                    "counter.{object_id}.step must be a nonnegative integer"
                ));
            }
        }
        Ok(())
    }
}

/// Initialize helper state using each domain's initial and restore rules, then
/// register their in-process services.
pub fn setup(runtime: &Runtime, helpers: &Helpers) -> Result<()> {
    helpers.validate()?;
    let registry = build_registry(helpers);

    // Preflight every restored value before changing any entity. The runtime
    // API has per-state atomic writes but no multi-state transaction.
    let mut init = Vec::with_capacity(registry.len());
    for (entity_id, helper) in &registry {
        let current = runtime.get_state(entity_id);
        let state = startup_state(helper, current.as_ref().map(|state| state.state.as_str()))?;
        init.push((entity_id.clone(), state, attributes(helper, entity_id)));
    }
    for (entity_id, state, attributes) in init {
        runtime.set_state(&entity_id, &state, attributes, Context::default(), false)?;
    }

    let registry = Arc::new(Mutex::new(registry));
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
        )?;
    }
    if !helpers.input_select.is_empty() {
        for (service, action) in [
            ("select_option", Action::SelectOption),
            ("select_next", Action::SelectNext),
            ("select_previous", Action::SelectPrevious),
            ("select_first", Action::SelectFirst),
            ("select_last", Action::SelectLast),
            ("set_options", Action::SelectSetOptions),
        ] {
            register(
                runtime,
                "input_select",
                service,
                action,
                "input_select",
                Arc::clone(&registry),
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
        let mut config = cfg.clone();
        config.options = deduplicate_options(&config.options);
        (format!("input_select.{id}"), Helper::InputSelect(config))
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
    registry: Arc<Mutex<BTreeMap<String, Helper>>>,
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
    SelectFirst,
    SelectLast,
    SelectSetOptions,
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
    SelectFirst,
    SelectLast,
    SelectSetOptions(Vec<String>),
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
    registry: &Mutex<BTreeMap<String, Helper>>,
) -> Result<ServiceResult> {
    if call.domain != expected_domain || call.service != expected_service {
        return invalid("helper service handler received a mismatched service call");
    }
    let mut registry = registry
        .lock()
        .map_err(|_| Error::Service("helper service registry lock was poisoned".into()))?;
    let (targets, payload) = parse_targets(call, &registry)?;
    let operation = parse_operation(action, &payload)?;

    // Compute and validate every outcome while holding the shared lock, then
    // write. A bad value or target in a batch cannot partially mutate helpers.
    let mut plan = Vec::with_capacity(targets.len());
    let mut updated_selects = Vec::new();
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
        if let Operation::SelectSetOptions(options) = &operation {
            let Helper::InputSelect(config) = helper else {
                return invalid(format!("service does not apply to {entity_id}"));
            };
            let mut updated = config.clone();
            updated.options.clone_from(options);
            let next = if options.iter().any(|option| option == &state.state) {
                state.state
            } else {
                options[0].clone()
            };
            let updated_helper = Helper::InputSelect(updated.clone());
            plan.push((
                entity_id.clone(),
                next,
                attributes(&updated_helper, entity_id),
            ));
            updated_selects.push((entity_id.clone(), updated));
        } else {
            let next = apply_operation(helper, &state.state, &operation, entity_id)?;
            plan.push((entity_id.clone(), next, attributes(helper, entity_id)));
        }
    }

    for (entity_id, config) in updated_selects {
        registry.insert(entity_id, Helper::InputSelect(config));
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
            let value = coerce_service_f64(data.get("value").unwrap(), "input_number.set_value")?;
            if !value.is_finite() {
                return invalid("input_number.set_value requires a finite numeric value");
            }
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
            let value = coerce_service_string(data.get("value").unwrap(), "input_text.set_value")?;
            Ok(Operation::TextSet(value))
        }
        Action::SelectOption => {
            only_data_fields(data, &["option"])?;
            let value =
                coerce_service_string(data.get("option").unwrap(), "input_select.select_option")?;
            Ok(Operation::SelectOption(value))
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
        Action::SelectFirst | Action::SelectLast => {
            require_no_data(data)?;
            Ok(if matches!(action, Action::SelectFirst) {
                Operation::SelectFirst
            } else {
                Operation::SelectLast
            })
        }
        Action::SelectSetOptions => {
            only_data_fields(data, &["options"])?;
            let raw = data.get("options").unwrap();
            let options = match raw {
                Value::Array(items) => items
                    .iter()
                    .map(|value| coerce_service_string(value, "input_select.set_options"))
                    .collect::<Result<Vec<_>>>()?,
                value => vec![coerce_service_string(value, "input_select.set_options")?],
            };
            if options.is_empty() {
                return invalid("input_select.set_options requires a nonempty options list");
            }
            let mut unique = HashSet::with_capacity(options.len());
            if options.iter().any(|option| !unique.insert(option)) {
                return invalid("input_select.set_options does not allow duplicate options");
            }
            Ok(Operation::SelectSetOptions(options))
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
            let value = coerce_service_i64(data.get("value").unwrap(), "counter.set_value")?;
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
        // `cycle` is the only optional field in these supported services.
        if *field != "cycle" && !data.contains_key(*field) {
            return invalid(format!("helper service is missing required '{field}' data"));
        }
    }
    Ok(())
}

fn coerce_service_string(value: &Value, service: &str) -> Result<String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Bool(value) => Ok(if *value { "True" } else { "False" }.into()),
        Value::Number(value) => Ok(value.to_string()),
        _ => invalid(format!("{service} requires a scalar string value")),
    }
}

fn coerce_service_f64(value: &Value, service: &str) -> Result<f64> {
    let value = match value {
        Value::Number(value) => value.as_f64(),
        Value::String(value) => value.trim().parse::<f64>().ok(),
        Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        _ => None,
    };
    value.ok_or_else(|| Error::InvalidInput(format!("{service} requires a numeric value")))
}

fn coerce_service_i64(value: &Value, service: &str) -> Result<i64> {
    let value = match value {
        Value::Number(value) => value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
            .or_else(|| value.as_f64().map(f64::trunc).and_then(as_i64)),
        Value::String(value) => value.trim().parse::<i64>().ok(),
        Value::Bool(value) => Some(i64::from(*value)),
        _ => None,
    };
    value.ok_or_else(|| Error::InvalidInput(format!("{service} requires an integer value")))
}

fn as_i64(value: f64) -> Option<i64> {
    (value.is_finite() && value >= i64::MIN as f64 && value < i64::MAX as f64)
        .then_some(value as i64)
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
                Operation::NumberIncrement => (current + config.step).min(config.max),
                Operation::NumberDecrement => (current - config.step).max(config.min),
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
        (Helper::InputSelect(config), Operation::SelectFirst) => Ok(config.options[0].clone()),
        (Helper::InputSelect(config), Operation::SelectLast) => {
            Ok(config.options[config.options.len() - 1].clone())
        }
        (Helper::Counter(config), operation) => {
            let current = parse_counter_state(current, entity_id)?;
            let value = match operation {
                Operation::CounterIncrement => current.checked_add(config.step),
                Operation::CounterDecrement => current.checked_sub(config.step),
                Operation::CounterReset => Some(clamp_counter(config.initial, config)),
                Operation::CounterSet(value) => {
                    if config.maximum.is_some_and(|maximum| *value > maximum) {
                        return invalid(format!(
                            "value {value} for {entity_id} exceeds its configured maximum"
                        ));
                    }
                    if config.minimum.is_some_and(|minimum| *value < minimum) {
                        return invalid(format!(
                            "value {value} for {entity_id} is below its configured minimum"
                        ));
                    }
                    if config.step > 0 && value % config.step != 0 {
                        return invalid(format!(
                            "value {value} for {entity_id} is not a multiple of step {}",
                            config.step
                        ));
                    }
                    Some(*value)
                }
                _ => return invalid(format!("service does not apply to {entity_id}")),
            }
            .ok_or_else(|| {
                Error::InvalidInput(format!("counter arithmetic overflow for {entity_id}"))
            })?;
            let value = if matches!(operation, Operation::CounterSet(_)) {
                value
            } else {
                clamp_counter(value, config)
            };
            Ok(value.to_string())
        }
        _ => invalid(format!("service does not apply to {entity_id}")),
    }
}

fn startup_state(helper: &Helper, restored: Option<&str>) -> Result<String> {
    match helper {
        Helper::InputBoolean(config) => Ok(if config
            .initial
            .unwrap_or_else(|| restored.is_some_and(|state| state == "on"))
        {
            "on"
        } else {
            "off"
        }
        .into()),
        Helper::InputNumber(config) => {
            if let Some(initial) = config.initial {
                return Ok(format_number(initial));
            }
            let restored = restored
                .and_then(|state| state.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value >= config.min && *value <= config.max)
                .unwrap_or(config.min);
            Ok(format_number(restored))
        }
        Helper::InputText(config) => Ok(config.initial.clone().unwrap_or_else(|| match restored {
            Some(value) if text_length_valid(value, config.min, config.max) => value.into(),
            _ => "unknown".into(),
        })),
        Helper::InputSelect(config) => Ok(config.initial.clone().unwrap_or_else(|| {
            restored
                .filter(|state| config.options.iter().any(|option| option == state))
                .unwrap_or(&config.options[0])
                .to_owned()
        })),
        Helper::Counter(config) => {
            if config.restore
                && let Some(restored) = restored
            {
                let value = parse_counter_state(restored, "restored counter")?;
                return Ok(clamp_counter(value, config).to_string());
            }
            Ok(config.initial.to_string())
        }
    }
}

fn clamp_counter(mut value: i64, config: &CounterConfig) -> i64 {
    if let Some(minimum) = config.minimum {
        value = value.max(minimum);
    }
    if let Some(maximum) = config.maximum {
        value = value.min(maximum);
    }
    value
}

fn deduplicate_options(options: &[String]) -> Vec<String> {
    let mut unique = HashSet::with_capacity(options.len());
    options
        .iter()
        .filter(|option| unique.insert(option.as_str()))
        .cloned()
        .collect()
}

fn attributes(helper: &Helper, _entity_id: &str) -> Map<String, Value> {
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
    let mut attrs = Map::new();
    if let Some(name) = name {
        attrs.insert("friendly_name".into(), Value::String(name.clone()));
    }
    if let Some(icon) = icon {
        attrs.insert("icon".into(), Value::String(icon.clone()));
    }
    if let Some(unit) = unit {
        attrs.insert("unit_of_measurement".into(), Value::String(unit.to_owned()));
    }
    match helper {
        Helper::InputNumber(config) => {
            attrs.insert(
                "initial".into(),
                config.initial.map_or(Value::Null, |initial| json!(initial)),
            );
            attrs.insert("editable".into(), Value::Bool(false));
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
            attrs.insert("editable".into(), Value::Bool(false));
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
            if let Some(pattern) = &config.pattern {
                attrs.insert("pattern".into(), Value::String(pattern.clone()));
            }
        }
        Helper::InputSelect(config) => {
            attrs.insert("editable".into(), Value::Bool(false));
            attrs.insert("options".into(), json!(&config.options));
        }
        Helper::Counter(config) => {
            attrs.insert("editable".into(), Value::Bool(false));
            attrs.insert("initial".into(), json!(config.initial));
            attrs.insert("step".into(), json!(config.step));
            if let Some(minimum) = config.minimum {
                attrs.insert("minimum".into(), json!(minimum));
            }
            if let Some(maximum) = config.maximum {
                attrs.insert("maximum".into(), json!(maximum));
            }
        }
        Helper::InputBoolean(_) => {
            attrs.insert("editable".into(), Value::Bool(false));
        }
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
    if icon
        .as_deref()
        .is_some_and(|icon| !icon.contains(':') || icon.chars().any(char::is_whitespace))
    {
        return invalid(format!(
            "{domain}.{object_id}.icon must use a non-whitespace prefix:name form"
        ));
    }
    let _ = (name, unit);
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
    // NumberEntity exposes native_value unchanged without a unit conversion.
    // Fixed decimal formatting would round valid small values down to zero.
    value.to_string()
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

    fn seed(runtime: &Runtime, entity_id: &str, state: &str) {
        runtime
            .set_state(entity_id, state, Map::new(), Context::default(), false)
            .unwrap();
    }

    #[test]
    fn initializes_missing_states_and_preserves_restored_values() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("helpers.sqlite");
        let first = Runtime::new(Arc::new(
            hass_storage::SqliteStore::open(&database).unwrap(),
        ))
        .unwrap();
        let helpers: Helpers =
            serde_yaml::from_str("input_boolean:\n  guest_mode:\n    name: Guest mode\n").unwrap();
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
            "input_number:\n  first:\n    min: 0\n    max: 5\n    initial: 1\n  second:\n    min: 0\n    max: 2\n    initial: 2\n",
        )
        .unwrap();
        setup(&runtime, &helpers).unwrap();

        let result = runtime.call_service(call(
            "input_number",
            "set_value",
            json!({"entity_id": ["input_number.first", "input_number.second"]}),
            json!({"value": 3}),
        ));
        assert!(result.is_err());
        assert_eq!(runtime.get_state("input_number.first").unwrap().state, "1");
        assert_eq!(runtime.get_state("input_number.second").unwrap().state, "2");
    }

    #[test]
    fn initial_restore_and_fallback_rules_match_helper_domains() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        for (entity_id, state) in [
            ("input_boolean.explicit", "on"),
            ("input_boolean.restored", "on"),
            ("input_number.explicit", "8"),
            ("input_number.restored", "8"),
            ("input_number.fallback", "99"),
            ("input_text.explicit", "old"),
            ("input_text.restored", "yes"),
            ("input_text.invalid", "too long"),
            ("input_select.explicit", "first"),
            ("input_select.restored", "last"),
            ("input_select.fallback", "not an option"),
            ("counter.no_restore", "3"),
            ("counter.restored", "3"),
            ("counter.clamped", "99"),
        ] {
            seed(&runtime, entity_id, state);
        }
        let helpers: Helpers = serde_yaml::from_str(
            r#"
input_boolean:
  explicit:
    initial: false
  restored: {}
input_number:
  explicit:
    min: 0
    max: 10
    initial: 2
  restored:
    min: 0
    max: 10
  fallback:
    min: 5
    max: 10
input_text:
  explicit:
    min: 1
    max: 5
    initial: new
  restored:
    min: 1
    max: 5
  invalid:
    min: 2
    max: 5
  absent:
    min: 1
    max: 5
input_select:
  explicit:
    options: [first, last]
    initial: last
  restored:
    options: [first, last]
  fallback:
    options: [first, last]
counter:
  no_restore:
    initial: 2
    restore: false
  restored:
    initial: 1
    step: 2
    minimum: 0
    maximum: 4
  clamped:
    minimum: 0
    maximum: 4
"#,
        )
        .unwrap();
        setup(&runtime, &helpers).unwrap();

        for (entity_id, expected) in [
            ("input_boolean.explicit", "off"),
            ("input_boolean.restored", "on"),
            ("input_number.explicit", "2"),
            ("input_number.restored", "8"),
            ("input_number.fallback", "5"),
            ("input_text.explicit", "new"),
            ("input_text.restored", "yes"),
            ("input_text.invalid", "unknown"),
            ("input_text.absent", "unknown"),
            ("input_select.explicit", "last"),
            ("input_select.restored", "last"),
            ("input_select.fallback", "first"),
            ("counter.no_restore", "2"),
            ("counter.restored", "3"),
            ("counter.clamped", "4"),
        ] {
            assert_eq!(
                runtime.get_state(entity_id).unwrap().state,
                expected,
                "{entity_id}"
            );
        }
    }

    #[test]
    fn accepts_pinned_yaml_coercions_and_publishes_helper_attributes() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        let helpers: Helpers = serde_yaml::from_str(
            r#"
input_boolean:
  defaulted: null
  coerced:
    initial: 'yes'
    name: 3
    icon: ' mdi:lan '
input_number:
  coerced:
    min: '0'
    max: '3'
    step: '0.1'
    initial: '1.5'
input_text:
  defaulted: null
  patterned:
    min: '1'
    max: 4
    initial: 2
    pattern: '[A-Z]+'
input_select:
  scalar:
    options: home
  duplicates:
    options: [a, a, b]
counter:
  defaulted: null
  configured:
    initial: '5'
    step: '2'
    minimum: '0'
    maximum: '10'
    restore: 'off'
"#,
        )
        .unwrap();
        setup(&runtime, &helpers).unwrap();

        let boolean = runtime.get_state("input_boolean.coerced").unwrap();
        assert_eq!(boolean.state, "on");
        assert_eq!(boolean.attributes["friendly_name"], "3");
        assert_eq!(boolean.attributes["icon"], "mdi:lan");
        assert_eq!(boolean.attributes["editable"], false);
        let unnamed = runtime.get_state("input_boolean.defaulted").unwrap();
        assert!(!unnamed.attributes.contains_key("friendly_name"));

        let number = runtime.get_state("input_number.coerced").unwrap();
        assert_eq!(number.state, "1.5");
        assert_eq!(number.attributes["initial"], 1.5);
        assert_eq!(number.attributes["editable"], false);

        let text = runtime.get_state("input_text.patterned").unwrap();
        assert_eq!(text.state, "2");
        assert_eq!(text.attributes["pattern"], "[A-Z]+");
        assert_eq!(text.attributes["editable"], false);
        let text_default = runtime.get_state("input_text.defaulted").unwrap();
        assert_eq!(text_default.state, "unknown");
        assert_eq!(text_default.attributes["mode"], "text");

        let scalar = runtime.get_state("input_select.scalar").unwrap();
        assert_eq!(scalar.state, "home");
        assert_eq!(scalar.attributes["options"], json!(["home"]));
        let deduplicated = runtime.get_state("input_select.duplicates").unwrap();
        assert_eq!(deduplicated.state, "a");
        assert_eq!(deduplicated.attributes["options"], json!(["a", "b"]));

        let counter = runtime.get_state("counter.configured").unwrap();
        assert_eq!(counter.state, "5");
        assert_eq!(counter.attributes["editable"], false);
        assert!(!counter.attributes.contains_key("restore"));
    }

    #[test]
    fn input_select_options_services_update_state_and_capabilities() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        let helpers: Helpers = serde_yaml::from_str(
            "input_select:\n  mode:\n    options: [home, away]\n    initial: home\n",
        )
        .unwrap();
        setup(&runtime, &helpers).unwrap();

        runtime
            .call_service(call(
                "input_select",
                "select_last",
                json!({"entity_id": "input_select.mode"}),
                json!({}),
            ))
            .unwrap();
        assert_eq!(
            runtime.get_state("input_select.mode").unwrap().state,
            "away"
        );

        runtime
            .call_service(call(
                "input_select",
                "set_options",
                json!({"entity_id": "input_select.mode"}),
                json!({"options": ["sleep", "work"]}),
            ))
            .unwrap();
        let state = runtime.get_state("input_select.mode").unwrap();
        assert_eq!(state.state, "sleep");
        assert_eq!(state.attributes["options"], json!(["sleep", "work"]));

        let duplicate = runtime.call_service(call(
            "input_select",
            "set_options",
            json!({"entity_id": "input_select.mode"}),
            json!({"options": ["broken", "broken"]}),
        ));
        assert!(duplicate.is_err());
        let invalid_option = runtime.call_service(call(
            "input_select",
            "select_option",
            json!({"entity_id": "input_select.mode"}),
            json!({"option": "home"}),
        ));
        assert!(invalid_option.is_err());
        let state = runtime.get_state("input_select.mode").unwrap();
        assert_eq!(state.state, "sleep");
        assert_eq!(state.attributes["options"], json!(["sleep", "work"]));

        runtime
            .call_service(call(
                "input_select",
                "select_last",
                json!({"entity_id": "input_select.mode"}),
                json!({}),
            ))
            .unwrap();
        assert_eq!(
            runtime.get_state("input_select.mode").unwrap().state,
            "work"
        );
        runtime
            .call_service(call(
                "input_select",
                "select_first",
                json!({"entity_id": "input_select.mode"}),
                json!({}),
            ))
            .unwrap();
        assert_eq!(
            runtime.get_state("input_select.mode").unwrap().state,
            "sleep"
        );
    }

    #[test]
    fn number_and_counter_services_clamp_preserve_precision_and_validate_before_writing() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        let helpers: Helpers = serde_yaml::from_str(
            r#"
input_number:
  target:
    min: 0
    max: 3.8
    step: 1.2
    initial: 2.4
  precise:
    min: 0
    max: 1
    step: 0.000000001
    initial: 0.0000000000000001
counter:
  arrivals:
    initial: 10
    step: 5
    minimum: 5
    maximum: 20
    restore: false
  reset_clamps:
    initial: 25
    maximum: 20
    restore: false
"#,
        )
        .unwrap();
        setup(&runtime, &helpers).unwrap();

        assert_eq!(
            runtime
                .get_state("input_number.precise")
                .unwrap()
                .state
                .parse::<f64>()
                .unwrap(),
            1e-16
        );
        runtime
            .call_service(call(
                "input_number",
                "set_value",
                json!({"entity_id":"input_number.precise"}),
                json!({"value":2e-16}),
            ))
            .unwrap();
        assert_eq!(
            runtime
                .get_state("input_number.precise")
                .unwrap()
                .state
                .parse::<f64>()
                .unwrap(),
            2e-16
        );
        for (service, expected) in [("increment", "3.5999999999999996"), ("increment", "3.8")] {
            runtime
                .call_service(call(
                    "input_number",
                    service,
                    json!({"entity_id": "input_number.target"}),
                    json!({}),
                ))
                .unwrap();
            assert_eq!(
                runtime.get_state("input_number.target").unwrap().state,
                expected
            );
        }
        let invalid_number = runtime.call_service(call(
            "input_number",
            "set_value",
            json!({"entity_id": "input_number.target"}),
            json!({"value": 4}),
        ));
        assert!(invalid_number.is_err());
        assert_eq!(
            runtime.get_state("input_number.target").unwrap().state,
            "3.8"
        );

        for (service, expected) in [
            ("increment", "15"),
            ("increment", "20"),
            ("increment", "20"),
            ("decrement", "15"),
            ("reset", "10"),
        ] {
            runtime
                .call_service(call(
                    "counter",
                    service,
                    json!({"entity_id": "counter.arrivals"}),
                    json!({}),
                ))
                .unwrap();
            assert_eq!(
                runtime.get_state("counter.arrivals").unwrap().state,
                expected
            );
        }
        runtime
            .call_service(call(
                "counter",
                "set_value",
                json!({"entity_id": "counter.arrivals"}),
                json!({"value": 5}),
            ))
            .unwrap();
        let invalid_step = runtime.call_service(call(
            "counter",
            "set_value",
            json!({"entity_id": "counter.arrivals"}),
            json!({"value": 6}),
        ));
        assert!(invalid_step.is_err());
        assert_eq!(runtime.get_state("counter.arrivals").unwrap().state, "5");

        assert_eq!(
            runtime.get_state("counter.reset_clamps").unwrap().state,
            "25"
        );
        runtime
            .call_service(call(
                "counter",
                "reset",
                json!({"entity_id": "counter.reset_clamps"}),
                json!({}),
            ))
            .unwrap();
        assert_eq!(
            runtime.get_state("counter.reset_clamps").unwrap().state,
            "20"
        );
    }

    #[test]
    fn text_select_and_generic_boolean_services_use_real_helper_state() {
        let runtime = runtime(Arc::new(MemoryStore::default()));
        let _generic = crate::homeassistant::HomeAssistantServices::start(runtime.clone()).unwrap();
        let helpers: Helpers = serde_yaml::from_str(
            "input_boolean:\n  guest_mode: {}\ninput_text:\n  note:\n    initial: ok\n    min: 1\n    max: 8\n    pattern: '^z+$'\ninput_select:\n  mode:\n    options: [home, away]\n    initial: home\n",
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
        assert_eq!(
            runtime.get_state("input_text.note").unwrap().attributes["pattern"],
            "^z+$"
        );

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
        assert_eq!(helpers.counter["arrivals"].step, 1);
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
