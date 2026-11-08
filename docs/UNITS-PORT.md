# Native unit conversion port

## Scope and pinned source

The standalone Rust crate at crates/hass-units implements the converter families
found in Home Assistant's unit_conversion module at commit
cbf2c02c9bff331537c19bff69da8dc11c3fbfbf. Unit identifiers come from
homeassistant/const.py at the same commit. The source was inspected as text and
Python AST data through git show; no Home Assistant or dependency Python was
imported or executed.

The public entry points are UnitFamily, supported_units, is_valid_unit,
convert, and convert_optional. Unit identifiers are Option<&str> so a real
missing identifier remains distinct from a conversion error. Conversion
returns Result and reports invalid source and target units separately.

## Source map

The converter row gives the Python class and inclusive source lines. The
constant row gives the unit enum and its inclusive lines in homeassistant/const.py.
Counts match each converter's VALID_UNITS set or, for base classes without an
explicit VALID_UNITS set, its conversion table.
The 31 family tables contain 190 supported entries in total, counting the
unitless None identifier.

| Rust family | Pinned converter | Constants and supported count |
| --- | --- | --- |
| ApparentPower | ApparentPowerConverter, 194-207 | UnitOfApparentPower, 496-501; 3 |
| Area | AreaConverter, 210-226 | UnitOfArea, 707-719; 10 |
| BloodGlucoseConcentration | BloodGlucoseConcentrationConverter, 229-239 | UnitOfBloodGlucoseConcentration, 843-847; 2 |
| CarbonMonoxideConcentration | CarbonMonoxideConcentrationConverter, 242-264 | UnitOfRatio, 810-815; UnitOfDensity, 798-807; 4 |
| Conductivity | ConductivityConverter, 267-276 | UnitOfConductivity, 735-740; 3 |
| DataRate | DataRateConverter, 279-297 | UnitOfDataRate, 905-918; 11 |
| Distance | DistanceConverter, 300-325 | UnitOfLength, 619-630; 9 |
| Duration | DurationConverter, 328-349 | UnitOfTime, 604-615; 7 |
| ElectricCurrent | ElectricCurrentConverter, 352-361 | UnitOfElectricCurrent, 566-571; 3 |
| Frequency | FrequencyConverter, 364-375 | UnitOfFrequency, 634-641; 5 |
| ElectricPotential | ElectricPotentialConverter, 378-395 | UnitOfElectricPotential, 575-582; 5 |
| Energy | EnergyConverter, 398-419 | UnitOfEnergy, 527-544; 15 |
| EnergyDistance | EnergyDistanceConverter, 422-436 | UnitOfEnergyDistance, 556-562; 4 |
| Information | InformationConverter, 439-467 | UnitOfInformation, 878-901; 21 |
| Mass | MassConverter, 470-491 | UnitOfMass, 723-732; 7 |
| MassVolumeConcentration | MassVolumeConcentrationConverter, 494-507 | UnitOfDensity, 798-807; 3 |
| NitrogenDioxideConcentration | NitrogenDioxideConcentrationConverter, 510-525 | UnitOfRatio, 810-815; UnitOfDensity, 798-807; 3 |
| NitrogenMonoxideConcentration | NitrogenMonoxideConcentrationConverter, 528-541 | UnitOfRatio, 810-815; UnitOfDensity, 798-807; 2 |
| OzoneConcentration | OzoneConcentrationConverter, 544-559 | UnitOfRatio, 810-815; UnitOfDensity, 798-807; 3 |
| Power | PowerConverter, 562-583 | UnitOfPower, 505-514; 7 |
| Pressure | PressureConverter, 586-619 | UnitOfPressure, 645-659; 12 |
| RadiationConcentration | RadiationConcentrationConverter, 622-632 | UnitOfRadiationConcentration, 850-854; 2 |
| ReactiveEnergy | ReactiveEnergyConverter, 635-643 | UnitOfReactiveEnergy, 548-552; 2 |
| ReactivePower | ReactivePowerConverter, 646-659 | UnitOfReactivePower, 518-523; 3 |
| Speed | SpeedConverter, 662-764 | UnitOfSpeed, 858-869; UnitOfVolumetricFlux, 761-778; 13 |
| SulphurDioxideConcentration | SulphurDioxideConcentrationConverter, 767-780 | UnitOfRatio, 810-815; UnitOfDensity, 798-807; 2 |
| Temperature | TemperatureConverter, 783-902 | UnitOfTemperature, 595-600; 3 |
| TemperatureDelta | TemperatureDeltaConverter, 905-921 | UnitOfTemperature, 595-600; 3 |
| UnitlessRatio | UnitlessRatioConverter, 924-939 | UnitOfRatio, 810-815; includes None; 4 |
| Volume | VolumeConverter, 942-966 | UnitOfVolume, 671-687; 8 |
| VolumeFlowRate | VolumeFlowRateConverter, 969-1003 | UnitOfVolumeFlowRate, 691-704; 11 |

