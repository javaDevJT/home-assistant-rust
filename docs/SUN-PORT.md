# Native Rust Sun port plan

## Source pin and implementation boundary

This contract is traced against Home Assistant Core commit
[`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf).
That revision pins `astral==2.2` in [its `pyproject.toml`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/pyproject.toml#L37).
Keep this revision as the behavior oracle for a compatibility port; newer
Astral releases and newer HA behavior are separate changes.

The current Rust workspace already has `chrono` and `chrono-tz`, but its
`Cargo.toml`/`Cargo.lock` contain no solar-position or astronomy crate. The
practical no-new-dependency route is a small pure-Rust `f64` port of the
solar-calculation formulas Astral 2.2 documents as adapted from NOAA's solar
calculator spreadsheets. Use `chrono` for UTC instants and calendar dates, and
`chrono-tz` to select the Home Assistant local calendar date. Do not replace the
algorithm with a different ephemeris and claim parity without fixture evidence.

Astral's 2.2 documentation says observer height changes sunrise/sunset timing
through the visible horizon, but does not change the computed solar elevation.
Sunrise/sunset use the combined `-0.833°` horizon threshold, including the
apparent solar radius and a documented 32-arcminute refraction correction;
pressure and temperature effects on refraction are not modeled. Home Assistant
labels its `elevation` setting as height above sea level, while its helper passes
that number directly as Astral's observer elevation (which Astral describes
relative to the location/obscuring feature). Preserve that direct pass-through
for compatibility; changing its interpretation would be a behavior decision,
not an incidental port detail.

## Setup, entities, and metadata

The integration manifest is a calculated `service`, `single_config_entry`
integration with a config flow and `quality_scale: internal`
(`components/sun/manifest.json:1-12`). Config flow version is 1. The user flow
displays an empty form and creates a `Sun` entry with empty `data` and
`options`; YAML `sun: {}` creates that same import entry if none exists
(`config_flow.py:10-27`, `__init__.py:37-50`). Only one entry is allowed. The
entry owns one runtime `Sun` object, and unload must remove its time/config
listeners and unload both entity platforms (`__init__.py:53-73`). Core location
changes recalculate the observer and schedule (`entity.py:121-140`).

| Entity | Entity ID / unique ID | State and metadata |
| --- | --- | --- |
| Main Sun entity | `sun.sun` / fixed entity ID | `above_horizon` when rounded solar elevation is greater than `-0.833°`, otherwise `below_horizon`. It exposes `next_dawn`, `next_dusk`, `next_midnight`, `next_noon`, `next_rising`, `next_setting` as ISO timestamps; `elevation`, `azimuth` as degree values; and `rising` as a boolean. Those nine attributes are marked unrecorded. (`entity.py:42,81-99,154-175`; threshold in `const.py:12-14`.) |
| Six event sensors | `sensor.sun_next_{dawn,dusk,midnight,noon,rising,setting}` / `{entry_id}-next_*` | Timestamp device class; translated labels “Next …”; diagnostic; non-polling; update on the events signal. |
| Two position sensors | `sensor.sun_solar_{elevation,azimuth}` / `{entry_id}-solar_*` | Degree unit, measurement state class, diagnostic, non-polling, updated on the position signal; disabled by default in the entity registry. Values match the main attributes, rounded to two decimal places. |
| Rising binary sensor | `binary_sensor.sun_solar_rising` / `{entry_id}-solar_rising` | Diagnostic, non-polling, disabled by default, no device class; state label is “Rising”/“Setting”. Its boolean is `next_noon < next_midnight`. |

All sensor and binary sensor unique IDs use `{config_entry_id}-{key}` and share
the service device named `Sun`, identified by `("sun", config_entry_id)`;
the fixed `sun.sun` entity is created separately (`sensor.py:25-153`,
`binary_sensor.py:21-101`). Setup also removes the old `sensor.sun_solar_rising`
registry entry (`__init__.py:55-60`). Exact translated entity names are in
`components/sun/strings.json` (`Next dawn`, `Next dusk`, `Next midnight`,
`Next noon`, `Next rising`, `Next setting`, `Solar elevation`, `Solar azimuth`,
`Solar rising`).

## Solar calculation and scheduling contract

Implement event calculations as date-scoped functions returning an optional UTC
instant for an observer `(latitude, longitude, elevation_m)`: dawn/dusk at a
specified depression, sunrise/sunset, solar noon/midnight, and a rising/setting
crossing of a requested elevation. Astral's functions default to UTC results;
the HA helper chooses the date from the configured local timezone
(`helpers/sun.py:83-124,128-149`). The standard thresholds are horizon `-0.833°`,
civil `-6°`, nautical `-12°`, astronomical `-18°`; golden hour is `-4°…6°`,
blue hour is `-6°…-4°` (`const.py:12-26`). Astral event APIs take a positive
depression, so dawn/dusk callers negate the elevation boundary.

`get_observer_astral_event_next` starts from the current HA-local date minus one
day and scans through local date plus 366 days. For each date it calculates the
event in UTC, adds the requested offset, and returns the first instant strictly
later than `now`. It remembers the first per-date `ValueError`; if no event is
found it raises after the scan. Date-specific lookup uses the current local date
by default, converts datetime inputs to HA-local date, and returns `None` when
Astral reports no event that date. The `is_up` helper compares the next sunrise
and sunset (`helpers/sun.py:49-57,83-161`). Keep “missing on this date” distinct
from “no future event found” and from a valid event timestamp.

The `sun.sun` runtime recalculates at the next solar boundary. It checks, in
order, dawn at `-18/-12/-6°`, sunrise, dawn at `+10°`, solar noon, dusk at
`+10°`, sunset, dusk at `-6/-12/-18°`, then solar midnight. The earliest future
event determines phase; if noon/midnight is the next event, phase is recovered
from solar elevation at that instant. Position is recalculated at phase-specific
intervals: night 20 min, astronomical twilight 8 min, nautical twilight 4 min,
civil twilight 2 min, small-day 2 min, day 4 min. Near the next event, the
position timer is suppressed and the event callback takes over. At each position
update, azimuth/elevation are rounded to two decimals, state and position signals
are emitted, and the position timer is rearmed (`entity.py:44-78,177-305`).

All scheduled instants and timer callbacks are UTC. Use the configured timezone
only to pick the local date to scan, and use date arithmetic before converting
back to UTC; do not construct a possibly ambiguous/nonexistent local wall time
for the event. Trigger timers are recalculated after core configuration changes
(`trigger.py:223-265`). The pinned Sun tests cover timezone offsets and local-date
crossings, but no dedicated DST-transition test was found. Add explicit
`Europe/Oslo` spring-forward and fall-back fixtures before claiming DST parity;
assert each event is still one UTC instant, with no duplicate/skip when its local
calendar date changes.

## Automation conditions and triggers

Preserve both the legacy `sun` condition and the newer condition registry
(`condition.py:83-158,229-525`; `conditions.yaml:39-66`). The legacy condition
accepts `before`/`after` values `sunrise` or `sunset` and independent
`before_offset`/`after_offset` durations. Before-sunrise combined with
after-sunset is the overnight dark window and uses OR logic. If the required
event does not occur on the relevant local date (polar season), the condition is
false. Comparisons use event time plus offset.

| Condition | Contract |
| --- | --- |
| `is_up`, `is_set` | Compare the next sunrise and sunset; `is_set` is the inverse of `is_up`. |
| `is_ascending`, `is_descending` | Compare unrounded elevation now and one minute later; equal is descending. |
| `is_night` | Elevation `<= -18°`. |
| `is_morning_twilight`, `is_evening_twilight` | Rising/setting direction plus inclusive elevation band; `type` is `any` (−18…−0.833), `civil` (−6…−0.833), `nautical` (−12…−6), or `astronomical` (−18…−12). |
| `is_golden_hour`, `is_blue_hour` | Inclusive −4…6° or −6…−4° band; optional `period` is `any`, `morning`, or `evening`. |
| `is_midnight_sun`, `is_polar_night` | Evaluate the most recent solar midnight/noon, respectively: midnight sun if its low is above −0.833°; polar night if its high is below −0.833°. This keeps conditions aligned with the transition triggers. |
| `elevation` | Numeric threshold condition on elevation; selector accepts a degree-valued number/input_number/sensor and a value from −90° to 90°. |

The trigger registry/YAML includes `sunrise`, `sunset`, `solar_noon`,
`solar_midnight`, `dawn`, `dusk`, golden/blue-hour `started` and `ended`,
midnight-sun/polar-night `started` and `ended`, `elevation_changed`, and
`elevation_crossed_threshold` (`triggers.yaml:56-137`, `strings.json`). Event
triggers accept signed duration offsets; `dawn`/`dusk` default to civil and
accept `civil`/`nautical`/`astronomical`; golden/blue-hour triggers default to
period `any` and accept `morning`/`evening`. The elevation crossing trigger also
accepts `for`; the elevation-changed trigger accepts a `changed` threshold.
Legacy `sun` trigger configuration remains available with `event` (sunrise or
sunset), `offset`, and `offset_type` (`before` negates the offset). On firing,
the generic event has an empty additional payload, dawn/dusk includes `type`,
golden/blue hour includes `period`, and the legacy trigger includes `event` and
`offset` (`trigger.py:94-97,181-265,268-607`).

Polar event triggers must scan ahead for events that do not happen daily. The
midnight-sun and polar-night transitions occur when the elevation at solar
midnight/noon crosses `-0.833°`; skip the roughly annual crossing scan below
latitude 65°, stay armed but unscheduled where no crossing exists, and search
from `now - offset` so a pending multi-day “after” offset or an annual “before”
offset is not skipped (`trigger.py:459-509,512-541`). Ordinary sunrise/sunset and
dawn/dusk triggers likewise search ahead across dates with no event.

## Upstream acceptance fixtures

Use these pinned tests as the first compatibility gates, then carry their
semantic coverage into Rust tests. The timestamp fixtures are UTC.

1. `tests/components/sun/test_trigger.py:36-52`: at the default San Diego test
   location, frozen at 2015-09-15 01:00Z, exact next dawn/dusk values are civil
   dawn 13:07:26.084892Z, nautical dawn 12:38:34.055317Z, astronomical dawn
   12:09:09.742065Z; civil dusk 02:21:39.056429Z, nautical dusk
   02:50:29.596023Z, astronomical dusk 03:19:52.663966Z (dusk values occur
   earlier that same UTC date).
2. `tests/helpers/test_sun.py::test_norway_in_june` and
   `tests/components/sun/test_init.py::test_norway_in_june`: latitude 69.6,
   longitude 18.8, 2016-06-01Z has no same-date sunrise/sunset; next sunrise is
   2016-07-24 22:59:45.689645Z and next sunset is 2016-07-25 22:17:13.503932Z.
   `test_impossible_elevation` sets elevation to 10,000,000 m and expects a
   `ValueError` from next-sunrise lookup.
3. `tests/components/sun/test_trigger.py:431-438,511-578`: Kotzebue has two
   sunsets on local 2015-08-07 (2015-08-07 08:03:42Z and 2015-08-08 07:59:25Z).
   The synthetic `(66.5, -32.5, America/Anchorage)` case has two sunrises on
   local 2015-03-07 (2015-03-07 09:02:36Z and 2015-03-08 08:58:43Z). Both timers
   must fire twice rather than keying “one event per local day.”
4. `tests/components/sun/test_trigger.py:775-800,804-823`: Svalbard `(78.22,
   15.65, Europe/Oslo)` transition references: midnight-sun start 2015-04-18
   22:56:36Z; end 2015-08-25 22:59:19Z; polar-night start 2015-10-28
   10:41:13Z; end 2015-02-15 11:11:34Z. The firing test allows two seconds
   because its oracle is truncated to whole seconds.
5. `tests/components/sun/test_config_flow.py`: empty user/import entry is titled
   `Sun` with empty data/options; a second entry aborts with
   `single_instance_allowed`. `test_sensor.py` and `test_binary_sensor.py` assert
   entry-scoped unique IDs, diagnostic category, default-disabled position and
   rising entities, and signal-driven updates. `test_condition.py` and
   `test_trigger.py` cover no-event polar dates, thresholds, twilight defaults,
   offsets, and location-change rescheduling.

Preserve exact microsecond fixtures where the direct formula port reproduces
Astral 2.2's conversion. The upstream uses floating-point NOAA formulas, so there
is no documented global event-time error bound; do not claim ephemeris-grade
accuracy. Keep event-time fixture tolerances explicit, keep angle rounding at
two decimals on entity state, and test condition/trigger thresholds against the
unrounded calculation where the source does.

## Source map

- [helpers/sun.py](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/sun.py#L25-L161): observer construction, next-event/date scan, and `is_up` semantics.
- [sun integration setup and config flow](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sun): lifecycle, single-entry flow, metadata, and entity definitions.
- [entity.py](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sun/entity.py#L42-L305), [sensor.py](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sun/sensor.py#L25-L153), [binary_sensor.py](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sun/binary_sensor.py#L21-L101), and [const.py](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sun/const.py#L8-L47): thresholds, state/attributes, scheduling, and metadata.
- [condition.py](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sun/condition.py#L83-L525) with [`conditions.yaml`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sun/conditions.yaml), and [trigger.py](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sun/trigger.py#L94-L633) with [`triggers.yaml`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/sun/triggers.yaml): automation surface.
- [`tests/helpers/test_sun.py`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/helpers/test_sun.py) and [Sun component tests](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/sun): deterministic behavior and edge-case references.
- Astral 2.2 primary docs: [package API and UTC defaults](https://astral.readthedocs.io/en/stable/package.html),
  [elevation/refraction notes and NOAA attribution](https://astral.readthedocs.io/en/stable/index.html).
