//! Explicitly supported configuration, validated before opening a database.

use crate::helpers::Helpers;
use anyhow::{Context, Result, bail};
use hass_automation::Automation;
#[cfg(test)]
use hass_units::UnitSystemKind;
use hass_units::validate_unit_system;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, net::IpAddr, path::Path};

#[cfg(test)]
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub homeassistant: HomeConfig,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub recorder: RecorderConfig,
    #[serde(default)]
    pub automation: Vec<Automation>,
    #[serde(default)]
    pub script: BTreeMap<String, hass_script::ScriptConfig>,
    #[serde(default)]
    pub scene: Vec<hass_script::SceneConfig>,
    #[serde(default)]
    pub mqtt: Option<hass_mqtt::MqttConfig>,
    #[serde(default)]
    pub rest: Vec<hass_rest::RestConfig>,
    #[serde(default)]
    pub rest_command: BTreeMap<String, hass_rest::RestCommandConfig>,
    #[serde(default)]
    pub template: Vec<hass_template_entities::TemplateBlock>,
    #[serde(default)]
    pub group: BTreeMap<String, hass_group::GroupConfig>,
    #[serde(default, deserialize_with = "deserialize_zones")]
    pub zone: Vec<hass_zone::ZoneConfig>,
    #[serde(default, deserialize_with = "deserialize_people")]
    pub person: Vec<hass_person::PersonConfig>,
    #[serde(default)]
    pub device_tracker: hass_device_tracker::TrackerConfig,
    #[serde(default)]
    pub image_upload: serde_yaml::Value,
    #[serde(default)]
    pub tag: serde_yaml::Value,
    #[serde(default)]
    pub logger: Option<hass_logger::LoggerConfig>,
    #[serde(default)]
    pub system_log: Option<hass_system_log::SystemLogConfig>,
    #[serde(default)]
    pub system_health: serde_yaml::Value,
    #[serde(default)]
    pub ssdp: serde_yaml::Value,
    #[serde(skip)]
    pub ssdp_enabled: bool,
    #[serde(default, deserialize_with = "deserialize_zeroconf_config")]
    pub zeroconf: ZeroconfConfig,
    #[serde(skip)]
    pub zeroconf_enabled: bool,
    #[serde(default)]
    pub dhcp: serde_yaml::Value,
    #[serde(skip)]
    pub dhcp_enabled: bool,
    #[serde(default)]
    pub usb: serde_yaml::Value,
    #[serde(skip)]
    pub usb_enabled: bool,
    #[serde(default)]
    pub hardware: serde_yaml::Value,
    #[serde(skip)]
    pub hardware_enabled: bool,
    #[serde(default)]
    pub application_credentials: serde_yaml::Value,
    #[serde(skip)]
    pub application_credentials_enabled: bool,
    #[serde(default, deserialize_with = "deserialize_sun")]
    pub sun: bool,
    #[serde(default, deserialize_with = "deserialize_sensor_platforms")]
    pub sensor: Vec<crate::system_sensors::SystemSensorYamlPlatform>,
    #[serde(skip)]
    pub group_order: Vec<String>,
    #[serde(default)]
    pub timer: BTreeMap<String, hass_native::TimerConfig>,
    #[serde(default)]
    pub input_button: BTreeMap<String, hass_native::InputButtonConfig>,
    #[serde(default)]
    pub input_datetime: BTreeMap<String, hass_native::InputDatetimeConfig>,
    #[serde(default)]
    pub schedule: BTreeMap<String, hass_native::ScheduleConfig>,
    #[serde(flatten)]
    pub helpers: Helpers,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HomeConfig {
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub elevation: f64,
    pub time_zone: String,
    pub unit_system: String,
}

