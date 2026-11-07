# `hass-system-health`

Native System Health provider registration and collection, ported against
Home Assistant Core source commit
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. The complete source contract is
[`docs/SYSTEM-HEALTH-PORT.md`](../../docs/SYSTEM-HEALTH-PORT.md).

## SDK

`SystemHealthIntegration` is the app-owned strong owner. Keep it alive for the
period in which the API uses its weak `SystemHealthManager` handle. Providers
register callbacks explicitly; there is no runtime event-bus subscription or
Python platform discovery. A later registration replaces the prior callback
and URL for that domain.

```rust,ignore
let integration = SystemHealthIntegration::new();
let manager = integration.manager(); // weak handle for API features

integration.register_info(
    "example",
    Arc::new(|| Box::pin(async {
        Ok(BTreeMap::from([(
            "status".to_owned(),
            SystemHealthValue::Immediate(SystemHealthData::Json(json!("ready"))),
        )]))
    })),
    Some("https://example.invalid/manage".to_owned()),
)?;

let mut session = manager.collect().await?;
// Serialize session.initial in the API's `initial` event.
while let Some(update) = session.next_update().await {
    // Send the generic success/failure result, then send `finish` at EOF.
}
```

The principal public signatures are:

```rust,ignore
pub type HealthFuture<T> =
    Pin<Box<dyn Future<Output = hass_core::Result<T>> + Send + 'static>>;
pub type SystemHealthInfo = BTreeMap<String, SystemHealthValue>;
pub type SystemHealthCallback =
    Arc<dyn Fn() -> HealthFuture<SystemHealthInfo> + Send + Sync + 'static>;

pub enum SystemHealthData {
    Json(serde_json::Value),
    DateTime(chrono::DateTime<chrono::Utc>),
}
pub enum SystemHealthValue {
    Immediate(SystemHealthData),
    Deferred(HealthFuture<SystemHealthData>),
}

impl SystemHealthIntegration {
    pub fn new() -> Self;
    pub fn manager(&self) -> SystemHealthManager;
    pub fn register_info(
        &self, domain: impl Into<String>, callback: SystemHealthCallback,
        manage_url: Option<String>,
    ) -> hass_core::Result<()>;
}
impl SystemHealthManager {
    pub fn register_info(/* same arguments */) -> hass_core::Result<()>;
    pub async fn collect(&self) -> hass_core::Result<SystemHealthSession>;
    pub async fn get_info(&self) -> hass_core::Result<SystemHealthInfoMap>;
}
impl SystemHealthSession {
    pub async fn next_update(&mut self) -> Option<SystemHealthUpdate>;
}
impl SystemHealthUpdate {
    pub fn to_transport_event(&self) -> serde_json::Value;
}
pub async fn check_can_reach_url(
    url: &str, more_info: Option<&str>,
) -> serde_json::Value;
```

`collect` runs registered callbacks concurrently with the source five-second
timeout. The returned `initial` map is sorted by domain and contains source
shaped `info` and optional `manage_url`; deferred fields serialize as pending
and dates as `{"value":"<ISO-8601>","type":"date"}`. Deferred updates are
yielded in completion order. Their public result is either
`Success(SystemHealthData)` or generic `Failed`; `to_transport_event()` emits
the full source update envelope while keeping provider error details private.
Dropping the session drops its outstanding deferred futures. Transfer a spawned
Tokio task with `SystemHealthValue::from_task(handle)` so session teardown also
aborts that task. Wrapping a raw `JoinHandle` in an ordinary future would detach
the child on drop; other provider futures must own and cancel any work they
start. The crate starts no task itself, and deferred values have no added timeout.
Callback and deferred panics become generic transport failures; actual details
remain in native logs and the separate internal helper.

`get_info` is an internal helper with source-style conversions: date values
become ISO-8601 strings, `{ "type": "failed" }` values become `Failed: ...`,
and deferred callback errors become `Exception: ...`. It is separate from the
generic WebSocket update contract. The URL helper sends a GET with a five-second
timeout, treats every HTTP status as reachable, ignores the response body, and
returns source-shaped generic failure objects with optional `more_info`.

The crate does not provide Python/frontend/platform discovery, an API route,
WebSocket dispatch, or an LLM tool. It does not report unknown runtime or host
versions and does not manufacture reachability results.

## Verification

```sh
cargo test --manifest-path crates/hass-system-health/Cargo.toml
cargo clippy --manifest-path crates/hass-system-health/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path crates/hass-system-health/Cargo.toml -- --check
```