BaseUnitConverter behavior is at unit_conversion.py lines 113-191. The original
invalid-unit template is homeassistant/const.py line 1013. Exact unit strings
and every factor expression are encoded in supported_units and factor in
crates/hass-units/src/lib.rs.

## Conversion behavior

Base families use the source expression value / from_ratio * to_ratio. Each
factor is transcribed from the pinned _UNIT_CONVERSION table, including source
constant order and formulas for length, area, time, energy, mass, pressure,
volume, flow, radiation, and gas concentration. Gas ratios use the pinned
ambient values: 293.15 K, 101325 Pa, and gas constant 8.31446261815324.

Equal from and to identifiers return the value before validating either
identifier. This includes equal unknown strings and None/None for families
where None is not a supported unit. For other conversions, the base converter
looks up from first and to second. The Rust error preserves that side and uses
the source message template: “{} is not a recognized {} unit.” Speed validates
both sides but the original message always interpolates the target; the error
retains both the actual invalid side and the source-reported unit.

TemperatureConverter uses six affine formulas from lines 783-902. Temperature
delta uses the source factor table with Celsius and Kelvin factor 1.0 and
Fahrenheit factor 1.8, with no offset.

EnergyDistanceConverter marks mi/kWh and km/kWh as reciprocal units. When
exactly one side is reciprocal, it computes to_ratio / (value / from_ratio);
when both or neither are reciprocal, it uses the base factor formula. A zero
denominator returns DivisionByZero from convert. convert_optional returns None
only for an exactly zero input, as does the original allow-None factory. A
nonzero subnormal input whose normalized denominator underflows to zero still
returns DivisionByZero.

SpeedConverter handles Beaufort outside the factor table: m/s to Beaufort is
round(((m/s / 0.836) ** 2) ** (1 / 3)); Beaufort to m/s is
0.836 * Beaufort ** (3 / 2). Rust uses ties-to-even rounding to match Python's
round behavior. A finite negative Beaufort value produces a complex Python
result; because this API returns f64, it reports NonRealResult instead. NaN and
infinite speed values converted to Beaufort preserve Python's rounding errors
as CannotRoundNaN and CannotRoundInfinity. Finite power overflow is reported as
ArithmeticOverflow. These checks apply after the same-unit identity shortcut.

## Boundaries and gaps

Supported units follow the converter class table, not every constant in the
unit enum. Duration intentionally excludes UnitOfTime.MONTHS and YEARS.
MassVolumeConcentration excludes UnitOfDensity.MICROGRAMS_PER_CUBIC_FOOT.
Pollutant concentration converters accept only the ratio and density units
listed in their own VALID_UNITS sets; for example percentage is not accepted.
None is a valid unit only for UnitlessRatio, apart from the same-unit shortcut
described above.

