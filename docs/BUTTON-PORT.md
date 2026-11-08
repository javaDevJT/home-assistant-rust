# Native button SDK port

This crate ports the shared button-entity contract from Home Assistant Core
commit `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. It is an SDK layer for
providers; it does not claim that every Home Assistant button integration or
the button UI is implemented.

## Pinned behavior

- `homeassistant/components/button/__init__.py:26-32` defines `button` entity
  IDs, a 30-second component scan interval, a 10-second minimum scan interval,
  and `ButtonEntity` instances are separately marked not to poll at
  `homeassistant/components/button/entity.py:26-33`.
- `homeassistant/components/button/const.py:15-35` defines the domain, the
  `press` service, and exactly three button device classes: `identify`,
  `restart`, and `update`. The schema lowercases class input before coercion.
- `homeassistant/components/button/entity.py:35-64` selects device-class
  default naming when a class exists and exposes the last press as the entity
  state. `homeassistant/helpers/entity.py:1106-1119` maps a missing state to
  `unknown`; the button state is not a `last_pressed` attribute.
- `homeassistant/components/button/entity.py:76-82` restores the previous
  state string unless it is missing or `unavailable`; it does not parse the
  string as a timestamp.
- `homeassistant/components/button/services.py:8-15` registers `press` with
  no service data schema and directs it to `_async_press_action`.
- `homeassistant/components/button/entity.py:66-74` establishes the generic
  press ordering: generate a UTC ISO timestamp, set and write the new entity
  state, then await the provider's `async_press`. An action error propagates
  after the state write; Core does not roll the timestamp back. The MQTT
  implementation at `homeassistant/components/mqtt/button.py:95-101` only
  renders and publishes its configured command, so it relies on that generic
  wrapper for state and timestamp handling.
- `homeassistant/helpers/entity.py:1159-1182` adds ordinary extra state
  attributes only while available, but includes the device class independently
  of availability.

## Rust contract

`crates/hass-button` provides:

- `ButtonDescriptor`, which holds a shared `EntitySpec`, a typed device class,
  generic provider registry metadata, the source-derived default-name flag,
  and the non-polling property.
- `ButtonTimestamp`, which formats UTC state using Python `datetime.isoformat`
  precision: six fractional digits when microseconds are nonzero, no fraction
  when zero, and a `+00:00` offset. Sub-microsecond input is truncated.
- `ButtonSnapshot`, where an unpressed button is `unknown`, a press timestamp
  is the state string, and a restored state is retained verbatim. It projects
  to the shared `EntitySnapshot` without inventing a `last_pressed` attribute.
- `ButtonPressRequest::try_from(ServiceCall)`, which validates the `button`
  domain and `press` service, preserves schema-free data, and leaves selectors
  to the shared service-target resolver.
- `ButtonPressHandler`, a required provider action hook with no default or
  no-op implementation, and `execute_button_press`, the generic state/action
  wrapper.

`execute_button_press` stamps with the current UTC time and publishes through
`EntityHandle` before invoking the handler. It uses `EntityHandle::status()` to
distinguish an inactive registry lease from an unchanged publication: if the
lease is no longer active, the provider action is skipped; a suppressed
same-timestamp/context write on an active lease still invokes the provider. If
publication itself fails, the error returns before the action. If the provider
action fails, its error is returned and the timestamp already published to
current state and recorder history remains; the wrapper does not invent
rollback. The explicit-time `execute_button_press_at` variant makes this
ordering deterministic in tests. Service-target selection and any
provider-availability policy remain the calling host's responsibility; an
active registry lease does not imply that the provider reports the button as
available.

Before SDK adoption, native MQTT rendered and queued `button.press` without a
press timestamp. The current 48-crate candidate routes that action through
the wrapper and retains the timestamp across availability changes. The SDK's
nine cases, the MQTT package's 16 cases and the complete local 1,250-test
workspace gate pass, including concrete button timestamps, overlapping presses
and actual changed-state reporting. Exact-source Linux acceptance is pending
publication.

## Verification and open integration work

Standalone tests cover class parsing, timestamp and restore projection,
schema-free service validation, repeat calls with identical timestamp/context,
inactive-lease admission, and a synthetic SQLite-backed lifecycle press whose
provider fails after observing the timestamp publication. The failure test
asserts that the error propagates and the timestamp remains in current state
and recorded history. Another lifecycle check verifies that removing a
previously supplied device class clears its stale provider fallback.

The SDK crate delegates platform/service installation and concrete provider
effects to its host. MQTT adoption is in the current candidate; Home
Assistant's remaining integration setup/config-entry variants, translations
and localized default names, UI/service selectors, and per-integration press
effects still need native integration work. The upstream class set here is the
set at the pinned commit, not a claim about later Core revisions.
