# Native zone integration

`hass-zone` implements Home Assistant zone behavior in Rust. It has no Python
bridge and performs no device, geolocation-provider, or other network traffic.

## Supported behavior

- Publishes the implicit `zone.home` entity with the configured home name and
  coordinates, a 100 m radius, `mdi:home`, and the upstream editable flag.
- Loads named YAML zones and storage-backed zones. Storage CRUD uses the
  runtime metadata key `hass-zone.collection.v1`; metadata is validated before
  startup mutates runtime states. Its private ownership receipt records managed
  zone entity IDs only, never YAML zone definitions, so restart cleanup removes
  stale owned states while preserving foreign `zone.*` states. Stored zone IDs
  are inferred for older metadata; older YAML states without an ownership
  receipt are preserved because they cannot be distinguished safely from
  foreign states.
- Publishes `latitude`, `longitude`, `radius`, `passive`, `persons`,
  `device_trackers`, `editable`, `friendly_name`, and a configured `icon`
  attribute. Zone state is the number of persons in the zone. Persons and
  device trackers are tracked as separate sets from their actual `in_zones`
  state attributes, including for `zone.home`.
- Implements containing, active, and enclosing-zone queries. Passive zones
  remain in containing/enclosing results but cannot be active. Unavailable
  zones are skipped; containment uses the same strict outer-boundary test as
  Home Assistant, while enclosing-zone edge contact is included.
- Uses the pinned WGS-84 Vincenty inverse calculation and its 200-iteration,
  `1e-12` convergence threshold, non-convergence behavior, and six-decimal
  kilometer rounding before conversion to meters.
- Provides storage collection change notifications through the private
  `zone_collection_updated` runtime signal. The manager snapshot and signal
  subscription are atomic with storage mutations.
- Provides `ZoneManager::reload` for a root-owned `zone.reload` callback that
  rereads the current YAML configuration, and `update_home` for accepted core
  location/name changes.

## Native rewrite gaps

- The crate does not register `zone.reload`; the application must register an
  admin-only callback that rereads its current config and invokes
  `ZoneManager::reload`.
- WebSocket command authorization, acknowledgements, event framing, and
  unsubscribe handling are owned by `hass-api`. The manager exposes the
  collection CRUD and atomic snapshot/signal primitives those handlers need.
- Home Assistant entity registry behavior, translations, UI flows, and all
  unrelated zone conditions/triggers are outside this crate.
- Configured icon attributes and simple ASCII entity ID generation cover the
  current native model; the full Home Assistant entity registry's localization
  and Unicode slug behavior are not implemented here.

## Source baseline

Behavior was compared against local Home Assistant commit
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`, especially
`homeassistant/components/zone/__init__.py`,
`homeassistant/components/zone/services.py`,
`homeassistant/helpers/collection.py`, `homeassistant/util/location.py`, and
`tests/components/zone/test_init.py`.
