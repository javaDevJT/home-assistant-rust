//! Bounded native rendering for the supported Home Assistant template subset.
//!
//! The renderer reads the shared [`Runtime`] on every render. It has no file
//! loader, shell access, or mutable globals, so templates cannot open arbitrary
//! host resources or retain stale state snapshots.

use std::io::{self, Write};
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use hass_core::{Error, Runtime, State};
use minijinja::value::{Enumerator, Kwargs, Object, ObjectRepr, Rest, Value as MiniValue};
use minijinja::{Environment, Error as MiniError, ErrorKind, State as EvalState};
use serde_json::{Map, Value};

/// Result type shared with the native runtime.
pub type Result<T> = hass_core::Result<T>;

const MAX_TEMPLATE_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_FUEL: u64 = 25_000;
const MAX_RECURSION: usize = 64;

/// A cloneable template renderer backed by a shared Home Assistant runtime.
#[derive(Clone)]
pub struct TemplateEngine {
    environment: Arc<Environment<'static>>,
}

impl TemplateEngine {
    /// Construct a renderer that reads current state from `runtime`.
    pub fn new(runtime: Runtime) -> Self {
        let mut environment = Environment::new();
        environment.set_fuel(Some(MAX_FUEL));
        environment.set_recursion_limit(MAX_RECURSION);

        environment.add_global(
            "states",
            MiniValue::from_object(StatesObject {
                runtime: runtime.clone(),
            }),
        );

        register_state_helpers(&mut environment, runtime.clone());
        register_numeric_helpers(&mut environment);
        register_datetime_helpers(&mut environment, runtime);

        Self {
            environment: Arc::new(environment),
        }
    }

    /// Render a Home Assistant Jinja template to a bounded UTF-8 string.
    pub fn render(&self, template: &str, variables: &Map<String, Value>) -> Result<String> {
        ensure_template_size(template)?;
        let template = self
            .environment
            .template_from_str(template)
            .map_err(map_template_error)?;
        let mut output = BoundedWriter::new(MAX_OUTPUT_BYTES);
        template
            .render_captured_to(MiniValue::from_serialize(variables), &mut output)
            .map_err(map_template_error)?;
        String::from_utf8(output.bytes)
            .map_err(|_| Error::InvalidInput("template output was not valid UTF-8".into()))
    }

    /// Evaluate an expression or full template, preserving supported JSON types.
    ///
    /// Accepts either a bare expression such as `states('sensor.temp') | float`
    /// or one `{{ ... }}` wrapper. Control blocks and multiple output nodes use
    /// the same bounded renderer as [`Self::render`], then parse supported
    /// literal results. Ordinary text and quoted string results remain strings.
    pub fn render_value(&self, template: &str, variables: &Map<String, Value>) -> Result<Value> {
        ensure_template_size(template)?;
        let Some(expression) = expression_body(template) else {
            return self.render(template, variables).map(parse_rendered_value);
        };
        let expression = self
            .environment
            .compile_expression(expression)
            .map_err(map_template_error)?;
        let value = expression
            .eval(MiniValue::from_serialize(variables))
            .map_err(map_template_error)?;

        let mut json_bytes = BoundedWriter::new(MAX_OUTPUT_BYTES);
        serde_json::to_writer(&mut json_bytes, &value).map_err(|_| {
            Error::InvalidInput(
                "template value exceeded the output limit or was not JSON-compatible".into(),
            )
        })?;
        serde_json::from_slice(&json_bytes.bytes)
            .map_err(|_| Error::InvalidInput("template value is not JSON-compatible".into()))
    }
}

fn parse_rendered_value(rendered: String) -> Value {
    let text = rendered.trim();
    match text {
        "True" => return Value::Bool(true),
        "False" => return Value::Bool(false),
        "None" => return Value::Null,
        _ => {}
    }
    // The pinned result parser accepts decimal numeric text but deliberately
    // retains scientific notation and quoted strings as the rendered text.
    if text.bytes().any(|byte| byte.is_ascii_digit())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'+' | b'-'))
    {
        if text.contains('.') {
            if let Ok(number) = text.parse::<f64>()
                && let Some(number) = serde_json::Number::from_f64(number)
            {
                return Value::Number(number);
            }
        } else if let Ok(number) = text.parse::<i64>() {
            return Value::Number(number.into());
        } else if let Ok(number) = text.parse::<u64>() {
            return Value::Number(number.into());
        }
    }
    if matches!(text.as_bytes().first(), Some(b'[' | b'{'))
        && let Ok(value @ (Value::Array(_) | Value::Object(_))) = serde_json::from_str(text)
        && supported_container_literal(&value)
        && !text.contains("\\/")
    {
        return value;
    }
    Value::String(text.to_owned())
}

fn supported_container_literal(value: &Value) -> bool {
    match value {
        // Lowercase JSON booleans/null are not Python literals. Keep an
        // unsupported rendered container as text rather than change its type.
        Value::Bool(_) | Value::Null => false,
        Value::Array(values) => values.iter().all(supported_container_literal),
        Value::Object(values) => values.values().all(supported_container_literal),
        Value::Number(_) | Value::String(_) => true,
    }
}

fn ensure_template_size(template: &str) -> Result<()> {
    if template.len() > MAX_TEMPLATE_BYTES {
        return Err(Error::InvalidInput(
            "template source exceeds the size limit".into(),
        ));
    }
    Ok(())
}

