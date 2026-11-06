# Native configuration loading

The executable validates the complete resolved configuration before opening a
database or starting integrations. `hass --config configuration.yaml --check`
runs the same loader and typed validators used for startup.

The loader resolves `!include`, `!include_dir_list`, `!include_dir_named`,
`!include_dir_merge_list`, and `!include_dir_merge_named` relative to the file
containing the tag. Directory loading is deterministic and skips hidden files
and `secrets.yaml`. Cycles, unknown tags, conflicting package keys, malformed
files, and missing dependencies fail explicitly.

```yaml
homeassistant:
  time_zone: America/New_York
  packages: !include_dir_named packages
automation: !include automations.yaml
script: !include scripts.yaml
mqtt:
  host: !env_var MQTT_HOST localhost
  password: !secret mqtt_password
```

`!secret` searches the requesting file's directory and then its parents up to
the configuration root. Nested `secrets.yaml` files override parent values.
Secrets are YAML values; their expected type is checked by the integration's
validator. `!env_var NAME default value` supplies a string default when the
variable is absent. Required missing values are errors.

`homeassistant.packages` merges integration sections into the root before
typed validation. List-based native sections append entries; mapping sections
merge with duplicate conflicts rejected. The resolved package source is kept
by the YAML loader and removed at the typed application boundary to avoid
storing a second secret-bearing configuration object.

Each file is limited to 1 MiB, a load to 8 MiB and 1,024 files, and directory
traversal to 16,384 entries. Include depth is limited to 64 and YAML value
depth to 128. File parse errors report filenames and positions without source
snippets. Typed decode failures retain the field path without quoting resolved
secret values.

Supported integration options are defined by the strict native configuration
types and each crate's README in the [project index](../README.md). Unknown
integrations and options fail validation. Loader support does not imply that
every integration referenced by an upstream configuration has been ported.

The native zone component always creates `zone.home` from the accepted core
name and coordinates. A `zone:` list, one zone mapping, or an empty section
adds YAML zones. Editable zones use the authenticated `zone/*` WebSocket
collection separately from YAML definitions. The administrator-only
`zone.reload` service resolves the current YAML and its includes again and
replaces only the YAML zone collection. It does not reconfigure other domains.
Saved `config/core/update` settings update the home zone and take precedence
over startup YAML on subsequent restarts.