The crate now includes the pinned UnitSystem selection tables and entry points.
It does not include sensor raw-unit aliases, state formatting, or application
configuration and sensor-registry preference integration. No conversion
family from the pinned module was left unimplemented.

## Checks

The Rust tests cover every family with a golden conversion, supported-unit
counts, source-compatible identity and error boundaries, temperature offsets
versus deltas, optional values, reciprocal energy-distance behavior, Beaufort,
and conversion composition across supported units. Gas and derived-unit golden
values use independent arithmetic from the pinned scalar constants rather
than calling the Python package implementation.

An independent source audit evaluated the pinned factor expressions as AST
data and called the actual native factor function in a temporary Rust harness.
All 190 f64 bit patterns matched across all 31 families, with no missing or
extra units. The audit also checked formula order, absolute temperature biases,
identity bypass, error-side reporting, and finite Beaufort tie boundaries.
Regression tests cover reciprocal nonzero underflow and Beaufort exceptional
values. The current focused suite contains 10 converter tests; sensor adoption
adds 14 projection tests and two unit-table tests.

## UnitSystem selection and conversion contract

The native `UnitSystem` selection foundation is transcribed from
`homeassistant/util/unit_system.py` at
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`. Its typed built-ins are
`METRIC_SYSTEM` and `US_CUSTOMARY_SYSTEM`; `IMPERIAL_SYSTEM` aliases the US
customary system. Metric defaults are km, mm accumulated precipitation, m², g,
Pa, °C, L, and m/s. US customary defaults are mi, in accumulated
precipitation, ft², lb, psi, °F, gal, and mph.

`UnitSystem::as_dict()` returns the source serialization keys and unit strings:
`length`, `accumulated_precipitation`, `area`, `mass`, `pressure`,
`temperature`, `volume`, and `wind_speed`. Configurable-unit domains are
property-specific: length and accumulated precipitation accept all nine
distance units; area accepts ten; mass accepts four (`lb`, `oz`, `kg`, `g`);
pressure accepts twelve; temperature accepts only °C and °F; volume accepts
eight; and wind speed accepts the thirteen `SpeedConverter` units, including
the four volumetric-flux identifiers. Thus configured temperature excludes K
even though the converter supports it, and configured mass excludes `st`,
`mg`, and `μg` even though the mass converter supports them.

`get_unit_system` accepts exact `metric` and `us_customary` keys. The separate
`validate_unit_system` behavior lowercases and maps deprecated `imperial` to
US customary; lookup itself does not accept `imperial`. Custom construction
validates all eight selections in source order. A custom conversion table
replaces the canonical map, and `get_converted_unit` retains `None` keys and
returns no mapping as `None`.

The source conversion methods dispatch to existing families: temperature to
Temperature, length and accumulated precipitation to Distance, area to Area,
pressure to Pressure, wind speed to Speed, and volume to Volume. UnitSystem
has no mass conversion method. No conversion factors are duplicated in this
selection module. Rust conversion methods take `f64`; they do not model Python
`numbers.Number` acceptance for bool, Decimal, Fraction, or other numeric
objects, nor Python `None` or complex values as method inputs. A missing unit
identifier remains an `Option` and is distinct from a converter error.

Focused tests include both canonical systems, every configurable-unit domain,
all UnitSystem conversion entry points, source table classes and exceptional
knots behavior, replacement custom maps, invalid selections and keys,
serialization keys, same-unit identity, temperature offset versus delta, and
distance-based accumulated precipitation. App configuration, persisted
preferences, sensor registry policy, aliases, and state formatting remain
separate integration work.

An independent AST-data audit compared the actual public Rust API, compiled
directly with `rustc`, with all 118 source mapping rows (56 metric and 62 US
customary). All eight selection domains and 6,208 mapping probes matched with
no missing, incorrect or extra results. Lookup, normalization, custom-map
replacement and `None` keys also matched. The native `as_dict` uses a sorted
`BTreeMap`; its iteration order differs from Python's insertion order, while
the serialized object keys and values match.