fn expression_body(template: &str) -> Option<&str> {
    let trimmed = template.trim();
    if let Some(inner) = trimmed.strip_prefix("{{") {
        let inner = inner.strip_suffix("}}")?.trim();
        if inner.contains("{{") || inner.contains("}}") || inner.is_empty() {
            return None;
        }
        Some(inner)
    } else if trimmed.is_empty()
        || trimmed.contains("{{")
        || trimmed.contains("}}")
        || trimmed.contains("{%")
        || trimmed.contains("{#")
    {
        None
    } else {
        Some(trimmed)
    }
}

fn map_template_error(error: MiniError) -> Error {
    let location = error
        .line()
        .map(|line| format!(" at line {line}"))
        .unwrap_or_default();
    Error::InvalidInput(format!(
        "template evaluation failed{location} ({:?})",
        error.kind()
    ))
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(4096)),
            limit,
        }
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.limit {
            return Err(io::Error::other("template output exceeds the size limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn register_state_helpers(environment: &mut Environment<'static>, runtime: Runtime) {
    let state_runtime = runtime.clone();
    environment.add_function("is_state", move |entity_id: String, expected: MiniValue| {
        is_state(&state_runtime, &entity_id, &expected)
    });
    let state_runtime = runtime.clone();
    environment.add_test("is_state", move |entity_id: String, expected: MiniValue| {
        is_state(&state_runtime, &entity_id, &expected)
    });

    let attr_runtime = runtime.clone();
    environment.add_function("state_attr", move |entity_id: String, attribute: String| {
        state_attr(&attr_runtime, &entity_id, &attribute)
    });
    let attr_runtime = runtime.clone();
    environment.add_filter("state_attr", move |entity_id: String, attribute: String| {
        state_attr(&attr_runtime, &entity_id, &attribute)
    });

    let attr_runtime = runtime.clone();
    environment.add_function(
        "is_state_attr",
        move |entity_id: String, attribute: String, expected: MiniValue| {
            is_state_attr(&attr_runtime, &entity_id, &attribute, &expected)
        },
    );
    let attr_runtime = runtime.clone();
    environment.add_test(
        "is_state_attr",
        move |entity_id: String, attribute: String, expected: MiniValue| {
            is_state_attr(&attr_runtime, &entity_id, &attribute, &expected)
        },
    );

    let value_runtime = runtime.clone();
    environment.add_function("has_value", move |entity_id: String| {
        has_value(&value_runtime, &entity_id)
    });
    let value_runtime = runtime.clone();
    environment.add_filter("has_value", move |entity_id: String| {
        has_value(&value_runtime, &entity_id)
    });
    let states_runtime = runtime.clone();
    let value_runtime = runtime;
    environment.add_test("has_value", move |entity_id: String| {
        has_value(&value_runtime, &entity_id)
    });

    environment.add_filter("states", move |entity_id: String| {
        MiniValue::from(states_value(&states_runtime, &entity_id, false, false))
    });
}

fn is_state(runtime: &Runtime, entity_id: &str, expected: &MiniValue) -> bool {
    runtime
        .get_state(entity_id)
        .is_some_and(|state| matches_state(&state.state, expected))
}

fn matches_state(actual: &str, expected: &MiniValue) -> bool {
    if let Some(expected) = expected.as_str() {
        return actual == expected;
    }
    let Ok(expected) = serde_json::to_value(expected) else {
        return false;
    };
    match expected {
        Value::Array(values) => values.iter().any(|value| value.as_str() == Some(actual)),
        _ => false,
    }
}

fn state_attr(runtime: &Runtime, entity_id: &str, attribute: &str) -> MiniValue {
    runtime
        .get_state(entity_id)
        .and_then(|state| state.attributes.get(attribute).cloned())
        .map(MiniValue::from_serialize)
        .unwrap_or_else(|| MiniValue::from(()))
}

fn is_state_attr(
    runtime: &Runtime,
    entity_id: &str,
    attribute: &str,
    expected: &MiniValue,
) -> bool {
    runtime
        .get_state(entity_id)
        .and_then(|state| state.attributes.get(attribute).cloned())
        .is_some_and(|actual| MiniValue::from_serialize(actual) == *expected)
}

fn has_value(runtime: &Runtime, entity_id: &str) -> bool {
    runtime
        .get_state(entity_id)
        .is_some_and(|state| state.state != "unknown" && state.state != "unavailable")
}

fn states_value(runtime: &Runtime, entity_id: &str, rounded: bool, with_unit: bool) -> String {
    runtime
        .get_state(entity_id)
        .map(|state| format_state(&state, rounded, with_unit))
        .unwrap_or_else(|| "unknown".into())
}

fn format_state(state: &State, rounded: bool, with_unit: bool) -> String {
    let mut rendered = state.state.clone();
    if rounded
        && let Some(precision) = state
            .attributes
            .get("suggested_display_precision")
            .and_then(Value::as_u64)
            .filter(|precision| *precision <= 10)
        && let Ok(number) = state.state.parse::<f64>()
    {
        let precision = precision as usize;
        rendered = format!("{number:.precision$}");
        while rendered.contains('.') && rendered.ends_with('0') {
            rendered.pop();
        }
        if rendered.ends_with('.') {
            rendered.pop();
        }
    }
    if with_unit
        && let Some(unit) = state
            .attributes
            .get("unit_of_measurement")
            .and_then(Value::as_str)
        && !unit.is_empty()
    {
        rendered.push(' ');
        rendered.push_str(unit);
    }
    rendered
}

fn valid_entity_id(entity_id: &str) -> bool {
    let Some((domain, object_id)) = entity_id.split_once('.') else {
        return false;
    };
    if domain.is_empty() || object_id.is_empty() || object_id.contains('.') {
        return false;
    }
    let valid = |part: &str| {
        part.bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    };
    valid(domain) && valid(object_id)
}

struct StatesObject {
    runtime: Runtime,
}

impl std::fmt::Debug for StatesObject {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StatesObject")
    }
}

impl Object for StatesObject {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Iterable
    }

    fn get_value(self: &Arc<Self>, key: &MiniValue) -> Option<MiniValue> {
        self.get_value_by_str(key.as_str()?)
    }

    fn get_value_by_str(self: &Arc<Self>, key: &str) -> Option<MiniValue> {
        if key.contains('.') {
            if !valid_entity_id(key) {
                return Some(MiniValue::UNDEFINED);
            }
            return Some(
                self.runtime
                    .get_state(key)
                    .map(template_state_value)
                    .unwrap_or(MiniValue::UNDEFINED),
            );
        }
        if key.starts_with('_') || !valid_domain(key) {
            return Some(MiniValue::UNDEFINED);
        }
        Some(MiniValue::from_object(DomainStatesObject {
            runtime: self.runtime.clone(),
            domain: key.to_owned(),
        }))
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Iter(Box::new(
            self.runtime.states().into_iter().map(template_state_value),
        ))
    }

    fn enumerator_len(self: &Arc<Self>) -> Option<usize> {
        Some(self.runtime.states().len())
    }

    fn call(
        self: &Arc<Self>,
        _state: &EvalState<'_, '_>,
        args: &[MiniValue],
    ) -> std::result::Result<MiniValue, MiniError> {
        let Some(entity_id) = args.first().and_then(MiniValue::as_str) else {
            return Err(invalid_template_call("states expects an entity ID"));
        };
        let mut options = CallOptions::from_values(args[1..].to_vec())?;
        let rounded = options
            .take_optional("rounded")?
            .as_ref()
            .and_then(parse_bool)
            .unwrap_or(false);
        let with_unit = options
            .take_optional("with_unit")?
            .as_ref()
            .and_then(parse_bool)
            .unwrap_or(false);
        options.finish()?;
        Ok(MiniValue::from(states_value(
            &self.runtime,
            entity_id,
            rounded,
            with_unit,
        )))
    }
}

