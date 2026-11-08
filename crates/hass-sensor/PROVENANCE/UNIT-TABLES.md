# Sensor native unit table provenance

## Source pin and derivation

This table is derived from Home Assistant Core commit
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. The source snapshot is licensed
under Apache License 2.0 (`LICENSE.md` at that commit).

The rule source is `homeassistant/components/sensor/const.py`:

- `SensorDeviceClass` declares 62 canonical classes.
- `DEVICE_CLASS_UNITS` defines 57 native-unit validation rules with 270 allowed
  set entries, including three explicit `None` entries.
- Five classes have no mapping entry: `date`, `enum`, `monetary`, `timestamp`,
  and `uptime`. They return no rule; this is not represented as an empty or
  null-only allowed set.

The sets were resolved statically from the pinned AST. `UnitOf*` members were
resolved to their exact string values in `homeassistant/const.py`, along with
the referenced constants `DEGREE`, `LIGHT_LUX`,
`SIGNAL_STRENGTH_DECIBELS`, and `SIGNAL_STRENGTH_DECIBELS_MILLIWATT`. No
Home Assistant code was imported or executed. Table rows follow the source
`SensorDeviceClass` declaration order; each unit slice is sorted with an
explicit allowed `None` first, then by the exact unit string. The strings keep
their source spelling and Unicode characters.

The outer API `None` means `DEVICE_CLASS_UNITS` has no key for the class. An
inner `None` means the source set explicitly allows a unitless value. The
source explicitly allows `None` for `aqi` and `ph`; `power_factor` allows both
`None` and `%`.

## Converter units are a separate domain

`homeassistant/util/unit_conversion.py` declares converter `VALID_UNITS`, and
`UNIT_CONVERTERS` in the sensor constants maps some device classes to those
converters. These sets describe conversion compatibility, not the native-unit
validation rules. Static comparison found 11 class/converter pairs with extra
converter units and no class whose native set exceeded its converter set:

| Device class | Converter-only `VALID_UNITS` |
| --- | --- |
| `absolute_humidity` | `μg/m³` |
| `duration` | `w` |
| `gas` | `fl. oz.`, `gal`, `mL` |
| `power` | `BTU/h` |
| `power_factor` | `ppb`, `ppm` |
| `precipitation` | `ft`, `km`, `m`, `mi`, `nmi`, `yd` |
| `precipitation_intensity` | `Beaufort`, `ft/s`, `in/s`, `km/h`, `kn`, `m/min`, `m/s`, `mm/s`, `mph` |
| `volatile_organic_compounds` | `g/m³` |
| `volatile_organic_compounds_parts` | `%`, `None` |
| `water` | `fl. oz.`, `mL` |
| `wind_speed` | `in/d`, `in/h`, `mm/d`, `mm/h` |

The native table follows `DEVICE_CLASS_UNITS` exactly, so those converter-only
units are not added as native units.

## Source file hashes

The projector also applies the 13 `AMBIGUOUS_UNITS` entries from the same sensor
constants file (lines 908-922), following `entity.py`'s private native-unit
compatibility property (lines 436-448). These normalize legacy micro-sign and
reactive-power spellings for publication while retaining the raw provider unit
in the descriptor. User-selected and suggested units are separate inputs and
are not passed through that native-unit compatibility map.

SHA-256 hashes of the source file bytes at the pinned commit:

| Source path | SHA-256 |
| --- | --- |
| `homeassistant/components/sensor/const.py` | `f32a0e608e404bf85c6de7795797260231ffe8254c24d3101e5e0a29c014a8f5` |
| `homeassistant/util/unit_conversion.py` | `cef97b3984e2182470074d3e0c01e8e7b8c35a4c9c29473dce52e96bec9baa62` |
| `homeassistant/const.py` | `db785a2a0014e43f6b07820bf36bb2f372636d69c7ef08b3a73a31179dc398c5` |

The Rust data is derived from Home Assistant Core, which is distributed under
Apache License 2.0. See the pinned source's `LICENSE.md` for the license text.
