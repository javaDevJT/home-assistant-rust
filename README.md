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
| Storage | SQLite states/history/events/private metadata, migration, restart, single writer |
| Authentication | Password users, roles, JWT access, rotating refresh sessions, TOTP MFA, signed paths |
| Registries | Persistent entity/device/area/floor/label identity and CRUD |
| Configuration | Persistent entries, native driver flows/setup/unload/retry, core settings |
| Native helpers | input_boolean/number/text/select/button/datetime, counter, timer, schedule |
| Groups and controls | Legacy aggregate groups, native membership services, generic turn_on/turn_off/toggle delegation |
| Notifications | Internal feed, create/dismiss services, authenticated current/live WebSocket updates and automation triggers |
| Zones | Home, YAML and editable zones, WGS-84 distance, occupancy, live collection feeds and YAML reload |
| Automation | State/event/numeric/template/time/MQTT/notification/lifecycle triggers, conditions, execution modes |
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
- [Exhaustive upstream integration and subsystem parity](docs/PARITY.md)
- [Module contracts and ownership](docs/CONTRACT.md)
- [Automation YAML and execution limits](crates/hass-automation/README.md)
- [API endpoints, protocol bounds, and shutdown](crates/hass-api/README.md)
- [Native users, sessions, and authentication](crates/hass-auth/README.md)
- [MQTT broker, discovery, and device commands](crates/hass-mqtt/README.md)
- [RESTful sensors and commands](crates/hass-rest/README.md)
- [Scripts and scenes](crates/hass-script/README.md)
- [Reactive template sensors and action-backed controls](crates/hass-template-entities/README.md)
- [Timers, dates, buttons, and schedules](crates/hass-native/README.md)
- [Groups, membership, and aggregate states](crates/hass-group/README.md)
- [Internal notification services and feed](crates/hass-notifications/README.md)
- [Location zones, editable collection and geometry](crates/hass-zone/README.md)
- [Config entries, native drivers, and core settings](crates/hass-config/README.md)
- [Supported configuration example](examples/configuration.yaml)
- [YAML includes, secrets, packages, and loading limits](docs/CONFIGURATION.md)
- [Validation evidence](docs/VALIDATION.md)
- [Apache 2.0 license](LICENSE.md)

Baseline source: `home-assistant/core` at `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` (1,565 integration manifests; 20,410 Python files). Those counts describe upstream scope, not completed ports. Home Assistant names identify compatibility targets; this fork is not an official Home Assistant release.