struct DomainStatesObject {
    runtime: Runtime,
    domain: String,
}

impl std::fmt::Debug for DomainStatesObject {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DomainStatesObject")
            .field("domain", &self.domain)
            .finish()
    }
}

impl Object for DomainStatesObject {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Iterable
    }

    fn get_value(self: &Arc<Self>, key: &MiniValue) -> Option<MiniValue> {
        self.get_value_by_str(key.as_str()?)
    }

    fn get_value_by_str(self: &Arc<Self>, key: &str) -> Option<MiniValue> {
        let entity_id = format!("{}.{}", self.domain, key);
        if !valid_entity_id(&entity_id) {
            return Some(MiniValue::UNDEFINED);
        }
        Some(
            self.runtime
                .get_state(&entity_id)
                .map(template_state_value)
                .unwrap_or(MiniValue::UNDEFINED),
        )
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        let prefix = format!("{}.", self.domain);
        Enumerator::Iter(Box::new(
            self.runtime
                .states()
                .into_iter()
                .filter(move |state| state.entity_id.starts_with(&prefix))
                .map(template_state_value),
        ))
    }

    fn enumerator_len(self: &Arc<Self>) -> Option<usize> {
        Some(
            self.runtime
                .states()
                .iter()
                .filter(|state| state.entity_id.starts_with(&format!("{}.", self.domain)))
                .count(),
        )
    }
}

fn valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn template_state_value(state: State) -> MiniValue {
    MiniValue::from_object(TemplateStateObject { state })
}

#[derive(Debug)]
struct TemplateStateObject {
    state: State,
}

impl Object for TemplateStateObject {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &MiniValue) -> Option<MiniValue> {
        self.get_value_by_str(key.as_str()?)
    }

    fn get_value_by_str(self: &Arc<Self>, key: &str) -> Option<MiniValue> {
        let state = &self.state;
        let result = match key {
            "entity_id" => Some(MiniValue::from(state.entity_id.clone())),
            "state" => Some(MiniValue::from(state.state.clone())),
            "attributes" => Some(MiniValue::from_serialize(&state.attributes)),
            "last_changed" => Some(MiniValue::from(state.last_changed.to_rfc3339())),
            "last_updated" => Some(MiniValue::from(state.last_updated.to_rfc3339())),
            "last_reported" => Some(MiniValue::from(state.last_reported.to_rfc3339())),
            "context" => Some(MiniValue::from_serialize(&state.context)),
            "domain" => Some(MiniValue::from(
                state
                    .entity_id
                    .split_once('.')
                    .map(|pair| pair.0)
                    .unwrap_or(""),
            )),
            "object_id" => Some(MiniValue::from(
                state
                    .entity_id
                    .split_once('.')
                    .map(|pair| pair.1)
                    .unwrap_or(""),
            )),
            "name" => Some(MiniValue::from(
                state
                    .attributes
                    .get("friendly_name")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| {
                        state
                            .entity_id
                            .split_once('.')
                            .map(|pair| pair.1)
                            .unwrap_or("")
                    }),
            )),
            "state_with_unit" => Some(MiniValue::from(format_state(state, true, true))),
            _ => state.attributes.get(key).map(MiniValue::from_serialize),
        };
        result.or(Some(MiniValue::UNDEFINED))
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        let mut keys = vec![
            "entity_id",
            "state",
            "attributes",
            "last_changed",
            "last_updated",
            "last_reported",
            "context",
            "domain",
            "object_id",
            "name",
            "state_with_unit",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        keys.extend(self.state.attributes.keys().cloned());
        Enumerator::Iter(Box::new(keys.into_iter().map(MiniValue::from)))
    }
}

