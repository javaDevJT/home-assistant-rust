//! Explicitly supported configuration, validated before opening a database.

use crate::helpers::Helpers;
use anyhow::{Context, Result, bail};
use hass_automation::Automation;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{net::IpAddr, path::Path};

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
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            server_host: "127.0.0.1".parse().expect("literal IP"),
            server_port: 8123,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecorderConfig {
    pub db_path: String,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self {
            db_path: "data/hass.sqlite".into(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("cannot read configuration {}", path.display()))?;
        if metadata.len() > MAX_CONFIG_BYTES {
            bail!("configuration exceeds the 1 MiB limit");
        }
        Self::parse(&std::fs::read_to_string(path)?)
    }

    pub fn parse(source: &str) -> Result<Self> {
        if source.len() as u64 > MAX_CONFIG_BYTES {
            bail!("configuration exceeds the 1 MiB limit");
        }
        let value: serde_yaml::Value =
            serde_yaml::from_str(source).context("invalid configuration YAML")?;
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
                "counter",
            ]
            .contains(&key)
            {
                bail!("unsupported configuration key: {key}");
            }
        }
        let config: Self =
            serde_yaml::from_value(value).context("unsupported or invalid configuration option")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
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
        hass_automation::validate(&self.automation)?;
        Ok(())
    }

    pub fn api_config(&self) -> Value {
        let units = if self.homeassistant.unit_system == "metric" {
            json!({"length":"km", "mass":"g", "temperature":"°C", "volume":"L", "pressure":"Pa", "wind_speed":"m/s", "accumulated_precipitation":"mm"})
        } else {
            json!({"length":"mi", "mass":"lb", "temperature":"°F", "volume":"gal", "pressure":"psi", "wind_speed":"mph", "accumulated_precipitation":"in"})
        };
        let mut components = vec!["api", "websocket_api", "recorder"];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsupported_configuration_instead_of_ignoring_it() {
        assert!(Config::parse("mqtt: {}\n").is_err());
        assert!(Config::parse("http:\n  use_x_forwarded_for: true\n").is_err());
        assert!(Config::parse("homeassistant:\n  time_zone: Mars/Olympus\n").is_err());
        assert!(Config::parse("homeassistant:\n  latitude: .nan\n").is_err());
        assert!(Config::parse("http: {}\nhttp: {}\n").is_err());
    }

    #[test]
    fn default_config_is_explicitly_local_and_persistent() {
        let config = Config::parse("{}\n").unwrap();
        assert!(config.http.server_host.is_loopback());
        assert_ne!(config.recorder.db_path, ":memory:");
        assert_eq!(config.api_config()["version"], hass_core::VERSION);
    }

    #[test]
    fn config_check_does_not_create_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("configuration.yaml");
        std::fs::write(&path, "recorder:\n  db_path: missing/database.sqlite\n").unwrap();
        Config::load(&path).unwrap();
        assert!(!dir.path().join("missing").exists());
    }
}
