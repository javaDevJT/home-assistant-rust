//! Shared native instance identity for discovery and confirmed repair actions.

use anyhow::{Context, Result, ensure};
use hass_core::Runtime;
use serde_json::{Value, json};
use std::sync::Mutex;

pub const STORAGE_KEY: &str = "core.uuid";
const LEGACY_NATIVE_KEY: &str = "ssdp.instance_id";
// Only initialization/recreation is serialized here; no Runtime is retained.
static IDENTITY_WRITER: Mutex<()> = Mutex::new(());

pub fn get(runtime: &Runtime) -> Result<String> {
    let _writer = IDENTITY_WRITER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(value) = runtime.get_data(STORAGE_KEY)? {
        return decode(&value);
    }
    let identity = match runtime.get_data(LEGACY_NATIVE_KEY)? {
        Some(value) => {
            let identity = value
                .as_str()
                .context("invalid legacy native instance identity")?;
            validate(identity)?;
            identity.to_owned()
        }
        None => uuid::Uuid::new_v4().simple().to_string(),
    };
    save(runtime, &identity)?;
    Ok(identity)
}

/// Recreate only after an explicit administrator confirmation, then restart the
/// app so every protocol advertises the same new identity.
pub fn recreate(runtime: &Runtime) -> Result<String> {
    let _writer = IDENTITY_WRITER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let identity = uuid::Uuid::new_v4().simple().to_string();
    save(runtime, &identity)?;
    Ok(identity)
}

fn save(runtime: &Runtime, identity: &str) -> Result<()> {
    runtime.set_data(
        STORAGE_KEY,
        &json!({
            "version": 1, "minor_version": 1, "key": STORAGE_KEY,
            "data": { "uuid": identity },
        }),
    )?;
    Ok(())
}

fn decode(value: &Value) -> Result<String> {
    ensure!(
        value.get("version").and_then(Value::as_u64) == Some(1)
            && value.get("key").and_then(Value::as_str) == Some(STORAGE_KEY),
        "unsupported stored instance identity envelope"
    );
    let identity = value
        .get("data")
        .and_then(|data| data.get("uuid"))
        .and_then(Value::as_str)
        .context("invalid stored instance identity")?;
    validate(identity)?;
    Ok(identity.to_owned())
}

fn validate(identity: &str) -> Result<()> {
    ensure!(
        identity.len() == 32 && identity.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid stored instance identity"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_core::MemoryStore;
    use std::sync::{Arc, Barrier};

    #[test]
    fn legacy_native_identity_is_preserved_and_recreation_updates_the_shared_envelope() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("identity.sqlite");
        let reopen = || {
            Runtime::new(Arc::new(
                hass_storage::SqliteStore::open(&database).unwrap(),
            ))
            .unwrap()
        };
        let runtime = reopen();
        let legacy = "0123456789abcdef0123456789abcdef";
        runtime.set_data(LEGACY_NATIVE_KEY, &json!(legacy)).unwrap();
        assert_eq!(get(&runtime).unwrap(), legacy);
        assert_eq!(
            runtime.get_data(LEGACY_NATIVE_KEY).unwrap(),
            Some(json!(legacy))
        );
        assert_eq!(
            runtime.get_data(STORAGE_KEY).unwrap().unwrap()["data"]["uuid"],
            legacy
        );
        let replacement = recreate(&runtime).unwrap();
        assert_ne!(replacement, legacy);
        assert_eq!(get(&runtime).unwrap(), replacement);
        assert_eq!(
            runtime.get_data(LEGACY_NATIVE_KEY).unwrap(),
            Some(json!(legacy))
        );
        drop(runtime);
        let reopened = reopen();
        assert_eq!(get(&reopened).unwrap(), replacement);
        assert_eq!(
            reopened.get_data(LEGACY_NATIVE_KEY).unwrap(),
            Some(json!(legacy))
        );
    }

    #[test]
    fn malformed_or_future_identity_is_reported_without_replacing_existing_data() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        for stored in [
            json!("broken"),
            json!({"version":2,"key":STORAGE_KEY,"data":{"uuid":"0123456789abcdef0123456789abcdef"}}),
            json!({"version":1,"key":STORAGE_KEY,"data":{"uuid":"broken"}}),
        ] {
            runtime.set_data(STORAGE_KEY, &stored).unwrap();
            assert!(get(&runtime).is_err());
            assert_eq!(runtime.get_data(STORAGE_KEY).unwrap(), Some(stored));
        }
    }

    #[test]
    fn concurrent_protocol_initialization_observes_one_identity() {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let jobs: Vec<_> = (0..8)
            .map(|_| {
                let runtime = runtime.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    get(&runtime).unwrap()
                })
            })
            .collect();
        let identities: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
        assert!(identities.iter().all(|identity| identity == &identities[0]));
    }
}