struct CallOptions {
    positional: Vec<MiniValue>,
    kwargs: Option<Kwargs>,
}

impl CallOptions {
    fn from_values(mut args: Vec<MiniValue>) -> std::result::Result<Self, MiniError> {
        let kwargs = if args.last().is_some_and(MiniValue::is_kwargs) {
            let value = args
                .pop()
                .ok_or_else(|| invalid_template_call("invalid arguments"))?;
            Some(Kwargs::try_from(value)?)
        } else {
            None
        };
        Ok(Self {
            positional: args,
            kwargs,
        })
    }

    fn take_optional(&mut self, name: &str) -> std::result::Result<Option<MiniValue>, MiniError> {
        let positional = if self.positional.is_empty() {
            None
        } else {
            Some(self.positional.remove(0))
        };
        let keyword = if self.kwargs.as_ref().is_some_and(|kwargs| kwargs.has(name)) {
            self.kwargs
                .as_ref()
                .ok_or_else(|| invalid_template_call("invalid keyword arguments"))?
                .get::<MiniValue>(name)
                .map(Some)?
        } else {
            None
        };
        if positional.is_some() && keyword.is_some() {
            return Err(invalid_template_call("argument supplied more than once"));
        }
        Ok(keyword.or(positional))
    }

    fn finish(self) -> std::result::Result<(), MiniError> {
        if !self.positional.is_empty() {
            return Err(invalid_template_call("too many template arguments"));
        }
        if let Some(kwargs) = self.kwargs {
            kwargs.assert_all_used()?;
        }
        Ok(())
    }
}

fn invalid_template_call(message: &'static str) -> MiniError {
    MiniError::new(ErrorKind::InvalidOperation, message)
}

fn register_numeric_helpers(environment: &mut Environment<'static>) {
    environment.add_function("float", float_function);
    environment.add_filter("float", float_filter);
    environment.add_function("int", int_function);
    environment.add_filter("int", int_filter);
    environment.add_function("bool", bool_function);
    environment.add_filter("bool", bool_filter);
    environment.add_function("is_number", is_number_function);
    environment.add_filter("is_number", is_number_filter);
    environment.add_test("is_number", is_number_filter);
}

fn float_function(args: Rest<MiniValue>) -> std::result::Result<MiniValue, MiniError> {
    let mut values = args.0;
    let value = values
        .first()
        .cloned()
        .ok_or_else(|| invalid_template_call("float expects a value"))?;
    values.remove(0);
    forgiving_float(&value, values)
}

fn float_filter(
    value: MiniValue,
    args: Rest<MiniValue>,
) -> std::result::Result<MiniValue, MiniError> {
    forgiving_float(&value, args.0)
}

fn forgiving_float(
    value: &MiniValue,
    args: Vec<MiniValue>,
) -> std::result::Result<MiniValue, MiniError> {
    let mut options = CallOptions::from_values(args)?;
    let default = options.take_optional("default")?;
    options.finish()?;
    if let Some(number) = parse_float(value) {
        return Ok(MiniValue::from(number));
    }
    default.ok_or_else(|| invalid_template_call("float received invalid input without a default"))
}

fn int_function(args: Rest<MiniValue>) -> std::result::Result<MiniValue, MiniError> {
    let mut values = args.0;
    let value = values
        .first()
        .cloned()
        .ok_or_else(|| invalid_template_call("int expects a value"))?;
    values.remove(0);
    forgiving_int(&value, values)
}

fn int_filter(
    value: MiniValue,
    args: Rest<MiniValue>,
) -> std::result::Result<MiniValue, MiniError> {
    forgiving_int(&value, args.0)
}

fn forgiving_int(
    value: &MiniValue,
    args: Vec<MiniValue>,
) -> std::result::Result<MiniValue, MiniError> {
    let mut options = CallOptions::from_values(args)?;
    let default = options.take_optional("default")?;
    let base = options
        .take_optional("base")?
        .map(|value| value.as_i64().unwrap_or_default())
        .unwrap_or(10);
    options.finish()?;
    if let Some(number) = parse_int(value, base) {
        return Ok(number);
    }
    default.ok_or_else(|| invalid_template_call("int received invalid input without a default"))
}

fn bool_function(args: Rest<MiniValue>) -> std::result::Result<MiniValue, MiniError> {
    let mut values = args.0;
    let value = values
        .first()
        .cloned()
        .ok_or_else(|| invalid_template_call("bool expects a value"))?;
    values.remove(0);
    forgiving_bool(&value, values)
}

fn bool_filter(
    value: MiniValue,
    args: Rest<MiniValue>,
) -> std::result::Result<MiniValue, MiniError> {
    forgiving_bool(&value, args.0)
}

