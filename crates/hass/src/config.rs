//! Explicitly supported configuration, validated before opening a database.

use crate::helpers::Helpers;
use anyhow::{Context, Result, bail};
use hass_automation::Automation;
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
        let mut config: Self = serde_path_to_error::deserialize(value).map_err(|error| {
            // A typed decoding error can quote an invalid scalar. Preserve
            // its configuration path without printing resolved secret values.
            anyhow::anyhow!(
                "unsupported or invalid configuration option at {}",
                error.path()
            )
        })?;
        config.group_order = group_order;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
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
        if !matches!(home.unit_system.as_str(), "metric" | "imperial") {
            bail!("unit_system must be metric or imperial");
        }
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
        let units = if self.homeassistant.unit_system == "metric" {
            json!({"length":"km", "mass":"g", "temperature":"°C", "volume":"L", "pressure":"Pa", "wind_speed":"m/s", "accumulated_precipitation":"mm"})
        } else {
            json!({"length":"mi", "mass":"lb", "temperature":"°F", "volume":"gal", "pressure":"psi", "wind_speed":"mph", "accumulated_precipitation":"in"})
        };
        let mut components = vec![
            "api",
            "websocket_api",
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
            "webhook",
            "time_date",
            "uptime",
        ];
        if !self.script.is_empty() {
            components.push("script");
        }
        if !self.sensor.is_empty() {
            components.push("sensor");
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
        std::fs::write(&path, "recorder:\n  db_path: missing/database.sqlite\n").unwrap();
        Config::load(&path).unwrap();
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
