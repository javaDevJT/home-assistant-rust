//! Configuration validation uses the compiled Rust executable without starting it.
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

const IGNORED_SECRET: &str = "synthetic-check-config-ignored-secret";

fn device_trigger(domain: &str) -> String {
    format!(
        "input_boolean:\n  result: {{}}\nautomation:\n  - id: device_fixture\n    triggers:\n      - platform: device\n        domain: {domain}\n        device_id: synthetic-metadata\n        entity_id: {domain}.fixture\n        type: turned_on\n        for:\n          seconds: 1\n    actions:\n      - action: input_boolean.turn_on\n        target:\n          entity_id: input_boolean.result\n"
    )
}

fn device_action(domain: &str, action: &str, entity_id: &str) -> String {
    format!(
        "script:\n  device_fixture:\n    sequence:\n      - device_id: synthetic-metadata\n        domain: {domain}\n        entity_id: {entity_id}\n        type: {action}\n"
    )
}

#[tokio::test]
async fn compiled_checker_validates_twenty_scenarios_without_creating_runtime_files() {
    let binary = std::env::var_os("HASS_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_hass")))
        .canonicalize()
        .expect("compiled hass executable");
    let directory = tempfile::tempdir().unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut cases: BTreeMap<String, (String, bool)> = [
        ("empty", "{}\n".to_owned(), true),
        (
            "example",
            std::fs::read_to_string(root.join("examples/configuration.yaml")).unwrap(),
            true,
        ),
        (
            "unknown_root",
            "not_a_supported_integration: {}\n".to_owned(),
            false,
        ),
        (
            "credentials_empty",
            "application_credentials: {}\n".to_owned(),
            true,
        ),
        (
            "credentials_false",
            "application_credentials: false\n".to_owned(),
            true,
        ),
        (
            "credentials_null",
            "application_credentials:\n".to_owned(),
            true,
        ),
        (
            "credentials_ignored",
            format!("application_credentials:\n  client_secret: {IGNORED_SECRET}\n"),
            true,
        ),
    ]
    .into_iter()
    .map(|(name, yaml, valid)| (name.to_owned(), (yaml, valid)))
    .collect();
    for domain in ["binary_sensor", "switch"] {
        let yaml = device_trigger(domain);
        cases.insert(format!("{domain}_device_mapping"), (yaml.clone(), true));
        cases.insert(
            format!("{domain}_device_bad_type"),
            (
                yaml.replace("type: turned_on", "type: nonexistent_type"),
                false,
            ),
        );
        cases.insert(
            format!("{domain}_device_string_duration"),
            (
                yaml.replace("for:\n          seconds: 1", "for: '00:00:01'"),
                false,
            ),
        );
    }
    let switch = device_action("switch", "turn_on", "switch.fixture");
    for (name, yaml, valid) in [
        ("script_switch_device", switch.clone(), true),
        (
            "script_button_device",
            device_action("button", "press", "button.fixture"),
            true,
        ),
        (
            "script_device_bad_domain",
            device_action("unsupported", "turn_on", "unsupported.fixture"),
            false,
        ),
        (
            "script_device_bad_type",
            device_action("button", "toggle", "button.fixture"),
            false,
        ),
        (
            "script_device_missing_id",
            switch.replace(
                "- device_id: synthetic-metadata\n        domain:",
                "- domain:",
            ),
            false,
        ),
        (
            "script_device_missing_entity",
            switch.replace("        entity_id: switch.fixture\n", ""),
            false,
        ),
        (
            "script_device_uuid_schema_only",
            device_action("switch", "turn_on", "0123456789abcdef0123456789abcdef"),
            true,
        ),
    ] {
        cases.insert(name.to_owned(), (yaml, valid));
    }
    assert_eq!(cases.len(), 20);
    for (name, (yaml, valid)) in cases {
        let case_directory = directory.path().join(&name);
        std::fs::create_dir(&case_directory).unwrap();
        let config = case_directory.join("configuration.yaml");
        std::fs::write(&config, yaml).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::process::Command::new(&binary)
                .args(["--config", config.to_str().unwrap(), "--check"])
                .current_dir(&case_directory)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("configuration checker must terminate without starting the server")
        .expect("execute configuration checker");
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            !output.contains(IGNORED_SECRET),
            "{name}: checker echoed ignored secret"
        );
        assert_eq!(result.status.success(), valid, "{name}: {output}");
        let files = std::fs::read_dir(&case_directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(files, vec![config], "{name}: checker created runtime files");
        println!("{name} PASS exit {}", result.status.code().unwrap());
    }
    println!("cases 20 failed 0");
}