fn forgiving_bool(
    value: &MiniValue,
    args: Vec<MiniValue>,
) -> std::result::Result<MiniValue, MiniError> {
    let mut options = CallOptions::from_values(args)?;
    let default = options.take_optional("default")?;
    options.finish()?;
    if let Some(value) = parse_bool(value) {
        return Ok(MiniValue::from(value));
    }
    default.ok_or_else(|| invalid_template_call("bool received invalid input without a default"))
}

fn is_number_function(args: Rest<MiniValue>) -> bool {
    args.0
        .first()
        .is_some_and(|value| parse_float(value).is_some_and(f64::is_finite))
}

fn is_number_filter(value: MiniValue) -> bool {
    parse_float(&value).is_some_and(f64::is_finite)
}

fn parse_float(value: &MiniValue) -> Option<f64> {
    if let Some(value) = value.as_str() {
        return value.trim().parse().ok();
    }
    match serde_json::to_value(value).ok()? {
        Value::Number(number) => number.as_f64(),
        Value::Bool(true) => Some(1.0),
        Value::Bool(false) => Some(0.0),
        _ => None,
    }
}

fn parse_int(value: &MiniValue, base: i64) -> Option<MiniValue> {
    match serde_json::to_value(value).ok()? {
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                return Some(MiniValue::from(value));
            }
            if let Some(value) = number.as_u64() {
                return Some(MiniValue::from(value));
            }
            if let Some(value) = number.as_f64() {
                if !value.is_finite() || value < i64::MIN as f64 || value >= -(i64::MIN as f64) {
                    return None;
                }
                return Some(MiniValue::from(value.trunc() as i64));
            }
        }
        Value::Bool(value) => return Some(MiniValue::from(i64::from(value))),
        _ => {}
    }
    let text = value.as_str()?.trim();
    if base == 10 {
        if let Ok(number) = text.parse::<i64>() {
            return Some(MiniValue::from(number));
        }
        let number = text.parse::<f64>().ok()?;
        if !number.is_finite() || number < i64::MIN as f64 || number >= -(i64::MIN as f64) {
            return None;
        }
        return Some(MiniValue::from(number.trunc() as i64));
    }
    if !(2..=36).contains(&base) {
        return None;
    }
    let (negative, digits) = match text.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let digits = match base {
        16 => digits
            .strip_prefix("0x")
            .or_else(|| digits.strip_prefix("0X"))
            .unwrap_or(digits),
        8 => digits
            .strip_prefix("0o")
            .or_else(|| digits.strip_prefix("0O"))
            .unwrap_or(digits),
        2 => digits
            .strip_prefix("0b")
            .or_else(|| digits.strip_prefix("0B"))
            .unwrap_or(digits),
        _ => digits,
    };
    let unsigned = u64::from_str_radix(digits, base as u32).ok()?;
    if negative {
        if unsigned > (i64::MAX as u64) + 1 {
            return None;
        }
        let number = if unsigned == (i64::MAX as u64) + 1 {
            i64::MIN
        } else {
            -(unsigned as i64)
        };
        Some(MiniValue::from(number))
    } else if unsigned <= i64::MAX as u64 {
        Some(MiniValue::from(unsigned as i64))
    } else {
        Some(MiniValue::from(unsigned))
    }
}

fn parse_bool(value: &MiniValue) -> Option<bool> {
    let raw = match serde_json::to_value(value).ok()? {
        Value::Bool(value) => return Some(value),
        Value::String(value) => value,
        _ => return None,
    };
    let value = raw.trim().to_ascii_lowercase();
    match value.as_str() {
        "1" | "true" | "yes" | "on" | "enable" | "enabled" | "open" | "home" => Some(true),
        "0" | "false" | "no" | "off" | "disable" | "disabled" | "closed" | "away" => Some(false),
        _ => None,
    }
}

fn configured_time_zone(runtime: &Runtime) -> chrono_tz::Tz {
    runtime
        .time_zone()
        .parse()
        .expect("Runtime validates IANA time zones")
}

fn register_datetime_helpers(environment: &mut Environment<'static>, runtime: Runtime) {
    environment.add_function("as_timestamp", as_timestamp_function);
    environment.add_filter("as_timestamp", as_timestamp_filter);
    environment.add_function("as_datetime", as_datetime_function);
    environment.add_filter("as_datetime", as_datetime_filter);
    let r = runtime.clone();
    environment.add_function("as_local", move |args: Rest<MiniValue>| {
        as_local_function(args, configured_time_zone(&r))
    });
    let r = runtime.clone();
    environment.add_filter(
        "as_local",
        move |value: MiniValue, args: Rest<MiniValue>| {
            as_local_filter(value, args, configured_time_zone(&r))
        },
    );
    environment.add_function("strptime", strptime_function);
    let r = runtime.clone();
    environment.add_filter(
        "timestamp_custom",
        move |value: MiniValue, args: Rest<MiniValue>| {
            timestamp_custom_filter(value, args, configured_time_zone(&r))
        },
    );
    let r = runtime.clone();
    environment.add_function(
        "timestamp_custom",
        move |value: MiniValue, args: Rest<MiniValue>| {
            timestamp_custom_filter(value, args, configured_time_zone(&r))
        },
    );
    let r = runtime.clone();
    environment.add_filter(
        "timestamp_local",
        move |value: MiniValue, args: Rest<MiniValue>| {
            timestamp_local_filter(value, args, configured_time_zone(&r))
        },
    );
    environment.add_filter("timestamp_utc", timestamp_utc_filter);
    environment.add_function("now", move || {
        Utc::now()
            .with_timezone(&configured_time_zone(&runtime))
            .to_rfc3339()
    });
    environment.add_function("utcnow", || Utc::now().to_rfc3339());
}

