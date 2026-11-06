# Native person integration

`hass-person` manages YAML and editable people using real device-tracker
states already present in `Runtime`. A resolver supplies each person's current
entity ID; the person's `id` remains a separate stable unique ID. The crate
stores editable people in Runtime metadata and does not access device trackers,
Python, or external location providers.

## Supported behavior

- Filters YAML people whose linked `user_id` does not exist and validates
  linked-user existence and uniqueness when creating or editing stored people.
- Persists editable people across Runtime restarts, exposes synchronous typed
  collection CRUD, and emits atomic private `person_collection_updated`
  snapshots and change notifications.
- Selects the latest tracker within the pinned connected-with-zone, legacy
  `home`, GPS, and other-state priority groups; ignores `unknown` and
  `unavailable` tracker states and retains the configured-order winner on
  equal `last_updated` timestamps.
- Copies tracker coordinates, GPS accuracy, source, and `in_zones` attributes.
  Legacy `home` states without `in_zones` synthesize `zone.home` membership and
  borrow home-zone coordinates only when both coordinates are absent.
- Restores saved person location fields before startup, waits for the caller's
  `mark_started` signal before selecting current trackers, and follows tracker
  state changes through Runtime's public state events.
- Unlinks stored people on the `user_removed` Runtime event and removes stale
  managed entity states during YAML reload or restart.
- Provides the pinned person-to-tracker lookup helpers and the recorder
  significant-change rule (only a state-string change is significant).

## Native rewrite gaps

- The root application supplies `PersonUserValidator` from `AuthManager`, emits
  the `user_removed` event, registers the admin-only `person.reload` service,
  and implements the `person/update_own_profile` transport and AuthManager name
  mutation.
- Uploaded-picture authorization and image storage remain owned by the root
  application. The manager stores a picture string but does not validate or
  upload it.
- The caller must invoke `mark_started` after startup has finished; the crate
  does not infer Home Assistant's lifecycle from Runtime.
- IDs use a simple ASCII slug for newly created people. Home Assistant's
  Unicode slug transliteration is not fully reproduced.
- Entity registry mutation is owned by the root application, which supplies
  the resolver from its shared registry. Translations, UI flows, recorder
  exclusions, and other person-related services are outside this crate.

## Source baseline

Behavior was compared against local Home Assistant commit
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`, especially
`homeassistant/components/person/entity.py`,
`homeassistant/components/person/__init__.py`,
`homeassistant/components/person/helpers.py`,
`homeassistant/components/person/services.py`,
`homeassistant/helpers/collection.py`, and
`tests/components/person/test_init.py`.
