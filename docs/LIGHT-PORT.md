# Native light SDK port

`hass-light` ports the light entity contract from Home Assistant Core commit
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. It is a provider-facing crate:
`LightDescriptor` describes stable registry identity and capabilities,
`LightSnapshot` carries reported state, and `LightServiceRequest` parses and
normalizes the standard light service fields. The shared entity lifecycle
continues to own registration, user overrides, enable/disable behavior, and
state writes.

## Ported contract

- `LightColorMode` covers the pinned values `unknown`, `onoff`, `brightness`,
  `color_temp`, `hs`, `xy`, `rgb`, `rgbw`, `rgbww`, and `white`. Descriptor
  validation follows Core's mode invariants: the set is non-empty, `unknown`
  cannot be advertised, `onoff` and `brightness` are singleton modes, and
  `white` requires one of the five color modes.
- `LightFeature` uses the pinned bit values `effect = 4`, `flash = 8`, and
  `transition = 32`. The descriptor stores the feature mask, and capability
  projection publishes color modes, color-temperature bounds when applicable,
  and an effect list when the effect feature is enabled.
- Snapshot projection follows `LightEntity.state_attributes`: an on light
  must report a supported mode; off and unknown lights publish a null
  `color_mode`; effect, brightness, temperature, and color attributes are
  included only under the source's feature and mode conditions. HS, RGB, xy,
  RGBW, RGBWW, and color-temperature readings project the source's derived
  attributes where their reported channel value is available.
- Light capability attributes are also attached to `EntityPublication` as
  live attributes. They remain visible while the ordinary state is
  unavailable and are removed when a capability is no longer advertised.
- Service parsing accepts Core's target selectors and the pinned `turn_on`,
  `turn_off`, and `toggle` argument names. It validates numeric bounds and
  color tuple lengths, normalizes brightness percentages and steps, converts
  supported color representations using the source formulas, filters fields
  unsupported by the descriptor, and resolves zero brightness to an off
  action. Selector values remain unresolved for the shared registry resolver.
- The full pinned CSS color table and the `homeassistant` alias are available
  through pure lookup. Unknown names become white and attach a typed diagnostic
  with the original field and value; the SDK does not log.
- `LightProfileCatalog` parses built-in and host-supplied CSV strings in order.
  It retains CSV quoting and whitespace semantics, stops an individual source
  after its first blank or invalid row, reports that row to the host, and lets
  later sources replace earlier names. The host still owns file IO and catalog
  readiness.
- `normalize_with_context` accepts a host-supplied catalog and the
  registry-resolved entity ID. It applies explicit and default profiles,
  brightness and transition precedence, and emits diagnostics on the normalized
  result for request-level deduplication by the host.

Color conversions are ported from `homeassistant/util/color.py`: RGB/HS uses
the source HSV scaling and three-decimal rounding; RGB/xy uses the wide RGB D65
matrix and gamma correction; xy/RGB uses the inverse matrix and source
truncation; RGBW/RGBWW uses the source white extraction and max-channel
rescaling; Kelvin conversion uses the source approximation and mired formulas;
xy temperature uses McCamy's approximation. The formula-derived tests include
red/white RGB-to-xy goldens, primary HS-to-RGB values, white-channel endpoints,
and the xy-temperature singularity.

## Source map

All behavior claims above were checked against text from the pinned Home
Assistant tree; no Home Assistant or dependency Python was imported or run.

| Behavior | Pinned source |
| --- | --- |
| Color modes, capability/state attribute names, feature bits, defaults, service bounds and field names | `homeassistant/components/light/const.py:26-140`, `:150-231` |
| Supported-mode validation, capability predicates, preprocessing and supported-parameter filtering | `homeassistant/components/light/helper.py:30-201` |
| Brightness-step arithmetic, color-mode conversion order, white handling | `homeassistant/components/light/helper.py:218-435` |
| Light capability and state attributes, color-mode validation, toggle rules | `homeassistant/components/light/entity.py:161-408` |
| Service data split, zero-brightness off behavior and service schemas | `homeassistant/components/light/services.py:31-104` |
| RGB/xy matrix and gamma formulas | `homeassistant/util/color.py:209-310` |
| HS/RGB/xy formulas | `homeassistant/util/color.py:313-398` |
| RGBW/RGBWW channel formulas | `homeassistant/util/color.py:401-487` |
| Kelvin, RGBWW, xy temperature, blackbody coefficients and mired conversion | `homeassistant/util/color.py:503-640` |
| Full color-name table and name matching | `homeassistant/util/color.py:20-206` |
| Built-in light profile CSV | `homeassistant/components/light/light_profiles.csv` |
| Profile parsing, CSV merge and entity/group default precedence | `homeassistant/components/light/__init__.py:220-370` |
| Common alternative preprocessing and defaulted turn-on/off behavior | `homeassistant/components/light/helper.py:104-167`, `:218-250`; `homeassistant/components/light/services.py:31-57` |