fn as_timestamp_function(args: Rest<MiniValue>) -> std::result::Result<MiniValue, MiniError> {
    as_timestamp(args.0)
}

fn as_timestamp_filter(
    value: MiniValue,
    args: Rest<MiniValue>,
) -> std::result::Result<MiniValue, MiniError> {
    let mut values = vec![value];
    values.extend(args.0);
    as_timestamp(values)
}

fn as_timestamp(mut args: Vec<MiniValue>) -> std::result::Result<MiniValue, MiniError> {
    let value = args
        .first()
        .cloned()
        .ok_or_else(|| invalid_template_call("as_timestamp expects a value"))?;
    args.remove(0);
    let mut options = CallOptions::from_values(args)?;
    let default = options.take_optional("default")?;
    options.finish()?;
    if let Some(datetime) = parse_datetime(&value) {
        return Ok(MiniValue::from(
            datetime.timestamp() as f64
                + f64::from(datetime.timestamp_subsec_nanos()) / 1_000_000_000.0,
        ));
    }
    default.ok_or_else(|| {
        invalid_template_call("as_timestamp received invalid input without a default")
    })
}

fn as_datetime_function(args: Rest<MiniValue>) -> std::result::Result<MiniValue, MiniError> {
    as_datetime(args.0)
}

fn as_datetime_filter(
    value: MiniValue,
    args: Rest<MiniValue>,
) -> std::result::Result<MiniValue, MiniError> {
    let mut values = vec![value];
    values.extend(args.0);
    as_datetime(values)
}

fn as_datetime(mut args: Vec<MiniValue>) -> std::result::Result<MiniValue, MiniError> {
    let value = args
        .first()
        .cloned()
        .ok_or_else(|| invalid_template_call("as_datetime expects a value"))?;
    args.remove(0);
    let mut options = CallOptions::from_values(args)?;
    let default = options.take_optional("default")?;
    options.finish()?;
    if let Some(datetime) = parse_datetime(&value) {
        return Ok(MiniValue::from(datetime.to_rfc3339()));
    }
    if value.as_str().is_some() && default.is_none() {
        // Home Assistant preserves the legacy behavior of returning None for an
        // invalid datetime string when the caller did not provide a default.
        return Ok(MiniValue::from(()));
    }
    default.ok_or_else(|| {
        invalid_template_call("as_datetime received invalid input without a default")
    })
}

fn as_local_function(
    args: Rest<MiniValue>,
    time_zone: chrono_tz::Tz,
) -> std::result::Result<MiniValue, MiniError> {
    let Some(value) = args.0.first() else {
        return Err(invalid_template_call("as_local expects a value"));
    };
    let datetime = parse_datetime(value)
        .ok_or_else(|| invalid_template_call("as_local received invalid input"))?;
    Ok(MiniValue::from(
        datetime.with_timezone(&time_zone).to_rfc3339(),
    ))
}

fn as_local_filter(
    value: MiniValue,
    _args: Rest<MiniValue>,
    time_zone: chrono_tz::Tz,
) -> std::result::Result<MiniValue, MiniError> {
    as_local_function(Rest(vec![value]), time_zone)
}

fn strptime_function(args: Rest<MiniValue>) -> std::result::Result<MiniValue, MiniError> {
    let mut values = args.0;
    if values.len() < 2 {
        return Err(invalid_template_call("strptime expects a value and format"));
    }
    let value = values.remove(0);
    let format = values.remove(0);
    let mut options = CallOptions::from_values(values)?;
    let default = options.take_optional("default")?;
    options.finish()?;
    let parsed = value
        .as_str()
        .zip(format.as_str())
        .and_then(|(value, format)| {
            DateTime::parse_from_str(value, format)
                .ok()
                .map(|datetime| datetime.with_timezone(&Utc).to_rfc3339())
                .or_else(|| {
                    NaiveDateTime::parse_from_str(value, format)
                        .ok()
                        .map(|datetime| Utc.from_utc_datetime(&datetime).to_rfc3339())
                })
        });
    match parsed {
        Some(datetime) => Ok(MiniValue::from(datetime)),
        None => default.ok_or_else(|| {
            invalid_template_call("strptime received invalid input without a default")
        }),
    }
}

fn timestamp_custom_filter(
    value: MiniValue,
    args: Rest<MiniValue>,
    time_zone: chrono_tz::Tz,
) -> std::result::Result<MiniValue, MiniError> {
    let Some(datetime) = parse_datetime(&value) else {
        let mut options = CallOptions::from_values(args.0)?;
        let _format = options.take_optional("date_format")?;
        let _local = options.take_optional("local")?;
        let default = options.take_optional("default")?;
        options.finish()?;
        return default.ok_or_else(|| {
            invalid_template_call("timestamp_custom received invalid input without a default")
        });
    };
    let mut options = CallOptions::from_values(args.0)?;
    let format = options
        .take_optional("date_format")?
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "%Y-%m-%d %H:%M:%S".into());
    let local = options
        .take_optional("local")?
        .as_ref()
        .and_then(parse_bool)
        .unwrap_or(true);
    let default = options.take_optional("default")?;
    options.finish()?;
    let output = if local {
        datetime
            .with_timezone(&time_zone)
            .format(&format)
            .to_string()
    } else {
        datetime.format(&format).to_string()
    };
    if output.len() > MAX_OUTPUT_BYTES {
        return default.ok_or_else(|| {
            invalid_template_call("timestamp_custom output exceeds the size limit")
        });
    }
    Ok(MiniValue::from(output))
}

