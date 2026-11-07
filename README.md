# Home Assistant Core in Rust

An independent native Rust rewrite of [Home Assistant Core](https://github.com/home-assistant/core), forked to [javaDevJT/home-assistant-rust](https://github.com/javaDevJT/home-assistant-rust). The `rust-rewrite` branch contains the new implementation; `dev` preserves upstream.

**The entire rewrite is still in progress.** The native implementation includes the runtime, persistence, user authentication, registries, configuration entries, MQTT and REST integrations, templates, scripts, scenes, automations, and helpers described below. Full integration coverage, the official frontend, and existing Home Assistant database compatibility remain open. No Python bridge is used.

## Run

Use a dedicated configuration and database, separate from an existing Home Assistant installation. Rust 1.99 or newer is required by this workspace.

```sh
cargo build --release --locked
cargo run --locked -p hass -- --config examples/configuration.yaml --check
```

Start without a token to use native onboarding and password-provider login, or generate an administrator token file for local API work:

```sh
umask 077
openssl rand -hex 32 > /tmp/hass-rust.token
cargo run --locked -p hass -- --config examples/configuration.yaml --token-file /tmp/hass-rust.token
```

The example listens at `127.0.0.1:8123`. Its database is `examples/data/hass.sqlite`; relative database paths resolve against the configuration file's directory. `--database` overrides the path relative to the working directory. `HASS_TOKEN` or `HASS_TOKEN_FILE` can supply the token; there is no default token. `--host` and `--port` override the listener. Configuration checks do not require credentials and create no database.

```sh
curl http://127.0.0.1:8123/health
curl -H "Authorization: Bearer $(cat /tmp/hass-rust.token)" http://127.0.0.1:8123/api/states
curl -X POST -H "Authorization: Bearer $(cat /tmp/hass-rust.token)" \
  -H 'Content-Type: application/json' -d '{}' \
  http://127.0.0.1:8123/api/events/demo_toggle
```

The event runs the example automation and toggles `input_boolean.demo`. A POST to `/api/states/...` changes the recorded state representation. Helper services operate on configured native helpers; generic Home Assistant controls delegate to actual domain providers, including the supported MQTT device commands.

## Implemented surface

| Area | Current native implementation |
| --- | --- |
| Runtime | Typed states, attributes, contexts, ordered events, service registry |
| Entities | Shared provider leases, stable identity, registry overrides, rename/disable/remove, restore and owned cleanup |
| Storage | SQLite states/history/events/private metadata, migration, restart, single writer |
| Authentication | Password users, roles, JWT access, rotating refresh sessions, TOTP MFA, signed paths |
| Registries | Persistent entity/device/area/floor/label identity and CRUD |
| Configuration | Persistent entries and subentries, native create/options/reconfigure flows, setup/unload/retry, core settings |
| Native helpers | input_boolean/number/text/select/button/datetime, counter, timer, schedule |
| Groups and controls | Legacy aggregate groups, native membership services, generic turn_on/turn_off/toggle delegation |
| Notifications | Internal feed, create/dismiss services, authenticated current/live WebSocket updates and automation triggers |
| Zones | Home, YAML and editable zones, WGS-84 distance, occupancy, live collection feeds and YAML reload |
| Presence | Native tracker reports/expiry, registry-backed people, tracker priority, account linkage and caller-only name edits |
| Images | Authenticated JPEG/PNG/GIF uploads, public originals/thumbnails, private collection feeds and caller-bound profile pictures |
| Tags | Registry-backed collection, native reader callback, scan-triggered actions, private WebSocket feeds and history attribute projection |
| Automation | State/event/numeric/template/time/MQTT/notification/webhook/tag/lifecycle triggers, conditions, execution modes |
| Clock sensors | Time/date formats, timezone and DST scheduling, fixed setup-time uptime, YAML and config-entry flows |
| Sun | Native astronomical calculations, singleton/config-entry entities, location updates, solar conditions and scheduled actions |
| Logging | Actual dynamic tracing levels/message filters, admin controls, persistent and once-only settings, public change notifications |
| System Log | Deduplicated retained errors, actual write/clear services, admin raw streams and integration filters |
| System Health | Native provider SDK, cancellable one-shot WebSocket reports, current native version/timezone |
| Repairs | Persistent issue registry, refresh scopes, visibility/dismissal, native fix flows and independent child-flow continuation |
| SSDP | Native discovery cache/callbacks, UDP scanning, UPnP XML/advertisements, config-flow bridge and admin WebSocket feed |
| Actions | Shared script executor, cancellation/queues, scenes, bounded native templates |
| Integrations | MQTT discovery, state/availability and commands; REST polling and commands |
| REST API | Authenticated states, config, services, events, templates, history |
| WebSocket | Native auth, live entity/config/template/event subscriptions, services, registries |
| Process | YAML validation, trusted proxy origin checks, local-user policy, joined shutdown |

Every integration remains subject to the detailed [parity inventory](docs/PARITY.md); no complete upstream integration is claimed. Unsupported options and commands fail explicitly. The official frontend, additional auth providers and custom permissions, most device integrations, recorder statistics, and upstream database migrations remain open. Event inventory reports actual public wildcard receivers; history requires `filter_entity_id`. Synchronous service handlers must terminate; shutdown waits for in-flight handlers.

## Checks and project index

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

- [Rewrite status, baseline inventory, and task ledger](docs/REWRITE.md)
- [Active full-rewrite execution ledger](docs/PORTING.md)
- [Pinned Sun port contracts and astronomical reference cases](docs/SUN-PORT.md)
- [System Log source contracts and stream acceptance cases](docs/SYSTEM-LOG-PORT.md)
- [System Health provider and subscription port contracts](docs/SYSTEM-HEALTH-PORT.md)
- [SSDP discovery and pinned dependency port contracts](docs/SSDP-PORT.md)
- [Native SSDP SDK, scanner and UPnP server APIs](crates/hass-ssdp/README.md)
- [Repairs and issue registry port contracts](docs/REPAIRS-PORT.md)
- [Config subentries, native flow callbacks and lifecycle contracts](docs/CONFIG-SUBENTRIES-PORT.md)
- [Exhaustive upstream integration and subsystem parity](docs/PARITY.md)
- [Module contracts and ownership](docs/CONTRACT.md)
- [Shared entity identity, provider leases and lifecycle](crates/hass-entity/README.md)
- [Automation YAML and execution limits](crates/hass-automation/README.md)
- [API endpoints, protocol bounds, and shutdown](crates/hass-api/README.md)
- [Native users, sessions, and authentication](crates/hass-auth/README.md)
- [MQTT broker, discovery, and device commands](crates/hass-mqtt/README.md)
- [RESTful sensors and commands](crates/hass-rest/README.md)
- [Scripts and scenes](crates/hass-script/README.md)
- [Bounded native template rendering](crates/hass-template/README.md)
- [Reactive template sensors and action-backed controls](crates/hass-template-entities/README.md)
- [Timers, dates, buttons, and schedules](crates/hass-native/README.md)
- [Groups, membership, and aggregate states](crates/hass-group/README.md)
- [Internal notification services and feed](crates/hass-notifications/README.md)
- [Location zones, editable collection and geometry](crates/hass-zone/README.md)
- [Device tracker reports and known-device configuration](crates/hass-device-tracker/README.md)
- [People, tracker selection and account links](crates/hass-person/README.md)
- [Uploaded images, thumbnails and profile pictures](crates/hass-image-upload/README.md)
- [Webhooks, callback authority and private automation delivery](crates/hass-webhook/README.md)
- [Native time/date and uptime sensors](crates/hass-system-sensors/README.md)
- [Native tag collection, scan callbacks and automation triggers](crates/hass-tag/README.md)
- [Native logger policy, filters and persisted controls](crates/hass-logger/README.md)
- [Native System Log capture, raw streams and failure diagnostics](crates/hass-system-log/README.md)
- [Native System Health providers, deferred values and reachability](crates/hass-system-health/README.md)
- [Native issue registry, repair handlers and fix flows](crates/hass-repairs/README.md)
- [Native Sun calculations, entities and config-entry lifecycle](crates/hass-sun/README.md)
- [Config entries, native drivers, and core settings](crates/hass-config/README.md)
- [Supported configuration example](examples/configuration.yaml)
- [YAML includes, secrets, packages, and loading limits](docs/CONFIGURATION.md)
- [Validation evidence](docs/VALIDATION.md)
- [Apache 2.0 license](LICENSE.md)

Baseline source: `home-assistant/core` at `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` (1,565 integration manifests; 20,410 Python files). Those counts describe upstream scope, not completed ports. Home Assistant names identify compatibility targets; this fork is not an official Home Assistant release.

## Repairs registry implementation checkpoint

The pinned registry corrections are implemented in `crates/hass-repairs/src/registry.rs`, the built-in constructor/subentry resolver in `lib.rs`, and the nullable-fixability guard in `flows.rs`. Refresh matching and keep/defer/cancel behavior now follow the pinned source semantics; startup migrates documents with a missing storage minor version and validates linked issues against real config-subentry IDs; public create validates nonempty CALVER values using the pinned AwesomeVersion 25.8.0 strategy. Restored nonpersistent shells retain `is_fixable: null`, while active draft and list values remain booleans. Acceptance checks passed: `cargo test -p hass-repairs --locked` (23 tests), `cargo clippy -p hass-repairs --all-targets --locked -- -D warnings`, and `cargo fmt -p hass-repairs -- --check`. See the [pinned registry contract](docs/PORTING.md#active-repairs-registry-correction-contract).
