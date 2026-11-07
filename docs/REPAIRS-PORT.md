# Native Repairs and Issue Registry Port Contract

This contract describes the next native System port. It is based on the exact
`home-assistant/core` source pin
[`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`](https://github.com/home-assistant/core/tree/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf).
The Python source was inspected with `git show <pin>:<path>`; the upstream
working tree was not checked out. This document is a source-grounded port plan,
not an implementation or a claim of parity.

## Ownership and current Rust fit

Use one app-owned native Repairs System, with the issue registry as its durable
state and a flow manager/platform registry as its process-local service. A
single `hass-repairs` crate is a reasonable boundary; the root owns manifest,
workspace, and API wiring. The manager should be cloneable but weakly held over
an owner-owned `Arc<Shared>` (the pattern in
[`TagManager`](../crates/hass-tag/src/lib.rs#L200-L240) and
[`TagManager::live`](../crates/hass-tag/src/lib.rs#L644-L660)). A dropped or
shut-down owner must make handles fail cleanly. Keep one registry per shared
`Runtime`; pass `Runtime::clone()` to managers rather than constructing another
runtime over the same store ([`Runtime::new` and store lease](../crates/hass-core/src/lib.rs#L649-L763)).

Persist the registry as JSON through `Runtime::get_data` / `set_data` under the
upstream key `repairs.issue_registry`; keep registry state out of the public
event history. `set_data` persists before success and shares the runtime write
order ([Runtime metadata and subscriptions](../crates/hass-core/src/lib.rs#L790-L847)).
For each mutation, build the candidate document, persist it, then publish the
in-memory replacement and its notification. A failed metadata write must leave
the old state visible and emit no successful-change event. Do not hold a
registry lock while invoking a native repair platform or config-entry driver.

The current `hass-config::ConfigEntries` loads its persisted metadata in
`ConfigEntries::new`, exposes typed `get_entry` / `list_entries`, and persists
metadata before publishing `config_entry_changed` ([entry model and manager
construction](../crates/hass-config/src/lib.rs#L41-L84),
[manager construction](../crates/hass-config/src/lib.rs#L534-L605),
[entry reads](../crates/hass-config/src/lib.rs#L720-L732),
[mutation and event publication](../crates/hass-config/src/lib.rs#L1806-L1835)).
Construct Repairs after config-entry metadata is available and before native
drivers run setup, so linked IDs can be checked and integrations can report
issues during setup. Current `ConfigEntryDriver::setup` receives `Runtime` and
the entry, but no Repairs context ([driver callbacks](../crates/hass-config/src/lib.rs#L279-L352));
a driver that reports issues must capture a weak Repairs handle when registered,
or a future shared integration context must add it. Do not assume an implicit
global manager.

`ApiFeatures` is the existing optional-System boundary and the WebSocket
subscription dispatcher already separates public runtime events from private
signals ([features](../crates/hass-api/src/lib.rs#L49-L66),
[subscription ownership](../crates/hass-api/src/subscriptions.rs#L26-L85),
[router](../crates/hass-api/src/lib.rs#L103-L128)). Add the Repairs manager
there and wire the existing HTTP/WebSocket adapters. Do not register a new
Runtime service: the pinned Repairs integration defines no service. The pinned
manifest is a `system` integration, depends on `http`, and disables config flow
([manifest](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/repairs/manifest.json));
its setup creates flow/platform state and registers WebSocket commands and HTTP
views ([setup](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/repairs/__init__.py#L23-L41),
[flow setup](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/repairs/issue_handler.py#L127-L143)).

## Issue identity, fields, and persistence

The registry key is the ordered pair `(domain, issue_id)`. The same issue ID
under another domain is distinct. `domain` is the producer/handler key;
`issue_domain` is optional display/translation attribution for an integration
that reports an issue on another integration's behalf, and must not replace the
identity key ([entry model](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/issue_registry.py#L58-L105),
[flow lookup](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/repairs/issue_handler.py#L82-L109)).

The source `IssueEntry` fields are `active`, optional
`breaks_in_ha_version`, `config_entry_id`, `config_subentry_id`, `data`,
`dismissed_version`, `is_fixable`, `issue_domain`, `learn_more_url`, `severity`,
`translation_key`, and `translation_placeholders`, plus `created`, `domain`,
`is_persistent`, and `issue_id`. `data` is a nullable string/integer/float/null
map; placeholders are a nullable string map. Keep severity as a string on the
Rust boundary: upstream types the three values `critical`, `error`, `warning`,
but its own tests load and list `"other"`, so rejecting unknown strings would
break observed behavior ([severity and model](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/issue_registry.py#L42-L78),
[unknown severity fixture](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/repairs/test_websocket_api.py#L718-L771)).
Serialize `created` as the source's ISO datetime string; preserve its offset
rather than replacing it with a local timezone. Creation validates a supplied
`breaks_in_ha_version` as a Home Assistant calendar version in the public
creation wrapper; it does not validate arbitrary issue IDs or the manager's
direct `get_or_create` call ([create wrappers](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/issue_registry.py#L641-L724)).

Storage identity and format are:

```text
key: repairs.issue_registry
major: 1
minor: 4
data: { "issues": [ ... ] }
```

The persisted envelope uses `version`, `minor_version`, and `data`, as shown by
the pinned fixtures. An active issue marked `is_persistent: true` stores all
issue metadata. A non-persistent issue stores only
`config_entry_id`, `config_subentry_id`, `created`, `dismissed_version`,
`domain`, `is_persistent: false`, and `issue_id`. `active` is runtime-only and
is never serialized. On restart, persistent issues become active with their
metadata intact; non-persistent issues become inactive shells with `data`,
fixability, severity, translations, URL, and breaking-version cleared. The
shell retains `created`, config links, and dismissal so a same-ID report can
reactivate it without losing the user's ignored state ([serialization and
migration](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/issue_registry.py#L58-L128),
[load and canonical save](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/issue_registry.py#L534-L618),
[restart assertions](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/helpers/test_issue_registry.py#L157-L197)).

Keep the minor migrations in order: 1.1 adds `is_persistent: false`; 1.2 adds
`config_entry_id: null`; 1.3 adds `config_subentry_id: null`; the current
minor is 1.4. Unknown object fields are not a hard load failure: the source
constructs entries from known keys and canonicalizes them on save, so unknown
fields are dropped by a normalized rewrite. Do not add strict unknown-field
rejection. Do not silently reset an unsupported major version or malformed
document. A duplicate `(domain, issue_id)` in the input list overwrites that
map value as it is read; the source does not reject it. New creates with the
same key are updates. Preserve issue iteration order for list responses: Python
dict order is insertion order, and the API iterates `issues.values()`; a sorted
map would change the response order ([load loop and save iteration](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/issue_registry.py#L534-L618),
[list iteration](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/repairs/websocket_api.py#L96-L119)).

## Mutation, ignore, refresh, and config-entry lifecycle

`get_or_create` validates a linked config entry and linked subentry before
mutation. A subentry without its parent, an unknown parent, or an unknown
subentry is rejected. A new row starts active with `created=now` and no
dismissal. Re-reporting the same key replaces all current producer metadata,
sets it active, but retains the original `created` and `dismissed_version`.
Only a changed replacement writes and emits `action: update`; a no-op emits
nothing. `ignore=true` sets `dismissed_version` to the current HA version;
`ignore=false` sets it to null. The WebSocket `ignored` field is computed from
whether this value is non-null. Deleting a missing key is a no-op; deleting an
existing key removes it and emits. `async_keep` marks a key seen without
changing fields ([mutators](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/issue_registry.py#L246-L412),
[linkage, hide, ignore, refresh selection](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/issue_registry.py#L414-L523)).

Refresh is a first-class API, not a bulk-delete convenience. A scope selects
one domain and either all IDs, explicit IDs, or IDs matched from the start by
the supplied regular expression; pattern and explicit IDs are mutually
exclusive, and an optional config entry narrows the scope. Clean finish removes
untouched issues. Create/update/delete/keep touches an ID in all active scopes.
For overlaps, the newest active refresh containing an ID owns its decision;
older scopes cannot delete an ID claimed by the newer scope. Cancel/exception
does not delete the scope's untouched issues, except for deletions explicitly
deferred to that refresh. `defer_to_refresh` lets a delete during reload wait
for the newest overlapping refresh; a later recreate/keep cancels that pending
delete. Finishing twice is harmless. Preserve these rules rather than replacing
them with one generation counter ([refresh implementation](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/issue_registry.py#L131-L243),
[refresh selection and delete](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/issue_registry.py#L373-L412),
[refresh API and deferred-delete wrapper](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/issue_registry.py#L725-L794)).

Config-entry interactions are observable requirements:

- Removing an entry removes every linked issue, active or inactive. Removing a
  subentry removes only rows with that exact `(config_entry_id,
  config_subentry_id)` pair. Same subentry IDs in other entries survive.
- Disabling an entry retains linked issues, but fires an update notification
  for each so listings can be refreshed. Active issues linked to a disabled
  entry are hidden from `repairs/list_issues`; re-enabling makes them visible.
- On startup, after persisted issues are assigned, wait until config entries
  finish initializing before deleting rows linked to missing entries/subentries.
  During the wait, config-entry setup can recreate/update rows. Never prune
  against a partially loaded config-entry set.
- Removing config entries also recomputes Home Assistant unique-ID-collision
  issues through a refresh. Current native `ConfigEntries` rejects duplicate
  per-domain unique IDs and has no equivalent collision-issue callback; treat
  existing legacy conflicts as a migration question, not a reason to weaken
  native uniqueness.

Source: [config-entry callbacks](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/config_entries.py#L2400-L2430),
[disable notifications](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/config_entries.py#L2688-L2721),
[subentry cleanup](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/config_entries.py#L2845-L2865),
[unique-ID issue refresh](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/config_entries.py#L3127-L3148),
[deferred registry cleanup after initialization](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/issue_registry.py#L582-L608).
Pinned bootstrap starts issue-registry loading eagerly alongside config-entry
initialization ([bootstrap ordering](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/bootstrap.py#L483-L498));
the loader waits only when stored issues contain config-entry links. Native
startup may sequence `ConfigEntries::new` before Repairs loading because its
persisted entries are available synchronously, as long as no linked row is
pruned earlier.

The native dependency is real: current Rust `ConfigEntry` has only
`supported_subentry_types` and reports `num_subentries: 0`; there is no stored
subentry map or subentry-flow manager. Current `ConfigEntries::remove_entry`
and `disable_entry` also have no Repairs cleanup/refresh callback
([current entry surface](../crates/hass-config/src/lib.rs#L41-L84),
[remove and disable](../crates/hass-config/src/lib.rs#L833-L930),
[public entry fragment](../crates/hass-config/src/lib.rs#L115-L140)). Full
linked-subentry and chained-subentry-flow parity depends on that native config
entry work. Entry removal should clear issues only after unload/remove succeeds
and removal is persisted; because config and issue documents are separate
metadata writes, do not claim a cross-document transaction. Startup orphan
cleanup is the recovery path if the second write fails.

## Repair flows and protocol contract

The native Repairs flow manager should own process-local flow sessions and
handler registrations. The platform contract creates a fix flow from
`(handler/domain, issue_id, issue.data)`. `issue_domain` is not the handler key.
If no `{domain}.repairs` platform is registered, the pinned behavior supplies a
confirmation-only flow; on confirmation it completes with an empty entry. A
flow's `issue_id` comes from flow context. Upstream still copies it into legacy
initial-step input and accepts legacy `data.issue_id` with a deprecation; do
not make that fallback a new API requirement. A completed non-abort flow
deletes the original issue; abort leaves it. Flows can chain to config, options,
or subentry flows and may include the corresponding public config-entry result
([flow manager](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/repairs/issue_handler.py#L24-L124),
[flow model and chaining](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/repairs/models.py#L23-L174),
[legacy flow contract tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/repairs/test_issue_handler.py#L21-L103),
[next-flow tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/repairs/test_models.py#L132-L214)).

The pinned command/route inventory is exact:

| Surface | Input | Result and behavior |
| --- | --- | --- |
| WS `repairs/list_issues` | `{ "type": "repairs/list_issues" }` | `{ "issues": [...] }`, active and not hidden; each item has `breaks_in_ha_version`, `created`, `dismissed_version`, derived `ignored`, `domain`, `is_fixable`, `issue_domain`, `issue_id`, `learn_more_url`, `severity`, `translation_key`, `translation_placeholders`. `data` and config-entry IDs are omitted. |
| WS `repairs/get_issue_data` | `domain`, `issue_id` | `{ "issue_data": data }`; missing issue returns `unknown_issue`. No fixability check. |
| WS `repairs/ignore_issue` | `domain`, `issue_id`, boolean `ignore` | Missing issue returns `unknown_issue`; otherwise changes dismissal and returns null. |
| HTTP `POST /api/repairs/issues/fix` | `handler`, `issue_id`; additional fields are accepted | Starts flow with `context.issue_id`. Unknown handler is 404; unknown step/entry is 400. Requires admin `POLICY_EDIT`. |
| HTTP `/api/repairs/issues/fix/{flow_id}` | GET progress, POST step, DELETE abort | GET/POST require admin `POLICY_EDIT`; DELETE inherits the base abort handler, which has no Repairs-specific admin decorator. GET/POST serialize a linked config entry through its public fragment. Unknown linked entry is 400. |

The three WS handlers have no per-command `require_admin` decorator in the
pinned source; preserve the base authenticated-WebSocket boundary rather than
silently adding or removing role checks. Flow creation and resource GET/POST
explicitly require admin `POLICY_EDIT`; the inherited resource DELETE abort
handler has no Repairs-specific admin decorator, so do not assume it shares
that role check ([WS handlers](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/repairs/websocket_api.py#L28-L119),
[flow HTTP authorization and errors](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/repairs/websocket_api.py#L135-L208),
[inherited resource DELETE](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/data_entry_flow.py#L106-L143)).
The Rust API currently has authenticated WebSocket identity/command policy and
optional Systems but no Repairs feature, command, or route ([native API
features](../crates/hass-api/src/lib.rs#L49-L66),
[command authorization](../crates/hass-api/src/extensions.rs#L173-L206)). Its
current `Identity` represents admin/owner rather than a separate `POLICY_EDIT`
permission, so exact fine-grained policy parity is not available yet.

`repairs_issue_registry_updated` has the payload
`{ "action": "create" | "remove" | "update", "domain": string,
"issue_id": string }`; it carries no issue data. The pinned code emits it via
`EventBus.async_fire_internal`, not the public `async_fire` wrapper. It is not
persisted by the event-bus call itself, but upstream dispatches it to ordinary
and match-all event listeners; this type is not in the pinned match-all
exclusion set ([issue event definition and emitters](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/helpers/issue_registry.py#L26-L48),
[match-all exclusions](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/core.py#L169-L172),
[internal event bus dispatch](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/core.py#L1655-L1746)).
The native runtime's `dispatch_signal` instead sends an unrecorded private feed
that public `subscribe_events` deliberately cannot see
([private signal API](../crates/hass-core/src/lib.rs#L901-L932),
[public event subscriptions](../crates/hass-api/src/subscriptions.rs#L273-L292)).
`Runtime::fire_event` is not a parity shortcut because it records a public
event. This visibility difference is an explicit open compatibility gap: keep
issue data off event payloads, do not invent a `repairs/subscribe` command (the
pinned Repairs adapter has none), and resolve event visibility at the Runtime
event boundary if generic public event subscribers must see this internal
notification.

The exact Repairs protocol has a list snapshot command but no Repairs-specific
subscription or initial replay. Generic Rust `subscribe_events` also starts at
subscription time without a snapshot. If a future native private feed is added,
make its snapshot/subscription ordering explicit; do not represent a new
subscription as upstream parity. `TagManager::snapshot_and_subscribe` is the
local atomic snapshot-plus-private-delta precedent
([implementation](../crates/hass-tag/src/lib.rs#L634-L650)), but it is not the
pinned Repairs API.

`get_issue_data` returns producer-supplied issue data to an authenticated
WebSocket client and passes it to the fix platform. Treat it as user-visible
data: do not place tokens, passwords, or config-entry `data`/`options` in it,
the list response, update notification, or logs. The current config-entry API
already redacts private fields from public fragments
([redaction contract](../crates/hass-config/README.md#L41-L52)); preserve that
when a repair flow returns a config entry.

### Native manager API checkpoint

The new crate's transport-ready contract is `RepairsIntegration::start(runtime,
config_entries, current_version) -> Result<RepairsIntegration>` plus
`integration.manager() -> RepairsManager`. The integration is the strong
process owner; `RepairsManager` is a cloneable weak handle and fails after the
owner is dropped. It accepts a real `ConfigEntryResolver` backed by
`ConfigEntries::get_entry`. A separate optional `ConfigSubentryResolver` is
required before creating a new issue with `config_subentry_id`; current native
config entries cannot confirm one. Loaded subentry IDs remain intact and are
not fabricated or pruned by assumption.

`RepairsManager` exposes issue `create/get/get-data/list/ignore/delete/keep`,
`refresh_issues` with a scoped `IssueRefresh` handle, and explicit
`clear_config_entry`, `clear_config_subentry`, and
`config_entry_disabled_by_changed` callbacks for the app/config owner to call
after real lifecycle mutations. It owns typed native handler registration
(`RepairsPlatform`) and the `start_fix_flow`, `flow_result`, `submit_step`,
and `abort_flow` lifecycle used by HTTP adapters. It does not own authorization
or expose a transport route. The API owner must enforce the pinned transport
permissions and serialize `IssueSummary`/`RepairsFlowProgress`, never the
internal `IssueEntry` wholesale. See the implementation checkpoint in
`crates/hass-repairs/README.md`.

## Focused acceptance falsifiers

These are the high-value failures to target when implementation starts; they
are based on the pinned tests below. They were inspected, not run for this
read-only port-contract task.

1. **Persistence/migration:** restart with both persistent and session issues.
   A persistent record loses any field or becomes inactive; a session record
   returns active, retains `created`, dismissal, or config links, or restores
   omitted producer metadata. Load fixtures for 1.1/1.2/1.3 and verify the
   added defaults and rewritten 1.4 envelope. Unknown JSON fields reject a
   valid record or cause the loader to erase unrelated valid rows. Duplicate
   `(domain, issue_id)` creates a second identity or collapses equal IDs across
   distinct domains. See
   [load/save and migration tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/helpers/test_issue_registry.py#L43-L207)
   and [migration fixtures](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/helpers/test_issue_registry.py#L418-L610).
2. **Update/event semantics:** unchanged recreation emits or persists an update;
   changed recreation resets creation time or dismissal; no-op ignore/delete
   emits; create/update/remove payload has extra data or the wrong key. A
   stored unknown severity such as `other` fails to round-trip. See
   [recreate and thread-safe wrapper cases](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/helpers/test_issue_registry.py#L251-L343)
   and [event assertions](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/helpers/test_issue_registry.py#L125-L155).
3. **Refresh races:** an issue outside scope is deleted; regex matches only
   suffixes (or only literal prefixes); a newer overlapping refresh cannot
   protect its key from an older finish; a recreated/kept issue is deleted; an
   exceptional cancel deletes untouched issues; a deferred delete cannot be
   canceled by recreate/keep; or finish twice has another effect. See
   [refresh selection and overlap tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/helpers/test_issue_registry.py#L1033-L1179)
   and [deferred-delete cases](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/helpers/test_issue_registry.py#L1313-L1357).
4. **Config links:** creation accepts an unknown entry/subentry; startup prunes
   before entries finish loading; linked rows survive successful entry or
   subentry removal; another entry's same subentry ID is removed; disable
   deletes the issue or leaves it visible; re-enable fails to refresh. See
   [link validation/startup](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/helpers/test_issue_registry.py#L613-L642),
   [entry/subentry removal and disable](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/helpers/test_issue_registry.py#L689-L804),
   and [wait for config initialization](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/helpers/test_issue_registry.py#L877-L943).
5. **WS/fix flow:** list leaks issue data/config IDs or includes inactive/disabled
   issues; get-data/ignore mishandles unknown IDs; fix accepts a missing or
   non-fixable issue; abort deletes an issue; success leaves it; issue ID is
   sourced from the wrong field; a missing platform has no confirmation flow;
   chained flow leaks private config-entry fields or loses the result; DELETE
   authorization is claimed without matching the inherited source handler. See
   [Repairs WebSocket API tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/repairs/test_websocket_api.py#L181-L265),
   [flow authorization](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/repairs/test_websocket_api.py#L599-L679),
   [list/visibility](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/repairs/test_websocket_api.py#L684-L877),
   and [flow completion/data tests](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/tests/components/repairs/test_websocket_api.py#L332-L406).
6. **Event visibility/security:** a Repairs update enters event history or
   public subscription semantics are claimed without implementation; the
   notification carries issue `data`; or a new Repairs subscription is added
   without upstream protocol evidence. Keep the private/public event mismatch
   explicit until the Runtime event boundary is resolved.

## Remaining limits

- Native `ConfigEntries` does not yet store or execute subentry flows, and its
  removal/disable lifecycle has no issue-registry callback. Those are concrete
  dependencies for full pinned linkage behavior.
- Native config-entry flows and drivers are process-local and callback-driven;
  there is no Python integration loader or upstream `LazyIntegrationPlatforms`
  equivalent. Only registered native issue producers and repair handlers can be
  covered by this System.
- This workspace has no Home Assistant frontend source or translations. The
  protocol and translation keys/placeholders can be ported and checked here;
  page rendering, localized issue text, and UI refresh behavior remain
  frontend work. There is no LLM dependency or LLM-generated repair behavior in
  the pinned Repairs implementation.
- The current native authentication adapter has admin/owner checks but no
  separate `POLICY_EDIT` permission object. Flow endpoints can enforce the
  closest available admin rule, but that is not proof of fine-grained policy
  parity.
- The native implementation in `crates/hass-repairs/**` is outside the 27-crate
  publication checkpoint. Root owns later workspace integration and README
  indexing.