fn timestamp_local_filter(
    value: MiniValue,
    args: Rest<MiniValue>,
    time_zone: chrono_tz::Tz,
) -> std::result::Result<MiniValue, MiniError> {
    let mut options = CallOptions::from_values(args.0)?;
    let default = options.take_optional("default")?;
    options.finish()?;
    match parse_datetime(&value) {
        Some(datetime) => Ok(MiniValue::from(
            datetime.with_timezone(&time_zone).to_rfc3339(),
        )),
        None => default.ok_or_else(|| {
            invalid_template_call("timestamp_local received invalid input without a default")
        }),
    }
}

fn timestamp_utc_filter(
    value: MiniValue,
    args: Rest<MiniValue>,
) -> std::result::Result<MiniValue, MiniError> {
    let mut options = CallOptions::from_values(args.0)?;
    let default = options.take_optional("default")?;
    options.finish()?;
    match parse_datetime(&value) {
        Some(datetime) => Ok(MiniValue::from(datetime.to_rfc3339())),
        None => default.ok_or_else(|| {
            invalid_template_call("timestamp_utc received invalid input without a default")
        }),
    }
}

fn parse_datetime(value: &MiniValue) -> Option<DateTime<Utc>> {
    if let Some(timestamp) = parse_float(value)
        && let Some(datetime) = datetime_from_timestamp(timestamp)
    {
        return Some(datetime);
    }
    let value = value.as_str()?.trim();
    if let Ok(datetime) = DateTime::parse_from_rfc3339(value) {
        return Some(datetime.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
    ] {
        if let Ok(datetime) = NaiveDateTime::parse_from_str(value, format) {
            return Some(Utc.from_utc_datetime(&datetime));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?));
    }
    None
}

