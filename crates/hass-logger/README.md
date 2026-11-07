# Native logger integration

`LoggerIntegration::start` validates logger YAML settings and every message
filter, loads durable `logger` settings, then registers the owned
`logger.set_level` and `logger.set_default_level` services. Provide the
application's admin authorizer and an explicit catalog of native integration
domains to tracing target roots.

Call `validate_config(&config)` during config-only validation before opening
runtime storage. It checks the strict field types and compiles all message
patterns without requiring a `Runtime`. Level names in YAML and logger services
are case-insensitive and do not accept surrounding whitespace; the WebSocket
level parser remains source-exact and accepts only named string values. An
empty module name denotes the root logger.

Attach the dynamic filter to the actual output layer so levels and message
filters change what is captured:

```rust,ignore
let logger = LoggerIntegration::start(runtime, config, catalog, authorize_admin)?;
let manager = logger.manager();
let output = tracing_subscriber::fmt::layer().with_filter(manager.filter());
```

Runtime service registrations retain weak manager handles. Dropping the
integration unregisters its services and makes retained manager/filter handles
inactive. Logger settings use the core runtime metadata key `core.logger`;
`once` settings apply on the next start and are removed durably before that
start returns, while `permanent` settings remain until replaced.
If startup notification fails after cleanup, the previous settings are
restored before returning the error so a failed start cannot consume `once`.
Successful default, module, and integration level changes publish an empty
`logging_changed` event with a fresh context after applying the change.

The catalog explicitly maps Python-style
`homeassistant.components.<domain>` names onto Rust tracing targets such as
`hass_<domain>`; `::` target hierarchy maps to dotted logger hierarchy. Names
without a catalog mapping remain ordinary Rust logger names and follow their
dotted hierarchy. `logger/log_info` reports the supplied loaded integrations
plus integration domains with current logger settings; it does not synthesize
arbitrary Python logger names. Filters are applied only to events from the
exact configured logger name, matching Python logging's record-filter
propagation. A matching message pattern suppresses the event.
The app assigns distinct native targets to each mapped integration so timer,
schedule, helper, scene and clock settings do not overwrite one another.

`event_record` resolves dynamic `log.target`/source fields from the installed
LogTracer bridge before per-event filtering. Native `hass.target` and
`hass.level` fields carry requested dynamic logger names and distinguish an
explicit `CRITICAL`/`FATAL` record (`50`) from an ordinary Rust error (`40`).
`NormalizedEventFormat` retains that target and severity, the original message
and structured fields in console output. `EventRecord::format_line` shares the
same representation with capture layers. Metadata admission defers dynamic
records until their actual fields can be checked; a logger named `log` cannot
override the original bridged target's level or message filters.

Message patterns use Rust's `regex` syntax. Python-only constructs such as
lookarounds and backreferences are rejected during startup validation.
