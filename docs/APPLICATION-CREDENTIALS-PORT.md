# Application Credentials Port

## Native local acceptance

The member-40 implementation provides private version-1 storage, loaded-provider
factories, alias-aware in-use checks, source-compatible admin CRUD/streams,
secret-free authenticated configuration and YAML/app lifetime wiring. Twelve
SDK cases, five slug cases, nine protocol cases and compiled-process restart
coverage pass within the final 998-test/114-target workspace gate. Formatting,
strict all-target Clippy, optimized build and seven optimized CLI cases pass.
The macOS binary SHA-256 is
`6a517d876da5d337f80f8656e91ddb29b6a1f4f3a0604178db097c448bb17d57`.
Source `d9025ac2860539d05752990723d6b138f5c606a2` is published and passes
[exact Linux CI](https://github.com/javaDevJT/home-assistant-rust/actions/runs/42398882199),
including formatting, strict all-target Clippy, all tests and CLI validation.

The native subset is partial. The app registers no production credential
platforms and does not advertise the 54 upstream providers as working. OAuth
exchange, callbacks, refresh and provider acceptance are separate work in the
[OAuth source contract](OAUTH2-CLIENT-PORT.md). Admin serialization includes
secrets by source design; no physical memory-zeroization guarantee is claimed.
See the [SDK](../crates/hass-application-credentials/README.md) and its
[data provenance](../crates/hass-application-credentials/PROVENANCE/NOTICE.md).

## Source and boundary

The source of truth is Home Assistant Core
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`: the
`application_credentials` component, its generic storage-collection adapter,
the OAuth config-flow helpers, and their focused tests. This is a source-only
audit; Home Assistant and its dependencies were not imported or run.

Application Credentials owns persisted OAuth client registrations and makes
them available to an integration's `application_credentials` platform. It
does not own the general OAuth callback, token exchange, refresh, or token
error engine. Those contracts live in
`homeassistant/helpers/config_entry_oauth2_flow.py` and
`homeassistant/helpers/oauth2.py`. Port the credential manager and its
provider seam separately from a full OAuth engine; do not count a provider
registry or type stub as the engine.

The current Rust auth service is for Home Assistant users and sessions. Its
README explicitly leaves external auth providers and general credential
linking open (`crates/hass-auth/README.md:73-76`). An application credential
port should use a separate private credential store and bridge to the existing
config-entry identity; the local login `TokenSet` is not an OAuth client
credential.

The local `ConfigEntry` stores `data` and derives `Serialize`, but its
`public_json()` projection omits that field
(`crates/hass-config/src/lib.rs:81-92,158-184`). The current config-entry GET
adapter uses `public_json()` (`crates/hass-api/src/config_entries.rs:103-123`),
and the manager's config-entry change event is built from that projection
(`crates/hass-config/src/lib.rs:3390-3396`). Subscription snapshots and
updates use the projection as well
(`crates/hass-api/src/subscriptions.rs:507-525,785-820`). This is not a claim
that the current public list API exposes raw entry data. A dedicated
credential metadata key avoids coupling client secrets to general config-entry
data and its raw serialization path; verify each future adapter's projection.

## Pinned component contract

- Setup creates `Store(hass, 1, "application_credentials")`, loads it, and
  keeps an `ApplicationCredentialsStorageCollection` at the component's
  `HassKey`. The integration manifest has `config_flow: false` and depends on
  `auth` and `websocket_api`.
- A stored record has a collection `id`, integration `domain`, `client_id`,
  `client_secret`, and optional `auth_domain` and `name`. Create requires
  `domain`, `client_id`, and `client_secret`; `auth_domain` and `name` are
  optional. The schema marks `client_secret` with `probatio.Secret` and strips
  leading/trailing whitespace from `client_id` and `client_secret`.
- New IDs are suggested from `domain.client_id` and assigned by the
  collection ID manager. Update fields are empty and `_update_data` raises
  `ValueError("Updates not supported")`.
- Deleting a collection item checks config entries for that item's integration
  domain. A matching `entry.data["auth_implementation"]` blocks deletion with
  `Cannot delete credential in use by integration <entry.domain>`. After
  deletion, the normal collection persistence path saves the changed record
  set.
- The YAML compatibility API `async_import_client_credential` requires the
  component to be set up. It assigns `auth_domain` to the supplied value or
  the integration domain, and `name` to the supplied name or
  `Import from configuration.yaml`. Import uses the same suggested ID and is
  a no-op when that ID already exists; it does not update the stored secret.
- `async_client_credentials(domain)` selects records by integration domain
  and passes `client_id`, `client_secret`, and optional `name` to the provider.
  Its implementation key is `auth_domain` when present, otherwise the
  collection item ID. The default `AuthImplementation` name is the supplied
  name or `client_id`.

Pinned component anchors:

- `homeassistant/components/application_credentials/__init__.py:46-65`:
  schema and setup constants.
- `homeassistant/components/application_credentials/__init__.py:85-148`:
  storage, CRUD overrides, provider credential projection.
- `homeassistant/components/application_credentials/__init__.py:151-196`:
  setup and YAML import.
- `homeassistant/components/application_credentials/__init__.py:199-319`:
  local implementation and platform resolution.
- `homeassistant/components/application_credentials/__init__.py:322-374`:
  provider catalog and config-entry WebSocket handlers.
- `homeassistant/components/application_credentials/manifest.json:1-10`:
  integration dependencies and config-flow setting.

The source does not state an encrypted-at-rest guarantee for this Store. The
Rust implementation must make its own storage confidentiality boundary
explicit; placing the secret in general config-entry data, public state, or
unfiltered event payloads would widen the source boundary.

## Secret visibility and WebSocket API

The CRUD adapter is `DictStorageCollectionWebsocket` with
`admin_only=True` (`homeassistant/helpers/collection.py:560-570,592-598`). Its
generic collection contract says this restricts
`list` and `subscribe` as well as the always-admin mutating commands because
the items contain secrets. The `probatio.Secret` field marker is not an output
redactor: pinned component tests assert that both create and list responses
include `client_secret`. Therefore the exact upstream boundary is admin-only
secret visibility, not a secret-free admin projection. Preserve that response
shape for parity, or record an intentional hardening change and adjust the
admin UI contract; never infer redaction from the schema marker.

The component exposes these WebSocket commands; it defines no REST CRUD
route:

| Command | Contract |
| --- | --- |
| `application_credentials/list`, `create`, `update`, `delete`, `subscribe` | Generic collection commands. Admin-only; `create` returns the record and generated `id`; `list` returns stored records. `update` is unsupported. |
| `application_credentials/config` | Returns the available domain catalog and per-domain `description_placeholders`; the handler has no `require_admin` decorator and returns no credential records. |
| `application_credentials/config_entry` | Requires administrator authority and a string `config_entry_id`. Returns only `application_credentials_id` when the entry uses a credential, otherwise `{}`. An unknown entry returns `invalid_config_entry_id` / `Config entry not found: <id>`. |

The lookup first requires an available application-credentials platform for
the entry's domain; pinned `_async_config_entry_app_credentials` explicitly
calls `_get_platform` before matching records (`__init__.py:249-266`, especially
line 254). Native loaded-platform availability is the current boundary;
persisted records alone do not imply an available provider.

Concrete error shapes asserted by the component tests:

- Read-only `list`, `subscribe`, and `config_entry` attempts fail with
  `unauthorized`.
- Creating a record for a domain without a loadable
  `application_credentials` platform fails with `invalid_format` and
  `No application_credentials platform for <domain>`.
- Updating fails with `invalid_format` and `Updates not supported`.
- Deleting a missing item fails with `not_found` and
  `Unable to find application_credentials_id <id>`.
- Deleting an in-use item fails with `home_assistant_error` and the integration
  domain in the message above.

## Provider catalog and OAuth seam

At the pinned revision, `homeassistant/generated/application_credentials.py`
contains **54 built-in domains** (lines 6-61). A text-only comparison against
the pinned tree finds **54 matching
`homeassistant/components/*/application_credentials.py` provider modules**,
with no missing or extra domain. This is a source catalog count, not a count of
ported behavior or completed integrations. The loader adds custom integrations
whose manifest `dependencies` contain `application_credentials`; the runtime
catalog is therefore not limited to the generated built-in list. Source
anchors: generated list lines 6-61 and `homeassistant/loader.py:555-566`.

Setup registers the component's provider callback under
`application_credentials`. For each requested integration domain it loads the
`application_credentials` platform. A missing integration/platform produces
no provider result; a present platform must implement either
`async_get_authorization_server` or `async_get_auth_implementation`. If the
custom method exists, it builds an implementation for each stored credential;
otherwise the component constructs its `LocalOAuth2Implementation` from the
platform's authorization and token URLs. `async_get_description_placeholders`
feeds the non-secret `config` response.

The OAuth helper source separates the reusable implementation contract and
local implementation
(`homeassistant/helpers/config_entry_oauth2_flow.py:118-305`), the full
config-flow handler
(`homeassistant/helpers/config_entry_oauth2_flow.py:387-634`), and
implementation lookup/provider registration
(`homeassistant/helpers/config_entry_oauth2_flow.py:662-710`). Token HTTP/auth
helpers are in `homeassistant/helpers/oauth2.py:29-120`. The OAuth helper
registers providers by provider domain and asks them for a domain-keyed
implementation map. Config entries persist the selected `auth_implementation`.
The component's
`_async_config_entry_app_credentials` resolves that value back to a stored
item ID by matching either its ID or `auth_domain`; the config-entry WebSocket
command exposes the resolved ID, never the client secret. No credentials for
the requested integration makes the integration flow abort with
`missing_credentials`; credentials belonging to another domain do not satisfy
it.

### Source edge requiring an explicit decision

The delete guard compares `entry.data["auth_implementation"]` directly to the
collection `item_id`
(`homeassistant/components/application_credentials/__init__.py:113-127`).
Imported compatibility credentials default `auth_domain` to the integration
domain (`homeassistant/components/application_credentials/__init__.py:180-196`),
and an imported OAuth flow stores that value as
`auth_implementation` (`tests/components/application_credentials/test_init.py:622-640`).
The lookup helper deliberately maps an auth-domain value back to the item ID
(`homeassistant/components/application_credentials/__init__.py:249-267`), but
the delete guard does not use that mapping. Thus
an in-use imported credential whose auth domain differs from its item ID
appears not to be protected by the guard. The upstream tests prove protection
for a normal credential, but do not try to delete an in-use imported
credential.

For the native invariant, retain a stable credential ID and check both that ID
and its compatibility `auth_domain` when testing whether a config entry uses
it. Treat ambiguous duplicate aliases as blocked or invalid instead of
allowing a potentially live credential to be deleted. Keep the upstream
comparison as the exact parity oracle and add a focused falsifier for the
imported case; the primary owns the deliberate deviation decision.

Also test duplicate `auth_domain` values explicitly: the source constructs a
dictionary keyed by `auth_domain`, so later records with the same key replace
earlier ones in the provider result. No focused component test covers that
collision.

## Focused acceptance and falsifiers

Use actual storage, config-entry state, and the real WebSocket adapter for the
native implementation. A type stub, direct manager-only fake, or successful
credential write does not establish the component boundary.

- Reopen the runtime/store and prove the same record ID, domain, client ID,
  auth-domain choice, name, and secret are available to the provider. Prove
  duplicate YAML imports do not overwrite the saved credential.
- Prove read-only users cannot list, subscribe, create, update, delete, or
  inspect entry-to-credential association. Assert the exact upstream admin
  create/list secret projection, unless an intentional redaction deviation is
  recorded. Assert `config` exposes only supported domains and placeholders.
- Prove required and optional create fields, whitespace trimming, unsupported
  update, unknown-domain, missing-item, invalid-entry, and in-use error
  results against the actual WebSocket envelope.
- Prove a normal selected credential blocks deletion; removing its config
  entry permits deletion and makes the entry lookup return
  `invalid_config_entry_id`. Add the imported-credential deletion falsifier
  above before accepting the guard.
- Prove domain isolation, missing-credential abort, multiple ordinary client
  credentials, name fallback, imported `auth_domain` compatibility, and both
  default and custom provider construction. A catalog count alone does not
  prove any of these behaviors.
- Keep OAuth authorization, callback/state validation, code exchange, refresh,
  PKCE, token persistence and token error mapping in the OAuth-engine
  acceptance suite. Do not report them complete from a credential CRUD test.

Pinned focused test anchors:

- `tests/components/application_credentials/test_init.py`: WebSocket CRUD and
  admin tests (`test_websocket_list_empty` through
  `test_websocket_create_strips_whitespace`, lines 232-872); imports and
  dedupe (`test_websocket_import_config`, `test_import_duplicate_credentials`,
  `test_import_named_credential`, lines 387-458); provider/flow and in-use
  behavior (`test_config_flow_no_credentials`, `test_config_flow_other_domain`,
  `test_config_flow`, `test_config_flow_multiple_entries`,
  `test_config_flow_create_delete_credential`,
  `test_config_flow_with_config_credential`,
  `test_websocket_without_platform`,
  `test_websocket_without_authorization_server`,
  `test_platform_with_auth_implementation`,
  `test_websocket_integration_list`, `test_name`, and
  `test_remove_config_entry_without_app_credentials`, lines 459-845).
- `tests/helpers/test_config_entry_oauth2_flow.py`: implementation-provider
  registration and unavailable-provider handling (`test_implementation_provider`,
  `test_async_get_config_entry_impl_with_failing_and_succeeding_provider`,
  `test_async_get_config_entry_implementation_with_failing_provider`,
  `test_async_get_config_entry_implementation_missing_provider`, and the
  removed-provider fallback tests, lines 1034-1112 and 1757-2170); these test
  the adjacent OAuth engine, not the credential store.
- `tests/helpers/test_oauth2.py`: token client-auth and URL/HTTP behavior
  (`test_client_auth`, `test_build_authorize_url_keeps_endpoint_query`,
  `test_token_request_passes_headers`, and
  `test_token_request_refuses_redirect`, lines 41-180), also outside the
  credential CRUD port.

## Audit-only dependency notes

The component manifest depends on Home Assistant's `auth` and
`websocket_api` integrations and declares no component-specific Python package.
The pinned source uses `probatio==0.13.0` for schema/secret annotations. The
separate OAuth helpers import core dependencies pinned in
`homeassistant/package_constraints.txt`: `aiohttp==3.14.5`,
`yarl==1.25.1`, `multidict>=6.4.2`, `PyJWT==2.15.1`, and
`habluetooth==7.1.2`. These are source-audit version anchors, not a request to
add Python or Rust dependencies to the native port.