fn datetime_from_timestamp(timestamp: f64) -> Option<DateTime<Utc>> {
    if !timestamp.is_finite() || timestamp < i64::MIN as f64 || timestamp >= -(i64::MIN as f64) {
        return None;
    }
    let seconds = timestamp.floor() as i64;
    let nanos = ((timestamp - seconds as f64) * 1_000_000_000.0).round() as u32;
    Utc.timestamp_opt(seconds, nanos).single()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hass_core::{Context, MemoryStore, Store};
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    fn runtime() -> Runtime {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::default());
        Runtime::new(store).expect("runtime should initialize")
    }

    fn set_state(runtime: &Runtime, entity_id: &str, state: &str, attributes: Value) {
        runtime
            .set_state(
                entity_id,
                state,
                attributes.as_object().cloned().unwrap_or_default(),
                Context::default(),
                false,
            )
            .expect("state write should succeed");
    }

    #[derive(Deserialize)]
    struct FixtureFile {
        source: String,
        cases: Vec<FixtureCase>,
    }

    #[derive(Deserialize)]
    struct FixtureCase {
        template: String,
        expected: String,
    }

    #[test]
    fn matches_pinned_upstream_template_expectations() {
        let fixture: FixtureFile = serde_json::from_str(include_str!(
            "../tests/fixtures/upstream_template_expectations.json"
        ))
        .expect("fixture JSON should be valid");
        assert!(
            fixture
                .source
                .contains("cbf2c02c9bff331537c19bff69da8dc11c3fbfbf")
        );

        let runtime = runtime();
        set_state(
            &runtime,
            "sensor.temperature",
            "21.5",
            json!({"unit_of_measurement": "°C"}),
        );
        set_state(&runtime, "light.desk", "on", json!({}));
        let engine = TemplateEngine::new(runtime);
        for case in fixture.cases {
            assert_eq!(
                engine.render(&case.template, &Map::new()).unwrap(),
                case.expected,
                "template: {}",
                case.template
            );
        }
    }

    #[test]
    fn reads_state_updates_and_supports_state_iteration() {
        let runtime = runtime();
        let engine = TemplateEngine::new(runtime.clone());
        assert_eq!(
            engine
                .render("{{ states('sensor.temp') }}", &Map::new())
                .unwrap(),
            "unknown"
        );

        set_state(&runtime, "sensor.temp", "12", json!({}));
        set_state(&runtime, "sensor.other", "unknown", json!({}));
        assert_eq!(
            engine
                .render("{{ states.sensor.temp.state }}", &Map::new())
                .unwrap(),
            "12"
        );
        assert_eq!(
            engine
                .render("{{ states.sensor | length }}", &Map::new())
                .unwrap(),
            "2"
        );
        assert_eq!(
            engine
                .render(
                    "{{ states | map(attribute='entity_id') | list | join(',') }}",
                    &Map::new()
                )
                .unwrap(),
            "sensor.other,sensor.temp"
        );
    }

    #[test]
    fn render_value_preserves_json_types_and_variables() {
        let runtime = runtime();
        set_state(&runtime, "light.desk", "on", json!({}));
        let engine = TemplateEngine::new(runtime);
        let mut variables = Map::new();
        variables.insert("enabled".into(), Value::Bool(true));
        assert_eq!(
            engine.render_value("{{ enabled }}", &variables).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            engine.render_value("{{ 12.5 }}", &Map::new()).unwrap(),
            json!(12.5)
        );
        assert_eq!(
            engine
                .render_value("{{ state_attr('sensor.none', 'x') }}", &Map::new())
                .unwrap(),
            Value::Null
        );
        assert_eq!(
            engine
                .render_value("{{ is_state('light.desk', 'on') }}", &Map::new())
                .unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            engine
                .render_value(
                    "{{ {'active': is_state('light.desk', 'on'), 'count': 2} }}",
                    &Map::new()
                )
                .unwrap(),
            json!({"active": true, "count": 2})
        );
    }

    #[test]
    fn numeric_fallbacks_do_not_invent_zero_values() {
        let engine = TemplateEngine::new(runtime());
        assert!(engine.render("{{ 'bad' | float }}", &Map::new()).is_err());
        assert!(engine.render("{{ int('bad') }}", &Map::new()).is_err());
        assert_eq!(
            engine
                .render("{{ 'bad' | float(default=3) }}", &Map::new())
                .unwrap(),
            "3"
        );
        assert_eq!(
            engine.render("{{ int('12.8') }}", &Map::new()).unwrap(),
            "12"
        );
        assert_eq!(
            engine
                .render("{{ int('0x10', base=16) }}", &Map::new())
                .unwrap(),
            "16"
        );
        assert_eq!(
            engine.render("{{ bool('off') }}", &Map::new()).unwrap(),
            "False"
        );
        assert!(engine.render("{{ bool('unknown') }}", &Map::new()).is_err());
        assert_eq!(
            engine
                .render_value("{{ bool('unknown', default=none) }}", &Map::new())
                .unwrap(),
            Value::Null
        );
        assert_eq!(
            engine
                .render_value("{{ 'bad' | float(default=none) }}", &Map::new())
                .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn timestamp_helpers_preserve_values_and_defaults() {
        let engine = TemplateEngine::new(runtime());
        assert_eq!(
            engine
                .render("{{ 1469119144 | timestamp_utc }}", &Map::new())
                .unwrap(),
            "2016-07-21T16:39:04+00:00"
        );
        assert_eq!(
            engine
                .render(
                    "{{ as_timestamp('2016-07-21T16:39:04+00:00') }}",
                    &Map::new()
                )
                .unwrap(),
            "1469119144.0"
        );
        assert_eq!(
            engine
                .render("{{ 'invalid' | as_timestamp(default=7) }}", &Map::new())
                .unwrap(),
            "7"
        );
        assert!(
            engine
                .render("{{ 'invalid' | as_timestamp }}", &Map::new())
                .is_err()
        );
    }

    #[test]
    fn rejects_invalid_templates_fuel_exhaustion_and_large_output() {
        let engine = TemplateEngine::new(runtime());
        assert!(engine.render("{% if %}", &Map::new()).is_err());
        let fuel_error = engine
            .render(
                "{% for i in range(10000) %}{% set x = (i + 1) * (i + 2) %}{% endfor %}",
                &Map::new(),
            )
            .expect_err("the loop should exhaust the per-render fuel budget");
        assert!(fuel_error.to_string().contains("OutOfFuel"), "{fuel_error}");
        assert!(engine
            .render(
                "{% macro recurse(n) %}{% if n > 0 %}{{ recurse(n - 1) }}{% endif %}{% endmacro %}{{ recurse(100) }}",
                &Map::new()
            )
            .is_err());
        assert!(engine.render("{{ 'x' * 70000 }}", &Map::new()).is_err());
        assert!(
            engine
                .render_value("{{ 'x' * 70000 }}", &Map::new())
                .is_err()
        );
        assert_eq!(
            engine
                .render_value("{% if true %}{{ 1 }}{% endif %}", &Map::new())
                .unwrap(),
            json!(1)
        );
        assert!(
            engine
                .render_value("prefix{{ 'x' * 70000 }}suffix", &Map::new())
                .is_err()
        );
    }

    #[test]
    fn composite_templates_render_actions_and_parse_supported_literal_results() {
        let engine = TemplateEngine::new(runtime());
        let variables =
            json!({"trigger":{"json":{"message":"hello"},"query":{"origin":"network"}}})
                .as_object()
                .unwrap()
                .clone();
        assert_eq!(
            engine
                .render_value(
                    "{{ trigger.json.message }}|{{ trigger.query.origin }}",
                    &variables,
                )
                .unwrap(),
            json!("hello|network")
        );
        for (source, expected) in [
            ("{{ 1 }}{{ 2 }}", json!(12)),
            ("{% if true %} +001 {% endif %}", json!(1)),
            ("{% if true %} -.5 {% endif %}", json!(-0.5)),
            ("{% if true %}True{% endif %}", json!(true)),
            ("{% if true %}None{% endif %}", Value::Null),
            ("{% if true %}1e3{% endif %}", json!("1e3")),
            ("{% if true %}'quoted'{% endif %}", json!("'quoted'")),
            ("{% if true %}[null]{% endif %}", json!("[null]")),
            ("{% if true %}[true]{% endif %}", json!("[true]")),
            (
                "[{% for n in range(3) %}{{ n }}{% if not loop.last %},{% endif %}{% endfor %}]",
                json!([0, 1, 2]),
            ),
        ] {
            assert_eq!(
                engine.render_value(source, &Map::new()).unwrap(),
                expected,
                "{source}"
            );
        }
    }
}
