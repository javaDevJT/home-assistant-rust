# hass-application-credentials

Native private storage and loaded-provider callbacks for Home Assistant
application credentials. This collection holds OAuth client IDs and client
secrets; provider access and refresh tokens belong to the separate OAuth client.

`ApplicationCredentialsIntegration::new(runtime, config_entries)` owns the
version-1 collection. Its manager and platform registry are weak handles.
Register a provider with `platform_registry().register(domain, platform)` and
retain its exact-lifetime guard while the integration is loaded. The catalog
contains registered providers only. A manifest entry does not register a
provider or make its authorization endpoints available.

The trusted manager supports create/list/delete/import, configuration-entry
association, provider implementations and bounded collection subscriptions.
Updates are unsupported. Client ID and secret trimming, import deduplication,
alias ordering and optional-field serialization follow the pinned source.
Trimming includes CPython's four ASCII information separators. IDs use the
pinned Unicode 16 normalization and text-unidecode 1.3 transliteration table,
including legacy entity-decoding failure behavior and collision suffixes.
Data hashes and licenses are recorded in [PROVENANCE/NOTICE.md](PROVENANCE/NOTICE.md).
Deleting a credential used by a matching-domain entry is blocked for both its
stable ID and effective authorization alias; the latter intentionally fixes
the pinned source's imported-alias deletion gap.

Admin DTO serialization includes client secrets to match the source API.
The WebSocket host authorizes administrators before accessing these DTOs;
the provider catalog is secret-free. Debug output redacts credential values.
Runtime metadata stores records privately, outside public configuration-entry
data and public state/event payloads. Persistence succeeds before cache changes
or collection events become visible. Invalid or future stored documents fail
without being rewritten.

Subscriptions atomically capture a snapshot and register for subsequent changes.
Their bounded queues report lag explicitly. Shutdown stops admission, clears
queued data and provider registrations, and waits for admitted factory calls.
Provider callbacks run outside registry and collection locks. A callback must
not await its own integration's shutdown barrier. Registration closure captures
are destroyed outside registry locks, allowing nested guard cleanup.

The SDK returns local OAuth descriptors or opaque custom implementations; it
does not exchange authorization codes, refresh tokens, open a browser, contact
providers, or implement an external config flow. The source contract and
remaining host boundaries are in
[APPLICATION-CREDENTIALS-PORT.md](../../docs/APPLICATION-CREDENTIALS-PORT.md).

Tests use injected providers, temporary SQLite stores and synthetic credential
values. Run `cargo test -p hass-application-credentials --locked --offline`.