impl Default for HomeConfig {
    fn default() -> Self {
        Self {
            name: "Home".into(),
            latitude: 0.0,
            longitude: 0.0,
            elevation: 0.0,
            time_zone: "UTC".into(),
            unit_system: "metric".into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    pub server_host: IpAddr,
    pub server_port: u16,
    pub use_x_forwarded_for: bool,
    pub trusted_proxies: Vec<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            server_host: "127.0.0.1".parse().expect("literal IP"),
            server_port: 8123,
            use_x_forwarded_for: false,
            trusted_proxies: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecorderConfig {
    pub db_path: String,
    pub purge_keep_days: u64,
    pub auto_purge: bool,
    pub auto_repack: bool,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self {
            db_path: "data/hass.sqlite".into(),
            purge_keep_days: 10,
            auto_purge: true,
            auto_repack: true,
        }
    }
}

/// Deprecated Zeroconf YAML options retained for compatibility validation.
/// Home Assistant chooses Zeroconf address families from the network adapters,
/// so these values are not used to configure the runtime.
#[derive(Clone, Debug)]
pub struct ZeroconfConfig {
    pub default_interface: Option<bool>,
    /// The upstream schema defaults this value to true when it is omitted.
    /// It is retained after validation; network adapters choose the families.
    #[allow(dead_code)]
    pub ipv6: bool,
    ipv6_was_configured: bool,
}

impl Default for ZeroconfConfig {
    fn default() -> Self {
        Self {
            default_interface: None,
            ipv6: true,
            ipv6_was_configured: false,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ZeroconfConfigInput {
    #[serde(deserialize_with = "deserialize_optional_cv_boolean")]
    default_interface: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_cv_boolean")]
    ipv6: Option<bool>,
}

impl<'de> Deserialize<'de> for ZeroconfConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let input = ZeroconfConfigInput::deserialize(deserializer)?;
        Ok(Self {
            default_interface: input.default_interface,
            ipv6: input.ipv6.unwrap_or(true),
            ipv6_was_configured: input.ipv6.is_some(),
        })
    }
}

impl ZeroconfConfig {
    fn has_deprecated_parameters(&self) -> bool {
        self.default_interface.is_some() || self.ipv6_was_configured
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut config = Self::from_value(crate::yaml::load(path)?)?;
        let known = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("known_devices.yaml");
        if known
            .try_exists()
            .context("cannot inspect known_devices.yaml")?
        {
            let value = crate::yaml::load(&known)?;
            let mut value = serde_json::to_value(value).context("invalid known-device mapping")?;
            if value.is_null() {
                value = json!({});
            }
            config
                .device_tracker
                .devices
                .extend(hass_device_tracker::parse_known_devices(value)?);
            config.device_tracker.validate()?;
        }
        Ok(config)
    }

    /// Reload only the zone component, preserving the loader's include,
    /// package and secret handling without reconfiguring other integrations.
    pub fn load_zones(path: &Path) -> Result<Vec<hass_zone::ZoneConfig>> {
        let root = crate::yaml::load(path)?;
        let value = root.get("zone").cloned().unwrap_or_default();
        let zones =
            deserialize_zones(value).map_err(|_| anyhow::anyhow!("invalid zone configuration"))?;
        for zone in &zones {
            zone.validate()?;
        }
        Ok(zones)
    }

    pub fn load_people(path: &Path) -> Result<Vec<hass_person::PersonConfig>> {
        let root = crate::yaml::load(path)?;
        let value = root.get("person").cloned().unwrap_or_default();
        let people = deserialize_people(value)
            .map_err(|_| anyhow::anyhow!("invalid person configuration"))?;
        for person in &people {
            person.validate()?;
        }
        Ok(people)
    }

    #[cfg(test)]
    pub fn parse(source: &str) -> Result<Self> {
        if source.len() as u64 > MAX_CONFIG_BYTES {
            bail!("configuration exceeds the 1 MiB limit");
        }
        Self::from_value(crate::yaml::parse(source)?)
    }

    fn from_value(mut value: serde_yaml::Value) -> Result<Self> {
        if value.is_null() {
            value = serde_yaml::Value::Mapping(Default::default());
        }
        let group_order = value
            .get("group")
            .and_then(serde_yaml::Value::as_mapping)
            .map(|groups| {
                groups
                    .keys()
                    .filter_map(serde_yaml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        // Packages have already been merged by the loader. They are not a
        // second integration configuration or public/debug settings payload.
        if let Some(home) = value
            .as_mapping_mut()
            .and_then(|root| root.get_mut(serde_yaml::Value::String("homeassistant".into())))
            .and_then(serde_yaml::Value::as_mapping_mut)
        {
            home.remove(serde_yaml::Value::String("packages".into()));
        }
        let mapping = value
            .as_mapping()
            .context("configuration must be a mapping")?;
        // serde flatten cannot be combined reliably with deny_unknown_fields.
        // Check root keys explicitly so unsupported integrations never disappear.
        for key in mapping.keys() {
            let key = key.as_str().context("configuration keys must be strings")?;
            if ![
                "homeassistant",
                "http",
                "recorder",
                "automation",
                "input_boolean",
                "input_number",
                "input_text",
                "input_select",
                "script",
                "scene",
                "mqtt",
                "rest",
                "rest_command",
                "template",
                "group",
                "zone",
                "person",
                "device_tracker",
                "image_upload",
                "tag",
                "logger",
                "system_log",
                "system_health",
                "ssdp",
                "zeroconf",
                "dhcp",
                "usb",
                "hardware",
                "application_credentials",
                "sun",
                "sensor",
                "timer",
                "input_button",
                "input_datetime",
                "schedule",
                "counter",
            ]
            .contains(&key)
            {
                bail!("unsupported configuration key: {key}");
            }
        }
        let ssdp_enabled = mapping.contains_key(serde_yaml::Value::String("ssdp".into()));
        let zeroconf_enabled = mapping.contains_key(serde_yaml::Value::String("zeroconf".into()));
        let dhcp_enabled = mapping.contains_key(serde_yaml::Value::String("dhcp".into()));
        let usb_enabled = mapping.contains_key(serde_yaml::Value::String("usb".into()));
        let hardware_enabled = mapping.contains_key(serde_yaml::Value::String("hardware".into()));
        let application_credentials_enabled =
            mapping.contains_key(serde_yaml::Value::String("application_credentials".into()));
        let mut config: Self = serde_path_to_error::deserialize(value).map_err(|error| {
            // A typed decoding error can quote an invalid scalar. Preserve
            // its configuration path without printing resolved secret values.
            anyhow::anyhow!(
                "unsupported or invalid configuration option at {}",
                error.path()
            )
        })?;
        config.group_order = group_order;
        config.ssdp_enabled = ssdp_enabled;
        config.zeroconf_enabled = zeroconf_enabled;
        config.dhcp_enabled = dhcp_enabled;
        config.usb_enabled = usb_enabled;
        config.hardware_enabled = hardware_enabled;
        config.application_credentials_enabled = application_credentials_enabled;
        if let Ok(kind) = validate_unit_system(&config.homeassistant.unit_system) {
            config.homeassistant.unit_system = kind.as_str().into();
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if let Some(logger) = &self.logger {
            hass_logger::validate_config(logger)?;
        }
        if let Some(system_log) = &self.system_log {
            hass_system_log::validate_config(system_log)?;
        }
        if !(1..=365_000).contains(&self.recorder.purge_keep_days) {
            bail!("recorder.purge_keep_days must be between 1 and 365000");
        }
        let home = &self.homeassistant;
        if home.name.trim().is_empty() {
            bail!("homeassistant.name must not be empty");
        }
        if !home.latitude.is_finite() || !(-90.0..=90.0).contains(&home.latitude) {
            bail!("latitude must be finite and between -90 and 90");
        }
        if !home.longitude.is_finite() || !(-180.0..=180.0).contains(&home.longitude) {
            bail!("longitude must be finite and between -180 and 180");
        }
        if !home.elevation.is_finite() {
            bail!("elevation must be finite");
        }
        home.time_zone
            .parse::<chrono_tz::Tz>()
            .context("unknown IANA time zone")?;
        validate_unit_system(&home.unit_system).map_err(|_| {
            anyhow::anyhow!(
                "homeassistant.unit_system must be metric or us_customary (imperial is a deprecated alias)"
            )
        })?;
        if self.recorder.db_path.trim().is_empty() || self.recorder.db_path == ":memory:" {
            bail!("recorder.db_path must be a persistent filesystem path");
        }
        self.helpers.validate()?;
        hass_api::network::NetworkPolicy::new(
            self.http.use_x_forwarded_for,
            &self.http.trusted_proxies,
        )?;
        hass_automation::validate(&self.automation)?;
        hass_script::validate_config(&self.script, &self.scene)?;
        self.native_config().validate()?;
        if let Some(mqtt) = &self.mqtt {
            mqtt.validate()?;
        }
        hass_rest::validate_config(&self.rest, &self.rest_command)?;
        hass_template_entities::validate(&self.template)?;
        hass_group::validate(&self.group)?;
        for zone in &self.zone {
            zone.validate()?;
        }
        let mut person_ids = std::collections::BTreeSet::new();
        for person in &self.person {
            person.validate()?;
            if !person_ids.insert(&person.id) {
                bail!("duplicate person unique ID");
            }
        }
        self.device_tracker.validate()?;
        if !self.tag.is_null()
            && self
                .tag
                .as_mapping()
                .is_none_or(|mapping| !mapping.is_empty())
        {
            bail!("tag accepts only an empty configuration section");
        }
        if !self.image_upload.is_null()
            && self
                .image_upload
                .as_mapping()
                .is_none_or(|mapping| !mapping.is_empty())
        {
            bail!("image_upload accepts only an empty configuration section");
        }
        Ok(())
    }

    /// The source empty-config validator accepts any value and warns only for
    /// truthy parameters; it does not reject them or configure a provider.
    pub fn system_health_has_parameters(&self) -> bool {
        yaml_truthy(&self.system_health)
    }

    pub fn ssdp_has_parameters(&self) -> bool {
        yaml_truthy(&self.ssdp)
    }

    pub fn dhcp_has_parameters(&self) -> bool {
        yaml_truthy(&self.dhcp)
    }

    pub fn usb_has_parameters(&self) -> bool {
        yaml_truthy(&self.usb)
    }

    pub fn hardware_has_parameters(&self) -> bool {
        yaml_truthy(&self.hardware)
    }

    pub fn application_credentials_has_parameters(&self) -> bool {
        yaml_truthy(&self.application_credentials)
    }

    pub fn zeroconf_has_deprecated_parameters(&self) -> bool {
        self.zeroconf.has_deprecated_parameters()
    }

    pub fn native_config(&self) -> hass_native::NativeConfig {
        hass_native::NativeConfig {
            timer: self.timer.clone(),
            input_button: self.input_button.clone(),
            input_datetime: self.input_datetime.clone(),
            schedule: self.schedule.clone(),
            time_zone: self.homeassistant.time_zone.clone(),
        }
    }

    pub fn api_config(&self) -> Value {
        let unit_system = validate_unit_system(&self.homeassistant.unit_system)
            .expect("HomeConfig unit system is validated during configuration parsing");
        let units = json!(unit_system.unit_system().as_dict());
        let mut components = vec![
            "api",
            "websocket_api",
            "network",
            "recorder",
            "auth",
            "template",
            "homeassistant",
            "group",
            "persistent_notification",
            "zone",
            "person",
            "device_tracker",
            "image_upload",
            "tag",
            "logger",
            "system_log",
            "repairs",
            "system_health",
            "sun",
            "sensor",
            "binary_sensor",
            "webhook",
            "time_date",
            "uptime",
        ];
        if self.ssdp_enabled {
            components.push("ssdp");
        }
        if self.zeroconf_enabled {
            components.push("zeroconf");
        }
        if self.dhcp_enabled {
            components.push("dhcp");
        }
        if self.usb_enabled {
            components.push("usb");
        }
        if self.hardware_enabled {
            components.push("hardware");
        }
        if self.application_credentials_enabled {
            components.push("application_credentials");
        }
        if !self.script.is_empty() {
            components.push("script");
        }
        if !self.scene.is_empty() {
            components.push("scene");
        }
        if self.mqtt.is_some() {
            components.push("mqtt");
        }
        if !self.rest.is_empty() {
            components.push("rest");
        }
        if !self.rest_command.is_empty() {
            components.push("rest_command");
        }
        if !self.timer.is_empty() {
            components.push("timer");
        }
        if !self.input_button.is_empty() {
            components.push("input_button");
        }
        if !self.input_datetime.is_empty() {
            components.push("input_datetime");
        }
        if !self.schedule.is_empty() {
            components.push("schedule");
        }
        if !self.automation.is_empty() {
            components.push("automation");
        }
        if !self.helpers.input_boolean.is_empty() {
            components.push("input_boolean");
        }
        if !self.helpers.input_number.is_empty() {
            components.push("input_number");
        }
        if !self.helpers.input_text.is_empty() {
            components.push("input_text");
        }
        if !self.helpers.input_select.is_empty() {
            components.push("input_select");
        }
        if !self.helpers.counter.is_empty() {
            components.push("counter");
        }
        json!({
            "location_name": self.homeassistant.name,
            "latitude": self.homeassistant.latitude,
            "longitude": self.homeassistant.longitude,
            "elevation": self.homeassistant.elevation,
            "time_zone": self.homeassistant.time_zone,
            "unit_system": units,
            "version": hass_core::VERSION,
            "state": "RUNNING",
            "components": components,
            "safe_mode": false,
            "recovery_mode": false,
        })
    }
}

fn yaml_truthy(value: &serde_yaml::Value) -> bool {
    match value {
        serde_yaml::Value::Null => false,
        serde_yaml::Value::Bool(value) => *value,
        serde_yaml::Value::Number(value) => value.as_f64().is_none_or(|value| value != 0.0),
        serde_yaml::Value::String(value) => !value.is_empty(),
        serde_yaml::Value::Sequence(value) => !value.is_empty(),
        serde_yaml::Value::Mapping(value) => !value.is_empty(),
        serde_yaml::Value::Tagged(value) => yaml_truthy(&value.value),
    }
}

fn deserialize_zeroconf_config<'de, D>(
    deserializer: D,
) -> std::result::Result<ZeroconfConfig, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(ZeroconfConfig::default());
    }
    serde_yaml::from_value(value).map_err(serde::de::Error::custom)
}

fn deserialize_optional_cv_boolean<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_any(CvBooleanVisitor).map(Some)
}

struct CvBooleanVisitor;

impl serde::de::Visitor<'_> for CvBooleanVisitor {
    type Value = bool;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a boolean, number, or recognized boolean string")
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
        Ok(value)
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
        Ok(value != 0)
    }

    fn visit_i128<E>(self, value: i128) -> std::result::Result<Self::Value, E> {
        Ok(value != 0)
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
        Ok(value != 0)
    }

    fn visit_u128<E>(self, value: u128) -> std::result::Result<Self::Value, E> {
        Ok(value != 0)
    }

    fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E> {
        Ok(value != 0.0)
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        match value.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enable" => Ok(true),
            "0" | "false" | "no" | "off" | "disable" => Ok(false),
            _ => Err(E::custom("invalid boolean value")),
        }
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_str(&value)
    }
}

