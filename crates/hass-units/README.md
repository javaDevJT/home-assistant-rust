# Native unit conversion

Dependency-free conversion for all 31 pinned converter families and 190 supported
unit entries. `convert` and `convert_optional` preserve optional unit identifiers,
same-unit identity shortcuts, source arithmetic order and fallible conversions.
The sensor SDK uses these converters for all 41 mapped device classes.

Independent source review matched all 190 native factor bit patterns. Tests cover
every family, identity and error boundaries, temperature offsets and deltas,
reciprocal conversion, underflow and Beaufort rounding exceptions.

See [the source map, behavior and representation limits](../../docs/UNITS-PORT.md).
Focused verification: `cargo test -p hass-units --locked --offline`.
