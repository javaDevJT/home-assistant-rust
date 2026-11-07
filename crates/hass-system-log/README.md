# `hass-system-log`

Native Home Assistant system-log capture for the Rust app. The pinned behavior contract and source mapping are in [`docs/SYSTEM-LOG-PORT.md`](../../docs/SYSTEM-LOG-PORT.md).

## Supported behavior

- `SystemLogIntegration` registers `system_log.clear` and `system_log.write`, owns the publisher workers, and must be retained for as long as the app uses its weak `SystemLogManager` and `SystemLogLayer` handles.
- `SystemLogLayer` captures the normalized event once, including dynamic target, severity, source, structured fields, and root-to-leaf span names and fields. Compose it in the app's existing subscriber and attach the same `LoggerFilter` per-layer filter used by the output formatter.
- `SystemLogManager::list` returns newest-first warning, error, and critical rows. Rows deduplicate by logger name and source file/line, keep at most five distinct messages, preserve the first severity and exception, and count repeated occurrences. `max_entries: 0` disables retained rows.
- `system_log.write` emits a real tracing event. The service accepts lower-case `debug`, `info`, `warning`, `error`, and `critical` levels; warning and higher enter the retained list, while all admitted levels enter the raw stream. Calls to different levels for one logger share their native source so they deduplicate together. An empty logger name maps to `root`.
- `message` and `logger` service values use Home Assistant `cv.string` behavior: strings pass through, booleans become `True`/`False`, numbers become Python-style strings, and nulls, arrays, and objects fail. `level` remains an exact lower-case choice. Config `max_entries` applies integer coercion and a nonnegative check; `fire_event` accepts the pinned boolean spellings and numeric truth values. Unknown keys inside `system_log` are rejected; zero is valid.
- `clear` empties retained rows only. It does not clear a raw stream or events already sent.
- `subscribe_raw` returns an RAII subscription. `initial()` reads a registered `LogReader`, or returns an empty backlog when none is registered, while merging live rows without dropping repeated identical records. `next_batch()` returns coalesced batches; integration filtering accepts both native and pinned Home Assistant log headers and carries logger matches across continuation lines. Debug output from the pinned WebSocket connection logger is omitted from the raw stream. When the API adapter marks raw-send work with the `hass_raw_stream_delivery` span, DEBUG/TRACE records from that delivery are suppressed; unrelated provider and client logs remain eligible.
- Optional `system_log_event` delivery is disabled by default. Its logging callback uses a bounded, nonblocking 256-item queue. Full queues drop the newest event and increment `EventDeliveryStatus.queue_dropped`; storage failures increment `publish_failed`. Read `event_delivery_status()` while active. `shutdown().await` drains accepted queue entries, joins workers, and returns an error if it observed event loss, persistence failures, or a worker failure.

The integration uses weak manager/layer handles to avoid a subscriber/runtime ownership cycle. Shutdown closes capture under a gate, waits for in-flight writes, unregisters only its owned services, closes raw subscriptions, drains the optional event queue, and joins both workers. `Drop` initiates stop; call `shutdown().await` when joined cleanup and delivery errors matter. A raw stream queue overflow fails the affected subscription with a service error so consumers know its record sequence is incomplete.

## Limits

- Native Rust events do not include Python exception tracebacks or traceback root-cause frames. The crate preserves explicit exception fields but does not invent traceback data.
- Retained rows, `system_log_event`, and raw stream strings are not secret-masked. The separate LLM query tools `system_log__get_entries` and `system_log__get_logs` are not implemented here.
- Initial raw backlog is empty unless the host registers a `LogReader`; the crate does not fabricate historical log output.
- Raw and per-subscriber queues are bounded by record count, not bytes. A single large message can still use substantial memory within the limit.

## Verification

```sh
cargo test -p hass-system-log
cargo clippy -p hass-system-log --all-targets -- -D warnings
cargo fmt -p hass-system-log -- --check
```
