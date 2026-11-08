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

## Explicit gaps

- Light profiles depend on Core's profile store and integration data. The
  normalizer reports a provider-owned parameter error for `profile`; profile
  application and default profile transitions are not emulated.
- `color_name` uses Core's full CSS color table and warning/fallback behavior.
  The parser validates its string shape, but normalization reports a
  provider-owned parameter error instead of guessing a mapping.
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

## Next color-name and profile continuation

This contract is not implemented in the 52-member candidate.

- `util/color.py:20-176` embeds 149 names: 148 CSS entries and the Home
  Assistant alias `(24,188,242)`. `color_name_to_rgb` at lines 198-206 removes
  literal spaces and lowercases, then raises `ValueError("Unknown color")` on
  a miss. Tabs and newlines are not general trimming characters.
- `light/helper.py:123-134` converts unknown names to white and emits one
  warning. `light/services.py:31-41` preprocesses once before target fan-out.
  The current MQTT adapter normalizes per target; diagnostics need a request
  boundary or deduplication rather than a log call in the pure SDK.
- `light/light_profiles.csv` supplies relax, concentrate, energize and reading.
  `Profile` in `light/__init__.py:230-280` accepts four or five fields: name,
  optional x/y, optional byte brightness and optional clamped transition.
  Both color coordinates are required for derived HS color.
- The loader at lines 283-329 merges built-in then config-directory CSV, with
  later names replacing earlier ones. It skips each header unconditionally.
  A malformed profile row aborts the remaining rows of that file; a following
  file is still processed. Unknown valid profile names apply as silent no-ops.
- Entity defaults precede `group.all_lights.default` and fill missing fields
  (`light/__init__.py:331-370`). Explicit color/brightness/transition win;
  brightness percentage overrides profile brightness. Per-entity brightness
  step handling precedes profile application.
- For turn-on, off/unknown state receives full defaults. An already-on light
  with no parameters also receives full defaults; with parameters it receives
  only a missing transition. Explicit transition skips defaults. Turn-off
  keeps only transition/flash after default lookup (`light/helper.py:137-167,
  218-250`). Toggle selects the corresponding on/off path, and zero brightness
  or white becomes off (`light/services.py:44-57`).

Implementation needs a pure parsed profile catalog and normalization context,
the registry-resolved entity ID, request-scoped diagnostics and a host-owned
config-directory loader. Existing constructors must retain their compatibility
path. New checks must cover all table values, literal-space behavior, warning
deduplication, CSV override/abort rules, default precedence, rename and real MQTT
payloads. Profile loading readiness must have an explicit host contract; source
startup currently initializes the catalog asynchronously.
