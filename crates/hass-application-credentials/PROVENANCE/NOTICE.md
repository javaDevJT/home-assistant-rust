# Application credential slug provenance and notices

## Pinned behavior

The ID input is `f"{domain}.{client_id}"` in Home Assistant Core commit
[`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf):
[`application_credentials._get_suggested_id`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/application_credentials/__init__.py#L101-L103)
forms that value, and [`IDManager.generate_id`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/collection.py#L96-L106)
passes it to `slugify`. Home Assistant's [`homeassistant.util.slugify`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/util/__init__.py#L39-L44)
uses `_` as its default separator, returns an empty string for empty input, and
returns `"unknown"` when non-empty input becomes an empty slug.

That commit pins `python-slugify==9.1.3` in both
[`requirements.txt`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/requirements.txt#L49)
and [`homeassistant/package_constraints.txt`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/package_constraints.txt#L64).
The package's default is its frozen `legacy` algorithm with named, decimal, and
hexadecimal entity decoding enabled, `max_length=0`, and backend `auto`. Its
required transliteration dependency is `text-unidecode>=1.3`; `Unidecode` and
`anyascii` are optional extras. The pinned Home Assistant base and integration
requirement files do not request either extra. PyPI's release metadata lists
1.3 as the only `text-unidecode` release satisfying `>=1.3` at this audit, so
the declared dependency set selects `text-unidecode` 1.3. No Python package or
Home Assistant runtime was imported or executed during this port.

The port follows the legacy operation order: replace straight apostrophes with
hyphens; NFKD-normalize; transliterate; decode HTML4 named references, then
decimal references, then lowercase-`x` hexadecimal references; NFKD-normalize
again; lowercase; remove apostrophes; remove commas between decimal digits;
replace runs outside ASCII letters, digits, and hyphens with one hyphen;
collapse and trim hyphens; then map hyphens to underscores. The default has no
length truncation.

The frozen legacy decoder is deliberately different from the package's modern
algorithm: uppercase `X` does not start a hexadecimal reference, and a
`ValueError` or `OverflowError` leaves the entire decimal or hexadecimal pass
unchanged. Decimal conversion also honors CPython's default 4,300-digit input
limit. Python `chr()` accepts surrogate values; the Rust port uses a NUL
sentinel for those values because both that sentinel and a surrogate become one
disallowed-character separator in the later slug filter.

## Unicode and transliteration data

The Home Assistant pin requires CPython `>=3.14.2`. CPython 3.14.2's generated
Unicode database declares Unicode 16.0.0. The Rust dependency is therefore
pinned to `unicode-normalization = "=0.1.24"`, whose normalization tables also
declare Unicode 16.0.0. The exact version pin prevents Cargo from selecting
0.1.26, which declares Unicode 18.0.0.

| Included file | Source and transformation | SHA-256 |
| --- | --- | --- |
| `data/text-unidecode-1.3.bin` | Unmodified `src/text_unidecode/data.bin` from the PyPI `text-unidecode` 1.3 sdist; 65,535 BMP table slots, NUL-separated. | `7924666da4ce0ad24d4b8142b330e6d8e89465cd883934724cd9e4b7d8130f09` |
| `data/html4-name2codepoint.tsv` | Sorted `name2codepoint` literal from CPython 3.14.2 `Lib/html/entities.py`; 252 HTML4 names, not the separate HTML5 map. | `d48bccfd720ffa1de9d945dbbed34a6ff2c6b31b2e4d26400f8e90a14ec3b5df` |
| `data/unicode16-decimal-zeroes.txt` | The 76 starts of decimal digit blocks (`General_Category=Nd`) derived from Unicode 16.0.0 `UnicodeData.txt`; covers all 760 decimal digits used by Python's `\d` behavior. | `4822e3ebfa172bb896f9cbed8a88b026b4338b3213f2c7ab69bae3fff11163ce` |

The transliteration table has entries for code points U+0001 through U+FFFF.
The source maps U+0000 to itself and drops code points beyond its table; the
port retains those behaviors. The block index is built lazily from the bundled
table, so it does not add a second generated lookup asset.

## Source artifacts and verification

All source code was inspected as text. The only Python-derived runtime data is
the HTML4 name table; the Rust implementation does not copy Python code. The
table file from `text-unidecode` is redistributed unchanged.

| Artifact | SHA-256 |
| --- | --- |
| Home Assistant `application_credentials/__init__.py` at the pinned commit | `8304ccb2e6c7a9616201452c0fd07d84070b56c6d1d0a160a79731d91d8e2b21` |
| Home Assistant `helpers/collection.py` at the pinned commit | `7a4a242cd7f80c6139393fcc282f3bcd4788d1590e2f81f9f5bed4172bb277b1` |
| Home Assistant `util/__init__.py` at the pinned commit | `7519066f062d07434c7fdb815d8752640aedea6ffcc72011d18a3d0b82a8fd41` |
| Home Assistant `requirements.txt` | `713b76f06c40ba6d0f4bd9963daa156e3de5f7571faab142b8c66ea1af1238c0` |
| Home Assistant `requirements_all.txt` | `c8460ea7ab89160dc3708656696485b0e80cf0cd546daadbe371b137d2372df9` |
| Home Assistant `package_constraints.txt` | `751ec6a433873d8acc8de942b40b453375b65bdfbbc8936c943eab7af7b83789` |
| Home Assistant `pyproject.toml` | `edccb2538e6a61894bcfe6d9f9d78b5a44a7b395df7b67a14f2169fe711a0190` |
| PyPI `python_slugify-9.1.3.tar.gz` | `90e997f2e0987239ce95e12f700086eb18e1d1d3ee22624fbbdbd095afca42b6` |
| PyPI `text-unidecode-1.3.tar.gz` | `bad6603bb14d279193107714b288be206cac565dfa49aa5b105294dd5c4aab93` |
| CPython 3.14.2 `Lib/html/entities.py` | `d9c65fb2828dbc1f3e399058a341d51e9375ec5bca95a8e92599c41bd5b78bde` |
| CPython 3.14.2 `Modules/unicodedata_db.h` | `00a5ed9e602bb55e9f080333ac8cbc360352eea9de960eec69efdb0c3d68b110` |
| Unicode 16.0.0 `UnicodeData.txt` | `ff58e5823bd095166564a006e47d111130813dcf8bf234ef79fa51a870edb48f` |
| `unicode-normalization` 0.1.24 `src/tables.rs` | `69a0faad67afe03146d80ff5b2d889d1898cbfe8f7677a24f012148f6fd27348` |

The relevant primary artifacts are available from
[Home Assistant Core at the pinned commit](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf),
[PyPI python-slugify 9.1.3](https://pypi.org/pypi/python-slugify/9.1.3/json),
[PyPI text-unidecode 1.3](https://pypi.org/pypi/text-unidecode/1.3/json),
[CPython 3.14.2](https://github.com/python/cpython/tree/v3.14.2),
[Unicode 16.0.0 UCD](https://www.unicode.org/Public/16.0.0/ucd/UnicodeData.txt),
and [`unicode-normalization` 0.1.24](https://github.com/unicode-rs/unicode-normalization/tree/v0.1.24).

## License notices

- `data/text-unidecode-1.3.bin` is covered by the dual license shipped in
  `data/text-unidecode-LICENSE.txt`. This distribution selects the Artistic
  License 1.0 option; the complete upstream license text is included.
- `data/html4-name2codepoint.tsv` is derived from CPython's standard-library
  source and distributed with the PSF 2.0 license text in
  `data/cpython-PSF-2.0-LICENSE.txt`.
- `data/unicode16-decimal-zeroes.txt` is derived from the Unicode Character
  Database and distributed with Unicode's license text in
  `data/unicode-license.txt`.
- `python-slugify` source code was not copied; its MIT-licensed release is a
  behavioral reference only.