fn deserialize_sun<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    if value.is_null() || value.as_mapping().is_some_and(|mapping| mapping.is_empty()) {
        Ok(true)
    } else {
        Err(serde::de::Error::custom(
            "sun accepts only an empty configuration section",
        ))
    }
}

fn deserialize_sensor_platforms<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<crate::system_sensors::SystemSensorYamlPlatform>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    let values = match value {
        serde_yaml::Value::Null => Vec::new(),
        serde_yaml::Value::Sequence(values) => values,
        other => vec![other],
    };
    serde_yaml::from_value(serde_yaml::Value::Sequence(values)).map_err(serde::de::Error::custom)
}

fn deserialize_zones<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<hass_zone::ZoneConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    let value = match value {
        serde_yaml::Value::Null => serde_yaml::Value::Sequence(Vec::new()),
        serde_yaml::Value::Mapping(ref mapping) if mapping.is_empty() => {
            serde_yaml::Value::Sequence(Vec::new())
        }
        serde_yaml::Value::Sequence(_) => value,
        other => serde_yaml::Value::Sequence(vec![other]),
    };
    serde_yaml::from_value(value).map_err(serde::de::Error::custom)
}

fn deserialize_people<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<hass_person::PersonConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    let values = match value {
        serde_yaml::Value::Sequence(values) => values,
        other => vec![other],
    };
    let values = values
        .into_iter()
        .filter(|value| match value {
            serde_yaml::Value::Null => false,
            serde_yaml::Value::Bool(value) => *value,
            serde_yaml::Value::Number(value) => value.as_f64().is_none_or(|value| value != 0.0),
            serde_yaml::Value::String(value) => !value.is_empty(),
            serde_yaml::Value::Sequence(value) => !value.is_empty(),
            serde_yaml::Value::Mapping(value) => !value.is_empty(),
            serde_yaml::Value::Tagged(_) => true,
        })
        .collect();
    serde_yaml::from_value(serde_yaml::Value::Sequence(values)).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sun_yaml_import_requires_an_explicit_empty_section() {
        assert!(!Config::parse("{}").unwrap().sun);
        for source in ["sun:\n", "sun: {}\n"] {
            assert!(Config::parse(source).unwrap().sun);
        }
        for source in ["sun: true\n", "sun: []\n", "sun:\n  unknown: true\n"] {
            assert!(Config::parse(source).is_err(), "{source}");
        }
    }

    #[test]
    fn system_health_yaml_accepts_ignored_values_and_preserves_source_truthiness() {
        for (source, warns) in [
            ("{}\n", false),
            ("system_health:\n", false),
            ("system_health: {}\n", false),
            ("system_health: []\n", false),
            ("system_health: false\n", false),
            ("system_health: 0\n", false),
            ("system_health: ''\n", false),
            ("system_health: {ignored: value}\n", true),
            ("system_health: [ignored]\n", true),
            ("system_health: value\n", true),
            ("system_health: true\n", true),
            ("system_health: -1\n", true),
        ] {
            assert_eq!(
                Config::parse(source)
                    .unwrap()
                    .system_health_has_parameters(),
                warns,
                "{source}"
            );
        }
    }

    #[test]
    fn ssdp_yaml_tracks_explicit_enablement_and_ignored_parameter_truthiness() {
        let absent = Config::parse("{}\n").unwrap();
        assert!(!absent.ssdp_enabled);
        assert!(
            !absent.api_config()["components"]
                .as_array()
                .unwrap()
                .contains(&json!("ssdp"))
        );
        for (source, warns) in [
            ("ssdp:\n", false),
            ("ssdp: {}\n", false),
            ("ssdp: []\n", false),
            ("ssdp: false\n", false),
            ("ssdp: 0\n", false),
            ("ssdp: ''\n", false),
            ("ssdp: {ignored: value}\n", true),
            ("ssdp: [ignored]\n", true),
            ("ssdp: value\n", true),
            ("ssdp: true\n", true),
            ("ssdp: -1\n", true),
        ] {
            let config = Config::parse(source).unwrap();
            assert!(config.ssdp_enabled, "{source}");
            assert_eq!(config.ssdp_has_parameters(), warns, "{source}");
            assert!(
                config.api_config()["components"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("ssdp"))
            );
        }
    }

    #[test]
    fn dhcp_yaml_enables_only_an_explicit_section_and_ignores_legacy_parameters() {
        let absent = Config::parse("{}\n").unwrap();
        assert!(!absent.dhcp_enabled);
        assert!(
            !absent.api_config()["components"]
                .as_array()
                .unwrap()
                .contains(&json!("dhcp"))
        );
        for (value, warns) in [
            ("null", false),
            ("{}", false),
            ("[]", false),
            ("false", false),
            ("0", false),
            ("''", false),
            ("{ignored: value}", true),
            ("[ignored]", true),
            ("value", true),
            ("true", true),
            ("-1", true),
        ] {
            let config = Config::parse(&format!("dhcp: {value}\n")).unwrap();
            assert!(config.dhcp_enabled, "{value}");
            assert_eq!(config.dhcp_has_parameters(), warns, "{value}");
            assert!(
                config.api_config()["components"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("dhcp"))
            );
        }
    }

    #[test]
    fn usb_yaml_tracks_presence_and_accepts_ignored_parameters() {
        let absent = Config::parse("{}\n").unwrap();
        assert!(!absent.usb_enabled);
        for (value, warns) in [
            ("null", false),
            ("false", false),
            ("{}", false),
            ("[]", false),
            ("{ignored: value}", true),
            ("[ignored]", true),
            ("value", true),
        ] {
            let config = Config::parse(&format!("usb: {value}\n")).unwrap();
            assert!(config.usb_enabled);
            assert_eq!(config.usb_has_parameters(), warns);
            assert!(
                config.api_config()["components"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("usb"))
            );
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("configuration.yaml");
        std::fs::write(&path, "usb: {ignored: value}\n").unwrap();
        assert!(Config::load(&path).unwrap().usb_enabled);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn hardware_yaml_presence_enables_setup_and_parameters_are_ignored() {
        assert!(!Config::parse("{}\n").unwrap().hardware_enabled);
        for (value, warns) in [
            ("null", false),
            ("false", false),
            ("{}", false),
            ("{ignored: value}", true),
        ] {
            let config = Config::parse(&format!("hardware: {value}\n")).unwrap();
            assert!(config.hardware_enabled);
            assert_eq!(config.hardware_has_parameters(), warns);
            assert!(
                config.api_config()["components"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("hardware"))
            );
        }
    }

    #[test]
    fn application_credentials_yaml_does_not_export_ignored_secret_parameters() {
        assert!(
            !Config::parse("{}\n")
                .unwrap()
                .application_credentials_enabled
        );
        for (value, warns) in [
            ("null", false),
            ("false", false),
            ("{}", false),
            ("{client_secret: ignored-fixture-secret}", true),
        ] {
            let config = Config::parse(&format!("application_credentials: {value}\n")).unwrap();
            assert!(config.application_credentials_enabled);
            assert_eq!(config.application_credentials_has_parameters(), warns);
            let public = config.api_config();
            assert!(
                public["components"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("application_credentials"))
            );
            assert!(!public.to_string().contains("ignored-fixture-secret"));
        }
    }

    #[test]
    fn zeroconf_yaml_tracks_section_presence_defaults_and_deprecated_parameters() {
        let absent = Config::parse("{}\n").unwrap();
        assert!(!absent.zeroconf_enabled);
        assert!(absent.zeroconf.ipv6);
        assert!(!absent.zeroconf_has_deprecated_parameters());
        assert!(
            !absent.api_config()["components"]
                .as_array()
                .unwrap()
                .contains(&json!("zeroconf"))
        );

        for source in ["zeroconf:\n", "zeroconf: {}\n"] {
            let config = Config::parse(source).unwrap();
            assert!(config.zeroconf_enabled, "{source}");
            assert!(config.zeroconf.ipv6, "{source}");
            assert!(!config.zeroconf_has_deprecated_parameters(), "{source}");
            assert!(
                config.api_config()["components"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("zeroconf")),
                "{source}"
            );
        }

        let null_section = Config::parse("zeroconf: null\n").unwrap();
        assert!(null_section.zeroconf_enabled);
        assert!(null_section.zeroconf.ipv6);
        assert!(!null_section.zeroconf_has_deprecated_parameters());

        let explicit_default = Config::parse("zeroconf:\n  ipv6: true\n").unwrap();
        assert!(explicit_default.zeroconf_has_deprecated_parameters());
    }

    #[test]
    fn zeroconf_boolean_values_match_the_pinned_cv_boolean_validator() {
        for (value, expected) in [
            ("true", true),
            ("false", false),
            ("'  YeS  '", true),
            ("'ON'", true),
            ("'Enable'", true),
            ("'OFF'", false),
            ("'disable'", false),
            ("1", true),
            ("0", false),
            ("-2", true),
            ("0.0", false),
            ("2.5", true),
        ] {
            let config = Config::parse(&format!("zeroconf:\n  ipv6: {value}\n")).unwrap();
            assert_eq!(config.zeroconf.ipv6, expected, "ipv6: {value}");
            assert!(config.zeroconf_has_deprecated_parameters(), "ipv6: {value}");
        }

        let config = Config::parse("zeroconf:\n  default_interface: ' YES '\n").unwrap();
        assert_eq!(config.zeroconf.default_interface, Some(true));
        assert!(config.zeroconf_has_deprecated_parameters());
    }

    #[test]
    fn zeroconf_rejects_invalid_boolean_values_and_unknown_fields() {
        for source in [
            "zeroconf: []\n",
            "zeroconf: true\n",
            "zeroconf:\n  ipv6: null\n",
            "zeroconf:\n  default_interface: null\n",
            "zeroconf:\n  ipv6: maybe\n",
            "zeroconf:\n  ipv6: []\n",
            "zeroconf:\n  ipv6: {}\n",
            "zeroconf:\n  unsupported: true\n",
        ] {
            assert!(Config::parse(source).is_err(), "{source}");
        }
    }

    #[test]
    fn zeroconf_legacy_values_do_not_change_other_network_configuration() {
        let baseline = Config::parse("{}\n").unwrap();
        let configured =
            Config::parse("zeroconf:\n  default_interface: false\n  ipv6: false\n").unwrap();

        assert_eq!(baseline.http.server_host, configured.http.server_host);
        assert!(baseline.http.server_host.is_loopback());
        let baseline_components = baseline.api_config()["components"]
            .as_array()
            .unwrap()
            .clone();
        let mut configured_components = configured.api_config()["components"]
            .as_array()
            .unwrap()
            .clone();
        assert!(baseline_components.contains(&json!("network")));
        assert!(configured_components.contains(&json!("network")));
        configured_components.retain(|component| component != &json!("zeroconf"));
        assert_eq!(configured_components, baseline_components);
    }

    #[test]
    fn system_log_configuration_is_validated_before_runtime_setup() {
        let config = Config::parse("system_log:\n  max_entries: 7\n  fire_event: true\n").unwrap();
        let options = config.system_log.unwrap();
        assert_eq!(options.max_entries, 7);
        assert!(options.fire_event);
        let config =
            Config::parse("system_log:\n  max_entries: '0'\n  fire_event: ' yes '\n").unwrap();
        let options = config.system_log.unwrap();
        assert_eq!(options.max_entries, 0);
        assert!(options.fire_event);
        for source in [
            "system_log:\n  max_entries: -1\n",
            "system_log:\n  fire_event: invalid\n",
            "system_log: []\n",
            "system_log:\n  unsupported: true\n",
        ] {
            assert!(Config::parse(source).is_err(), "{source}");
        }
    }

    #[test]
    fn logger_configuration_is_validated_before_runtime_setup() {
        let config = Config::parse("logger:\n  default: info\n  logs:\n    homeassistant.components.mqtt: DeBuG\n  filters:\n    hass_mqtt: ['^skip']\n").unwrap();
        assert!(
            config.api_config()["components"]
                .as_array()
                .unwrap()
                .contains(&json!("logger"))
        );
        for source in [
            "logger:\n  default: null\n",
            "logger:\n  default: 10\n",
            "logger:\n  default: ' INFO '\n",
            "logger:\n  filters:\n    hass_mqtt: ['[broken']\n",
            "logger:\n  unsupported: true\n",
        ] {
            assert!(Config::parse(source).is_err(), "{source}");
        }
    }

    #[test]
    fn homeassistant_unit_system_accepts_source_keys_and_serializes_the_canonical_bundle() {
        for (setting, expected) in [
            ("metric", UnitSystemKind::Metric),
            ("us_customary", UnitSystemKind::UsCustomary),
            ("US_CUSTOMARY", UnitSystemKind::UsCustomary),
            ("imperial", UnitSystemKind::UsCustomary),
            ("IMPERIAL", UnitSystemKind::UsCustomary),
        ] {
            let config =
                Config::parse(&format!("homeassistant:\n  unit_system: {setting}\n")).unwrap();
            assert_eq!(
                config.homeassistant.unit_system,
                expected.as_str(),
                "{setting}"
            );

            let public_config = config.api_config();
            let units = &public_config["unit_system"];
            assert_eq!(*units, json!(expected.unit_system().as_dict()), "{setting}");
            let units = units.as_object().unwrap();
            assert_eq!(units.len(), 8, "{setting}");
            assert!(units.contains_key("area"), "{setting}");
            assert!(!units.contains_key("name"), "{setting}");
        }

        assert!(Config::parse("homeassistant:\n  unit_system: mars\n").is_err());
    }

    #[test]
    fn rejects_unsupported_configuration_instead_of_ignoring_it() {
        assert!(Config::parse("not_a_real_integration: {}\n").is_err());
        assert!(Config::parse("http:\n  unknown_proxy_setting: true\n").is_err());
        assert!(Config::parse("homeassistant:\n  time_zone: Mars/Olympus\n").is_err());
        assert!(Config::parse("homeassistant:\n  latitude: .nan\n").is_err());
        assert!(Config::parse("http: {}\nhttp: {}\n").is_err());
    }

    #[test]
    fn default_config_is_explicitly_local_and_persistent() {
        for source in ["{}\n", "\n", "null\n"] {
            let config = Config::parse(source).unwrap();
            assert!(config.http.server_host.is_loopback());
            assert_ne!(config.recorder.db_path, ":memory:");
            assert_eq!(config.api_config()["version"], hass_core::VERSION);
        }
    }

    #[test]
    fn config_check_does_not_create_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("configuration.yaml");
        std::fs::write(
            &path,
            "recorder:\n  db_path: missing/database.sqlite\nzeroconf:\n  ipv6: false\n",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert!(config.zeroconf_enabled);
        assert!(!config.zeroconf.ipv6);
        assert!(!dir.path().join("missing").exists());
    }

    #[test]
    fn included_packages_merge_before_validation_and_typed_errors_hide_secret_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("configuration.yaml");
        std::fs::write(
            directory.path().join("packages.yaml"),
            "helpers:\n  input_boolean:\n    packaged:\n      name: From package\n",
        )
        .unwrap();
        std::fs::write(
            &path,
            "homeassistant:\n  packages: !include packages.yaml\n",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert!(config.helpers.input_boolean.contains_key("packaged"));
        std::fs::write(
            directory.path().join("secrets.yaml"),
            "bad_port: credential-that-must-not-appear\n",
        )
        .unwrap();
        std::fs::write(&path, "http:\n  server_port: !secret bad_port\n").unwrap();
        let error = format!("{:#}", Config::load(&path).unwrap_err());
        assert!(error.contains("http.server_port"));
        assert!(!error.contains("credential-that-must-not-appear"));
    }

    #[test]
    fn group_display_order_follows_resolved_yaml_insertion_order() {
        let config =
            Config::parse("group:\n  zebra: [input_boolean.demo]\n  alpha: [input_boolean.demo]\n")
                .unwrap();
        assert_eq!(config.group_order, vec!["zebra", "alpha"]);
    }

    #[test]
    fn zone_yaml_accepts_empty_and_singleton_forms_and_validates_coordinates() {
        for source in ["zone:\n", "zone: {}\n", "zone: []\n"] {
            assert!(Config::parse(source).unwrap().zone.is_empty());
        }
        let zone = "name: Work\nlatitude: 42\nlongitude: -83\n";
        let config = Config::parse(&format!(
            "zone:\n{}",
            zone.lines()
                .map(|line| format!("  {line}\n"))
                .collect::<String>()
        ))
        .unwrap();
        assert_eq!(config.zone.len(), 1);
        assert_eq!(config.zone[0].name, "Work");
        assert_eq!(config.zone[0].radius, 100.0);
        assert!(
            Config::parse("zone:\n  - name: Invalid\n    latitude: 100\n    longitude: 0\n")
                .is_err()
        );
    }

    #[test]
    fn people_yaml_removes_falsy_entries_and_keeps_unique_ids_separate_from_names() {
        for source in [
            "person:\n",
            "person: {}\n",
            "person: [null, false, 0, '', {}, []]\n",
        ] {
            assert!(Config::parse(source).unwrap().person.is_empty());
        }
        let config = Config::parse(
            "person:\n  id: '1234'\n  name: Resident\n  device_trackers: device_tracker.phone\n",
        )
        .unwrap();
        assert_eq!(config.person[0].id, "1234");
        assert_eq!(
            config.person[0].device_trackers,
            vec!["device_tracker.phone"]
        );
        assert!(
            Config::parse(
                "person:\n  - {id: duplicate, name: First}\n  - {id: duplicate, name: Second}\n"
            )
            .is_err()
        );
    }

    #[test]
    fn known_device_import_resolves_secrets_without_rewriting_source_or_creating_database() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("configuration.yaml");
        let known_path = directory.path().join("known_devices.yaml");
        let source = "phone:\n  name: !secret device_name\n  mac: aa:bb:cc:dd:ee:ff\n  track: true\nrouter:\n  name: Router\n";
        std::fs::write(&config_path, "{}\n").unwrap();
        std::fs::write(&known_path, source).unwrap();
        std::fs::write(
            directory.path().join("secrets.yaml"),
            "device_name: Handset\n",
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();
        let phone = config
            .device_tracker
            .devices
            .iter()
            .find(|device| device.dev_id == "phone")
            .unwrap();
        assert_eq!(phone.name.as_deref(), Some("Handset"));
        assert_eq!(phone.mac.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert!(phone.track);
        assert!(
            !config
                .device_tracker
                .devices
                .iter()
                .find(|device| device.dev_id == "router")
                .unwrap()
                .track
        );
        assert_eq!(std::fs::read_to_string(&known_path).unwrap(), source);
        assert!(!directory.path().join("data").exists());
        std::fs::write(&known_path, "\n").unwrap();
        assert!(
            Config::load(&config_path)
                .unwrap()
                .device_tracker
                .devices
                .is_empty()
        );
    }
}
