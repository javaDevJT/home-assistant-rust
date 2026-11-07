# Native `system_log` port contract and checkpoint

## Scope and provenance

This contract is based on Home Assistant Core commit [`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf), available in the local Git object database. The Rust implementation lives in `crates/hass-system-log`; API transport and app wiring are owned by their callers. The behavior notes below distinguish pinned source semantics from deliberate native bounds and remaining unported surfaces.

The port has two distinct log surfaces:

1. A small, retained list of warning-and-higher entries, exposed to administrators and optionally copied to the event bus.
2. A transient stream of formatted log records at all emitted levels, exposed to administrators with an optional integration filter.

The 2026 `system_log__get_logs` LLM tool is a third, file-backed query surface with its own paging and privacy rules. Do not merge these limits or response shapes.

## Pinned source behavior

### Configuration and retained entries

`homeassistant/components/system_log/const.py:1-22` sets `max_entries=50`, `fire_event=false`, event name `system_log_event`, raw backlog of 100 lines (5,000 for an integration), a 1,000-record live queue, and a 0.5-second stream cooldown. `__init__.py:36-50` validates `max_entries` with `cv.positive_int` (`int` coercion followed by a nonnegative check) and `fire_event` with `cv.boolean`; zero is valid. The outer global config schema allows other integration keys, while the inner `system_log` schema rejects unknown keys. The native config deserializer mirrors scalar coercion and inner-key rejection. Setup installs a root handler at `WARNING`, registers the raw stream, and removes its handler/state on Home Assistant close (`__init__.py:283-315`).

The admin `system_log/list` WebSocket command returns the records array directly as its result (`__init__.py:317-327`). Each row has these keys:

```json
{
  "name": "logger name",
  "message": ["recent distinct message", "..."],
  "level": "WARNING",
  "source": ["native source file path", 123],
  "timestamp": 0.0,
  "exception": "formatted exception text or empty string",
  "count": 1,
  "first_occurred": 0.0
}
```

`timestamp` and `first_occurred` are Unix seconds in the native list response. The message is an array capped at five distinct strings, not a scalar. A malformed `%`-style log message is guarded and rendered as a `Bad logger message: ...` string (`__init__.py:124-147`). For exceptions, the stored `exception` is `record.exc_text` or `""`; the handler only formats `exc_info` itself when it has a formatter (`__init__.py:165-213,250-280`). Do not assume every logged error has a populated traceback string. Native rows use the source path and line carried by `EventRecord`; Rust events do not provide a Python traceback root-cause frame, so native deduplication uses `(logger name, source path, source line)` and does not invent one.

The dedup key is `(logger name, source path/line, traceback root-cause path/line/function)`. Message text and severity are not in the key. Repeats increment `count`, update only the last `timestamp`, append the message only if it is not already in the five-item deque, and move the entry to the newest position. `first_occurred`, initial `level`, and initial `exception` stay as first captured. The store evicts the least-recently-updated unique row over `max_entries`; list order is newest first (`__init__.py:216-247`). For ordinary records, source falls back to the record pathname/line. Exception source detection walks the traceback and picks the last Home Assistant/config frame before falling back to the record site (`__init__.py:53-123`).

Only records at `WARNING` or above enter this list. With `fire_event=true`, each incoming warning/error emits a `system_log_event` carrying that occurrence's fresh `LogEntry.to_dict()`, not the merged dedup row; in particular its event count is one. The default is off. The event payload contains the same unredacted message/source/exception fields, and the system-log handler adds no event-specific permission check (`__init__.py:250-280`). Generic event-bus authorization still applies.

### Services and authority

`services.py:19-51` registers `system_log.clear` with an empty schema and `system_log.write` with required `message`, optional `level` in `debug|info|warning|error|critical` (default `error`), and optional `logger`. `message` and `logger` use `cv.string`, which converts JSON booleans and numbers to Python-style strings and rejects nulls and containers. Empty `logger` selects the root logger; the default is `homeassistant.components.system_log.external`. `level` is an exact lower-case choice. `clear` clears only retained dedup rows; it does not erase the log file, pending raw stream, or events already sent. `write` traverses the normal logging path. The native service emits a real tracing event through the installed subscriber, using shared callsite `hass_system_log::external`, dynamic `hass.target`/`hass.level`, and the common native service caller's source location. It does not insert a synthetic row directly.

`system_log/list` and `system_log/subscribe_raw` have explicit `require_admin` guards. The two service handlers do not add a system-log-specific role guard; native code should preserve the host service registry's normal authorization contract rather than infer a new role policy from this component. The LLM tool registration checks the Home Assistant LLM API ID but has no component-local admin decorator; its caller authority is supplied by the LLM API layer.

### Raw WebSocket stream

The command schema is `{"type":"system_log/subscribe_raw"}` with optional string `integration`, guarded by `require_admin` (`stream.py:255-283`). An unknown integration returns `ERR_NOT_FOUND` / `Integration not found`. A valid integration filters on its package path plus its declared logger names. Home Assistant matching is exact name or dotted child prefix, case-sensitive (`stream.py:81-108,255-283`); native targets also allow Rust `::` child namespaces.

The event body is an array of formatted record strings, one string per record; a traceback may make a string multiline. The event envelope is produced by `event_message(id, lines)`, so consumers receive `{"type":"event","id":id,"event":["...", "..."]}`. These are not structured entry dictionaries. The command sends the result acknowledgement first, then one event containing the backlog plus any live lines collected while reading it (`stream.py:284-311`).

On first subscriber, the handler adopts the active console formatter and attaches to the root logger. It keeps at most 1,000 formatted records, drains once every 0.5 seconds, and fans matching records to each subscriber. Debug messages from the WebSocket connection logger are dropped to prevent the stream from echoing its own sends. The raw stream sees all records that reach the logging handler, independently of the retained-list `WARNING` threshold (`stream.py:110-166`). The native layer mirrors this target-specific suppression and skips DEBUG/TRACE records produced inside an API `hass_raw_stream_delivery` span. The adapter must mark raw-send work with that span; unrelated WebSocket/provider logs remain eligible.

The backlog source is the configured log file when present; that takes precedence over a registered `LogReader`. For an unfiltered stream it returns the last 100 physical lines. For an integration stream it returns up to 5,000 matching lines; continuation lines inherit the preceding record's logger. Tail reads use 64 KiB blocks. Without a log file, the registered async reader is used with the same 100/5,000-line tails; without either source, backlog is empty (`stream.py:35-45,61-108,218-252`).

The live handler is attached only while at least one subscriber exists. Unsubscribe removes that callback; when the last one leaves, the root handler is detached and its transient queue cleared (`stream.py:173-215`). The subscription cleanup closure is stored in the WebSocket connection's ordinary subscription map; clients cancel it with `unsubscribe_events`. On Home Assistant close, setup also removes the handler and shuts down the debouncer (`__init__.py:299-306`).

The upstream live queue is capped by record count rather than bytes, and upstream `pending` during asynchronous backlog reads has no explicit cap (`stream.py:117,284-309`). The native port deliberately bounds both the shared raw queue and per-subscriber pending/live queues at 1,000 records. It flushes events captured while the backlog is read before switching to live batches, and removes only the number of duplicate first lines present in both sources. Large formatted records can still consume substantial memory within those record-count limits.

### LLM queries and privacy

`system_log__get_entries` reads the in-memory rows, defaults to 25 and caps at 50. It filters by exact lower-case level (`error|warning|critical`) and by a case-insensitive substring of logger name or source path. `include_traceback` defaults false; its output converts the two-element source tuple to an array and Unix timestamps to UTC ISO strings. The response is `{"result":[...]}` and only includes the `exception` key when explicitly requested (`llm.py:89-135,138-212`).

The newer `system_log__get_logs` is a separate, read-only file query. It is registered only for the Home Assistant LLM API and, outside Supervisor installs, when the log-file path exists; its current source list is `core` and default is `["core"]` (`llm.py:295-378,517-529`). Inputs are a nonempty registered `source` list, optional ISO-8601 `start_time`/`end_time`, optional minimum `level` (`debug|info|warning|error|critical`), case-insensitive `contains`, and `limit` (default 100, max 500). Records with an unparsed level are retained even when a minimum level is set. Default window is the last 15 minutes; offsetless times are local. Invalid times and reversed windows return explicit tool errors. It reads at most 2,000 physical lines per source from the first record at/after the start, merges by timestamp oldest first, and returns `{"records":[{"time":local ISO milliseconds,"source":name,"message":string}]}`. A source read failure is reported in an `errors` map while readable sources remain usable (`llm.py:37-64,214-270,295-514`).

`next_start_time` is the next unread timestamp (or the safe completion point when a source hits its line cap); pages do not split records with identical timestamps. Because log writes can complete out of timestamp order, a capped source backs its safe completion point off by 100 milliseconds. The page character target is 10,000 characters; the first timestamp group is always included, even if it exceeds that target. Long records retain their first 300 and last 700 characters with an omitted-character marker (`llm.py:37-64,270-293,396-514`). This is a page target, not a strict byte cap.

Privacy is surface-specific. The retained list, optional event, and admin raw stream are not secret-masked. `get_entries` omits exception text by default but does not redact messages. `get_logs` applies a best-effort linear-time mask for bearer/basic credentials and values whose keys resemble token/password/passwd/secret/API key, before record parsing; this is not comprehensive redaction (`llm.py:70-135,214-248,778-855`). Preserve the `fire_event=false` default, and do not advertise the query mask as a guarantee for other surfaces.

## Native Rust ownership and API

### Additive subscriber layer

The current app owns a single global subscriber in `crates/hass/src/main.rs:153-160`: `registry().with(fmt_layer.with_filter(logger.filter())).try_init()`. Keep that formatter/output layer and compose the system-log layer into the same registry; do not call a second global `try_init` or replace output with a capture writer. The existing `LoggerFilter` is a dynamic `tracing_subscriber::layer::Filter` backed by a weak manager reference; it checks the current target level and message patterns (`crates/hass-logger/src/lib.rs`, `LoggerFilter` / `ManagerInner::event_level_enabled` / `ManagerInner::event_enabled`).

The app attaches the logger policy to the system-log layer as its own per-layer filter, so capture observes the same dynamically permitted events as output. The layer retains `WARNING` and higher in the dedup list while allowing every admitted severity into the raw stream. It does not use `Layer::enabled` for a layer-local severity decision. The shared formatter and system-log layer use the same normalized record and span details.

The implemented public surface lives in the dedicated `hass-system-log` crate rather than the logger configuration store:

```rust
pub struct SystemLogConfig {
    pub max_entries: usize,        // default 50; zero disables retained rows
    pub fire_event: bool,          // default false
}

pub struct SystemLogEntry {
    pub name: String,
    pub message: Vec<String>, // at most five distinct recent messages
    pub level: String,
    pub source: (String, u32),
    pub timestamp: f64, // Unix seconds
    pub exception: String,
    pub count: u64,
    pub first_occurred: f64,
}

pub struct SystemLogManager { /* weak handle */ }
pub struct SystemLogLayer { /* Weak reference to manager state */ }
pub struct RawLogSubscription { /* id + receiver + RAII unsubscribe */ }
pub struct EventDeliveryStatus { /* bounded queue and persistence loss counters */ }

impl SystemLogManager {
    pub fn layer(&self) -> SystemLogLayer;
    pub fn list(&self) -> hass_core::Result<Vec<SystemLogEntry>>; // newest first
    pub fn clear(&self) -> hass_core::Result<()>;
    pub fn event_delivery_status(&self) -> hass_core::Result<EventDeliveryStatus>;
    pub fn subscribe_raw(
        &self,
        integration: Option<&str>,
    ) -> hass_core::Result<RawLogSubscription>;
    pub fn register_log_reader(&self, reader: LogReader) -> hass_core::Result<()>;
}

impl RawLogSubscription {
    pub async fn initial(&mut self) -> hass_core::Result<Vec<String>>; // single-use
    pub async fn next_batch(&mut self) -> hass_core::Result<Option<Vec<String>>>;
}

impl SystemLogIntegration {
    pub fn start(
        runtime: Runtime,
        config: SystemLogConfig,
        catalog: IntegrationCatalogProvider,
    ) -> hass_core::Result<Self>;
    pub fn manager(&self) -> SystemLogManager; // weak handle
    pub fn layer(&self) -> SystemLogLayer; // weak handle
    pub async fn shutdown(self) -> hass_core::Result<()>;
}
```

`SystemLogIntegration::start` registers both services and starts the optional event publisher; partial service registration is rolled back on failure. Each instance has a distinct service owner, so a duplicate start conflicts without replacing another instance's services. The returned manager and tracing layer hold only weak state references; the app must retain the integration handle. Manager methods return `hass_core::Result`, using `Error::NotFound("integration")` for an unknown integration and `Error::Service("system log integration is stopped")` after shutdown. A raw subscription attaches before backlog I/O and is an RAII guard. The API adapter keeps the guard intact, sends the command acknowledgement, awaits single-use `initial()` for the no-gap backlog/race batch, sends that array event, then reads `next_batch()` until connection close or the guard drops.

The synchronous `Layer::on_event` visits the real event once, copies root-to-leaf span names and `FormattedFields<DefaultFields>` from the subscriber context, and formats the record without calling services or awaiting. Its capture gate permits concurrent writers but lets shutdown wait for in-flight captures before closing queues; callbacks arriving after closure are ignored. A weak layer reference prevents a subscriber/runtime ownership cycle. The integration handle owns the runtime/services; shutdown unregisters owned services, closes subscriptions, drains the bounded optional-event queue, and joins both workers.

Use a single `SystemLogEntry` projection for the list and event payload, but keep raw stream strings and file-backed LLM pages separate. The shared event normalizer preserves the event's actual message, target, level, source fields, structured fields, and span context. Service writes are actual tracing events, not direct insertion into the store. `hass.target` and `hass.level` provide the requested dynamic logger and Home Assistant severity; `hass.file` and `hass.line` preserve the common native service callsite across severity branches. Ordinary bridged `log` records use their original `log.target`, `log.file`, and `log.line` metadata.

### Dynamic targets and the existing bridge

Tracing macro callsite targets are static, so `system_log.write` uses shared callsite `hass_system_log::external` and passes the runtime logger name in `hass.target`. It passes requested severity in `hass.level`; `CRITICAL` remains distinct while the tracing dispatch level is `ERROR`. The shared `hass-logger` normalizer applies dynamic target, severity, source, and span fields to the output formatter, dynamic filter, and system-log capture. The locked `log 0.4.35` `log!` macro also accepts a runtime `$target:expr` for ordinary facade logs. Workspace `Cargo.toml` enables `tracing-subscriber`'s `env-filter` without disabling defaults; locked `tracing-subscriber 0.3.23` defaults include `tracing-log`, and its `SubscriberInitExt::try_init` installs `LogTracer` after the global subscriber.

For ordinary facade logs, locked `tracing-log 0.2.0` first checks the original dynamic target, then dispatches from a static `log` callsite with original target/module/file/line in event fields. `LoggerFilter` defers its callsite-level decision for `log` and `hass.target` events, then applies dynamic target, severity, and message patterns to the normalized event. The system-log layer consumes the same normalized target and source. Regression coverage in the logger crate exercises dynamic targets, a logger literally named `log`, critical severity, and conflicting configured rules.

`tracing`/`log` levels also differ: Rust's `log::Level` has `Error` but no `Critical`, while `system_log.write` accepts `critical` and Python exposes `CRITICAL`. Preserve the requested severity in the native record and rendered line when writing at critical level; mapping it silently to `ERROR` loses source behavior. Rust tracing events do not carry a Python `exc_info`; a `SystemLogLayer` can retain explicit error fields and source metadata but must not invent a traceback when none was emitted.

### Service, transport, and privacy contract

The system log service handlers are registered independently from `LoggerIntegration`'s `logger.set_level` service:

| Native surface | Required behavior |
| --- | --- |
| `system_log.clear` service | Empty argument object; clear dedup rows only. Apply the native service registry's standard caller authorization. |
| `system_log.write` service | Required `message` and optional `logger` use Home Assistant scalar-to-string coercion; nulls and containers fail. Empty logger maps to `root`. `level` defaults to `error` and accepts only lower-case `debug/info/warning/error/critical`; all severity branches share one native caller source. |
| `system_log/list` WebSocket | Admin only; success result is a newest-first array of rows with the source keys above; `message` stays an array and `exception` stays present as string (empty if absent). |
| `system_log/subscribe_raw` WebSocket | Admin only; optional validated integration; result ack precedes initial event; event is an array of formatted record strings. Register the ordinary unsubscribe closure and release it on unsubscribe/connection close. |
| `system_log_event` bus event | Publish only when configured `fire_event=true` (default false); each queued occurrence uses its raw row before deduplication. A bounded 256-item nonblocking queue drops the newest item when full and increments `event_delivery_status.queue_dropped`; store failures increment `publish_failed`. Shutdown drains queued events and returns an error when either counter is nonzero. Generic event permissions apply. |

The crate exposes an explicit log-reader/backlog provider. With no provider, initial backlog is empty. It preserves native and pinned Home Assistant record headers, integration logger-prefix filtering, continuation-line inheritance, multi-subscriber fanout, 0.5-second coalescing, no-gap read/subscribe ordering, and bounded 1,000-record pending/live queues. A raw queue overflow fails the affected subscription instead of silently returning a partial sequence. When the host supplies a file-backed reader, it remains the backlog source.

The source exposes raw logs only to admins. The optional event and query paths have their separate source rules; do not accidentally turn raw stream or event payloads into an unredacted public feed. If the native app does not yet have the LLM API, record that as unported rather than inventing an endpoint. If it does, retain the distinction between `get_entries` (default 25/max 50, traceback off) and `get_logs` (default core, per-source 2,000 lines, default 100/max 500, 10,000-character target and best-effort masking).

## Focused acceptance falsifiers

The native acceptance checks should use actual transports and the installed tracing subscriber, not direct insertion into the retained list:

1. **One subscriber, two real log surfaces:** install the actual `fmt + LoggerFilter + SystemLogLayer` registry; emit a unique `tracing::warn!` and one `system_log.write` service call. Assert one unchanged console write, one `system_log/list` row, and one raw WebSocket event string for each record. Check target, severity, source, message array, timestamp shape, and event envelope. The root process fixture has passed the list/raw/write/level checks; the API unsubscribe correction still needs its transport rerun.
2. **Dynamic target through LogTracer:** call `system_log.write` with a runtime logger name; read that name back. Configure a target-specific level and a matching/nonmatching message regex, emit with the same target, update settings live, and repeat. Assert that both output and the system-log layer admit the same events. Include a configured logger literally named `log` to catch static bridge-callsite leakage. The logger normalization and filter regression checks cover this bridge boundary.
3. **All five service levels:** call `write` for debug/info/warning/error/critical. Confirm debug/info reach only the raw stream, warning/error/critical also reach the retained list, and `critical` remains distinguishable from `error` in list and formatted output. Missing message, null/container message or logger, unknown fields, and non-choice levels fail; `message`/`logger` boolean and numeric scalars coerce like `cv.string`, and an empty logger selects `root`.
4. **Dedup and eviction:** emit repeated warnings from the same actual callsite with repeated and distinct message strings; assert count increments for every occurrence, five distinct message slots roll, first time remains stable, last time updates, new rows appear newest first, and max-entry eviction removes the least recently updated unique row. Confirm different `system_log.write` levels to one logger share the service caller source and deduplicate into one row retaining the first severity, while event-bus payloads remain per-occurrence (`count=1`). Confirm `clear` leaves raw streaming intact.
5. **Authority:** verify an owner/admin can call list and raw subscribe; a non-admin is denied by the WebSocket commands; service-call behavior matches the native service registry's established policy. Unknown integration returns not-found, while valid integration includes its declared package/loggers and dotted children only.
6. **Backlog race and lifecycle:** use a bounded test log-reader or actual temporary log file with seeded test lines; emit an actual record while the asynchronous backlog read is blocked, then assert ack-first, backlog plus the live record exactly once. Test another subscriber concurrently, generic `unsubscribe_events`, connection close, last-subscriber handler removal, and manager shutdown. With DEBUG enabled, send one fixture burst and confirm the raw subscription receives no periodic records generated by sending its own batch (pending the API span instrumentation and root process rerun). Afterwards, emitted events still reach the existing formatter but do not mutate stopped system-log state or schedule runtime work.
7. **Bounds and privacy:** overfill unique warning rows, the per-subscriber raw stream queue, and optional event queue with real test events; assert configured entry cap and oldest-first drop policy, 1,000-record raw queue cap and fail-fast overflow, observable event `queue_dropped`/`publish_failed` status, and shutdown errors after event loss. With `fire_event=false`, assert no bus event; with it true, inspect a synthetic secret fixture to prove exactly which surfaces remain raw. If `get_logs` is ported, verify its best-effort masking and truncation separately from `get_entries`'s default omission of traceback.

Relevant pinned tests are `tests/components/system_log/test_init.py` (`test_normal_logs`, `test_exception`, `test_error_posted_as_event`, `test_remove_older_logs`, `test_dedupe_logs`, `test_clear_logs`, `test_write_log`, `test_write_choose_logger`, `test_write_choose_level`), `test_stream.py` (`test_subscribe_raw`, `test_unsubscribe_raw`, `test_subscribe_raw_integration`, `test_subscribe_raw_lines_logged_while_reading`, `test_subscribe_raw_unknown_integration`, `test_subscribe_raw_all_and_integration`), and `test_llm.py` (`test_get_entries_default`, `test_get_entries_with_traceback`, `test_get_logs_default`, `test_get_logs_merge_sources`, `test_get_logs_source_line_limit`, `test_get_logs_pages`, `test_get_logs_character_budget`, `test_get_logs_mask_secrets`, `test_get_logs_shorten_record`).

## Source and library anchors

- [Pinned `__init__.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/system_log/__init__.py): config/entry/list/event/setup.
- [Pinned `services.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/system_log/services.py), [stream.py](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/system_log/stream.py), [llm.py](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/system_log/llm.py), and [const.py](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/system_log/const.py).
- Current workspace subscriber ownership: `crates/hass/src/main.rs:153-160`; dynamic filter implementation: `crates/hass-logger/src/lib.rs` (`LoggerFilter`, `ManagerInner::event_level_enabled`, `ManagerInner::event_enabled`).
- Locked bridge implementation verified locally: `tracing-subscriber 0.3.23` `Cargo.toml` default features and `src/util.rs:61-77`; `tracing-log 0.2.0` `src/lib.rs:165-199,218-275,430-470`; `log 0.4.35` `src/macros.rs:75-117`.
- Primary API references: [`Layer`](https://docs.rs/tracing-subscriber/0.3.23/tracing_subscriber/layer/trait.Layer.html), [`Filter`](https://docs.rs/tracing-subscriber/0.3.23/tracing_subscriber/layer/trait.Filter.html), [`NormalizeEvent`](https://docs.rs/tracing-log/0.2.0/tracing_log/trait.NormalizeEvent.html), and [`log!`](https://docs.rs/log/0.4.35/log/macro.log.html).

## Implementation checkpoint — 2026-11-06

The native crate and root app wiring are implemented. `hass-system-log` owns retained rows, the additive tracing layer, `system_log.clear`/`system_log.write`, raw subscriptions, optional event publication, and joined shutdown. The app keeps the integration handle alive, attaches the layer beside the existing formatter with the logger filter, and supplies the full logging integration catalog to raw-stream lookup.

Crate checks cover configuration coercion and strict inner keys, zero retained rows, scalar service coercion, empty logger-to-root mapping, exact level choices, cross-level service deduplication, event queue overflow diagnostics, persistence failure at shutdown, per-occurrence event payloads, bounded raw backlog overlap, and native plus Home Assistant log-header filtering. Run `cargo test -p hass-system-log`, `cargo clippy -p hass-system-log --all-targets -- -D warnings`, and `cargo fmt -p hass-system-log -- --check` from the repository root.

The API transport is separate from this crate. The API owner has a focused loopback integration test; its unsubscribe correction needs a rerun, and the raw-send `hass_raw_stream_delivery` span plus root no-echo process rerun are still pending. The broader `system_log__get_entries` and `system_log__get_logs` LLM tools remain unported. Native rows do not reconstruct Python traceback roots, and retained rows, bus events, and raw stream strings are not secret-masked. The optional event queue intentionally drops its newest occurrence when full; query `event_delivery_status()` while active or observe the error returned from `shutdown()` to detect event loss or persistence failure.