## Explicit gaps

- Profile file discovery, UTF-8/IO errors, startup readiness and host logging
  stay outside the SDK. Hosts should use `from_csv_sources_with_report`, map
  each `source_index` to its source path, and publish the ready catalog to
  normalization callers.
- Profile numeric parsing follows the pinned shapes and clamps transition to
  `0..6553`; the service schema's broader coercion behavior is not reproduced
  for every Python-only input form.
- Core color utilities can clip xy values to a light-specific gamut. The
  descriptor has no gamut field yet, so conversions use the same no-gamut
  path Core uses when no gamut is passed.
- Home Assistant's service schema coerces some string and floating inputs
  through `probatio` and `config_validation`. This port supports JSON numbers
  and numeric strings for scalar values, but does not reproduce every Python
  coercion edge case.
- Temperature range ordering and finite xy checks are explicit SDK guards to
  prevent undefined conversion math; the pinned `LightEntity` property
  accessors themselves do not validate those provider values.

## Native adoption

The SDK is enrolled in the root workspace and consumes its shared dependencies.
MQTT discovery registers typed descriptors with the application's existing
entity lifecycle. JSON light state and normalized commands use the SDK, including
live capability publication while unavailable. Package wire fixtures cover
registry rename, disable, discovery removal and reload. The compiled-process
fixture also exercises real broker commands, config-entry disable/re-enable and
SQLite-backed registry identity. Expanded acceptance receipts are recorded in
VALIDATION.md after the final candidate gate.

## Named colors and profiles

The SDK includes the pinned 148 CSS names and the `homeassistant` alias.
Matching removes literal spaces and lowercases; tabs and newlines remain part
of a lookup key. Unknown names normalize to white and attach a
`light.unknown_color_name` diagnostic carrying `color_name` and the exact input
value. The host owns once-per-request logging across target groups.

The built-in CSV is embedded as data. `LightProfileCatalog::from_csv_sources`
merges host-supplied strings in order, with later names replacing earlier
entries. Each source skips exactly one record. The parser handles quoted and
multiline fields, preserves field whitespace, treats an empty physical record
as an invalid row, stops that source after the first invalid row, and continues
with later sources. `from_csv_sources_with_report` also returns the source
index, row index, parsed row or raw malformed record, and a stable reason so
the host can report the path without reparsing. The SDK performs no file IO.

`LightNormalizationContext` takes the registry-resolved entity ID explicitly
and a ready catalog. `normalize_with_context` applies explicit profiles, then
entity and `group.all_lights.default` defaults in source order. It retains the
source's brightness-step precedence and explicit caller fields. Off/unknown
states receive full defaults. An already-on request without effective
parameters receives defaults; with parameters it receives only a missing
transition, so an explicit transition prevents extra defaults on that path.
The legacy `normalize` method uses built-ins without inferring an entity ID
from the descriptor.

The zero-brightness reroute restores the original common-preprocessed request
before applying off defaults. Per-entity step processing and explicit profiles
can modify a copy, but their transition must not leak into that reroute. A
focused regression covers both step forms, absent and present defaults, and
an explicit caller transition. All 27 light package cases and strict lint pass
after this review correction.

The host still owns file discovery, UTF-8/IO errors, asynchronous catalog
readiness, diagnostic deduplication and provider execution. Light-specific
gamut clipping, localization, Python-only service coercions, and broader
provider/UI coverage remain outside this continuation.

Candidate 53 supplies the host boundary in
[`light_profiles.rs`](../crates/hass/src/light_profiles.rs): it reads the
configuration file's sibling `light_profiles.csv`, merges it after the embedded
built-ins, and completes the catalog before restoring config-entry providers.
A malformed row retains prior rows and reports its path and logical record;
an IO/UTF-8 failure leaves the whole catalog empty, matching the pinned
initializer's assignment-after-success behavior. Missing paths and non-files
retain built-ins. All three focused host fixtures pass. The
`--check` return precedes all profile IO. The ready catalog is shared by the
driver host; actual broker normalization and warning deduplication remain
subject to the pending MQTT and compiled-process checks.
