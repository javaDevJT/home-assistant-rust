# Home Assistant Core in Rust

An independent native Rust rewrite of [Home Assistant Core](https://github.com/home-assistant/core), forked to [javaDevJT/home-assistant-rust](https://github.com/javaDevJT/home-assistant-rust). The `rust-rewrite` branch contains the new implementation; `dev` preserves upstream.

**Full Home Assistant compatibility is not implemented.** This first milestone implements the runtime, persistence, helper services, automation subset, and selected REST/WebSocket protocols. Upstream device integrations, the official frontend, OAuth/user management, and existing Home Assistant databases are not supported. No Python bridge is used.

## Run

Use a dedicated configuration and database, separate from an existing Home Assistant installation. Rust 1.99 or newer is required by this workspace.

```sh
cargo build --release --locked
cargo run --locked -p hass -- --config examples/configuration.yaml --check
```

Generate a local token file, then start the server:

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

The event runs the example automation and toggles `input_boolean.demo`. A POST to `/api/states/...` only changes the recorded state representation. Helper services operate on the configured native helpers. Physical devices require an actual Rust integration port.

## Implemented surface

| Area | First milestone |
| --- | --- |
| Runtime | Typed states, attributes, contexts, ordered events, service registry |
| Storage | SQLite current states, history, event recording, restart restore |
| Native helpers | input_boolean, input_number, input_text, input_select, counter |
| Automation | State/event triggers, state/numeric conditions, services, bounded delays |
| REST | Authenticated states, config, services, event firing, history |
| WebSocket | Token handshake, states/config/services, event subscriptions, service calls, ping |
| Process | YAML validation, local listener, token file/env, Ctrl-C/SIGTERM shutdown |

This is protocol compatibility for the documented subset, not a drop-in replacement. Unsupported configuration keys fail validation. Unknown WebSocket commands and services return errors. `/api/events` listener inventory returns 501; history requires `filter_entity_id`. The token represents one administrator; upstream permission roles and refresh/login flows are unported. Jinja templates, external YAML includes/secrets, discovery/config entries, physical integrations, frontend assets, recorder statistics, and upstream database migrations remain unimplemented. This recorder currently grows without automatic purge. Synchronous service handlers must terminate; shutdown waits for in-flight handlers.

## Checks and project index

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

- [Rewrite status, baseline inventory, and task ledger](docs/REWRITE.md)
- [Module contracts and ownership](docs/CONTRACT.md)
- [Automation YAML and execution limits](crates/hass-automation/README.md)
- [API endpoints, protocol bounds, and shutdown](crates/hass-api/README.md)
- [Supported configuration example](examples/configuration.yaml)
- [Validation evidence](docs/VALIDATION.md)
- [Apache 2.0 license](LICENSE.md)

Baseline source: `home-assistant/core` at `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` (1,565 integration manifests; 20,410 Python files). Those counts describe upstream scope, not completed ports. Home Assistant names identify compatibility targets; this fork is not an official Home Assistant release.
