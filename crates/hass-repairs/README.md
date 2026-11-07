# hass-repairs

Native Repairs issue registry and process-local fix-flow manager. The crate is
intentionally outside the root Cargo workspace and 27-crate publication gate;
the root owner controls later workspace and application integration.

## Owner and persistence

`RepairsIntegration::start(runtime, config_entries, current_version)` loads the
`repairs.issue_registry` Runtime metadata document. It accepts storage version
1, migrates minor versions 1.1, 1.2, and 1.3 to 1.4, ignores unknown record
fields while rewriting canonically, defaults a missing `minor_version` to 1.1,
and rejects malformed or unsupported documents. The process owner keeps the
state alive; `integration.manager()` is a cloneable weak handle that fails after
the owner is dropped. A `shutdown` call is not needed to release the owner.

Persisted session issues restore as inactive identity shells; persistent issues
restore active with their metadata. `active` is runtime-only. Registry
mutations persist with `Runtime::set_data` before replacing the visible issue
list, then emit `repairs_issue_registry_updated` through Runtime's ordinary
public event bus. The payload contains only `action`, `domain`, and `issue_id`;
it carries no issue body or data map. Runtime records public events in generic
event history, so Repairs notifications appear there under the current native
event-bus contract. Metadata and event writes are separate Runtime operations;
if event recording fails after the metadata commit, the issue change remains
durable while the manager returns the event-storage error.

`ConfigEntryResolver` reads the real initialized config-entry set. `start`
also uses `ConfigEntries::subentry_exists` to validate new parent/child links
and prune links to children that no longer exist. Custom hosts may use the
optional `ConfigSubentryResolver`; without it, subentry-linked creation fails
explicitly and stored child links are retained because absence cannot be
verified.

## Issue registry API

`IssueDraft::new(domain, issue_id, is_fixable, severity, translation_key)` sets
the required fields and defaults `is_persistent` to false. Optional producer
metadata follows the pinned `IssueEntry`; data values are limited to
string/number/null, and severity remains an open string for forward
compatibility. Producers must keep credentials out of issue data because the
authenticated `repairs/get_issue_data` command returns that map.

`RepairsManager` provides:

- `create_issue`, `get_issue`, `get_issue_data`, `list_issues`, `ignore_issue`,
  and `delete_issue`;
- `keep_issue` and `refresh_issues(RefreshScope) -> IssueRefresh`. Refresh
  scopes support all IDs, explicit IDs, regular-expression matches, and an
  optional config-entry filter. Pattern matching starts at the beginning of an
  issue ID, matching Python's `re.match`. A refresh's `keep` only applies to
  IDs in its own scope and binds older overlapping refreshes, not newer ones.
  `finish` deletes untouched selected rows; `cancel` skips that cleanup.
  Deferred deletion belongs to the newest active scope containing the ID, is
  ignored when that owner already touched the ID, and is applied when that
  owner finishes or cancels without a newer overlapping owner. Later creation
  or keep cancels a pending delete;
- `clear_config_entry`, `clear_config_subentry`, and
  `config_entry_disabled_by_changed` for a host to call after a successful
  config-entry lifecycle mutation;
- `register_platform` / `unregister_platform` by issue producer domain.

The API list DTO is `IssueSummary`: it contains the upstream display fields and
derived `ignored` flag, but omits issue data and config-entry IDs. Inactive
issues and issues linked to disabled entries are hidden. A create emits
`action: create`, a changed update emits `action: update`, and removal emits
`action: remove`. No-op operations do not persist or emit.

## Native fix flows

`RepairsPlatform::create_fix_flow(FixFlowRequest)` receives the issue's stored
data and returns a `RepairsFlow` callback. The flow context preserves caller
metadata and initial input separately from the issue's stored data; the
authoritative `issue_id` is present in both context and initial input, and the
first callback receives `None` as its user input. The no-platform path provides
the upstream confirmation-only form with an empty schema and issue translation
placeholders. Any submitted form input completes that confirmation.

Create and abort are terminal outer Repairs results. They carry the handler's
payload in `data` (create only), optional `next_flow` metadata as the source
`[flow_type, flow_id]` pair, and `result` as the linked config entry's public
fragment. A new config flow has a null linked result; options and subentry flows
link their actual parent config entry. `NextFlow::new` accepts only a type and
active flow ID; the resolver verifies that the selected manager owns that ID
and derives any entry link from the manager's own result metadata. Missing or
wrong-manager flow IDs fail as unknown flows, and missing linked entries fail
as unknown entries.

The child flow continues independently through its config, options, or
subentry manager. A successful outer Repairs result removes the issue; an outer
abort retains it regardless of later child-flow completion. Completed and
aborted Repairs flow IDs are removed immediately, so later GET, submit, or
abort requests return unknown flow. Only active Repairs sessions remain in
the process-local flow store.

`start_fix_flow(handler, issue_id)`, `init_fix_flow(handler, context, data)`,
`flow_result(flow_id)`, `submit_step(flow_id, user_input)`, and
`abort_flow(flow_id)` form the typed transport API. Responses use
`RepairsFlowProgress` and preserve the incoming context. The adapter is
responsible for the pinned WebSocket authentication boundary and HTTP
`POLICY_EDIT` checks. Current native auth exposes admin/owner rather than a
separate `POLICY_EDIT` permission, so an adapter must describe its closest
available role check accurately. Linked entries are returned through their
public fragments only.

## Checks and limits

The crate has not been added to the root workspace or application/API wiring.
Focused validation uses its isolated local workspace:

```sh
cargo test --manifest-path crates/hass-repairs/Cargo.toml --offline
cargo clippy --manifest-path crates/hass-repairs/Cargo.toml --offline --all-targets -- -D warnings
```

The host must call the explicit issue-registry lifecycle methods after
successful config-entry mutations. The frontend, translations, and
fine-grained policy model are outside this crate. No Python integration
loader, synthetic service, or LLM behavior is part of the pinned Repairs
implementation.

## Active implementation contract: registry corrections

This section records the registry acceptance contract for the native Repairs
port at the pinned Home Assistant revision `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`.

- A stored Repairs document with `version: 1` and no `minor_version` is read as
  minor version 1, then canonicalized to 1.4. Session-only issues restore as
  inactive identity shells with `is_fixable: None`; active issue creation still
  takes the non-null `IssueDraft.is_fixable` boolean. Unknown severity strings
  remain round-trippable.
- Non-empty `breaks_in_ha_version` values are validated as CALVER before the
  public `create_issue` mutation. Empty strings retain the helper's no-check
  behavior.
- `IssueDraft` and `IssueSummary` keep their public shapes. Changed updates
  retain `created` and `dismissed_version`; no-op updates and repeated ignore
  operations do not persist or emit. `ignored` is derived from whether
  `dismissed_version` is present. The list omits issue data and config IDs;
  `get_issue_data` returns producer-supplied user-visible data.
- Registry notifications remain ordinary public `repairs_issue_registry_updated`
  events containing exactly `action`, `domain`, and `issue_id`. They use the
  Runtime's normal event-history/Recorder path; no private-signal or extra
  delivery channel is part of the registry contract.
- Refresh regression cases cover prefix matching, disjoint scoped keeps,
  overlapping refresh ownership, deferred-delete after keep/recreate, and
  cancellation of the newest owner while an older scope remains active. The
  legacy storage case also covers an envelope missing `minor_version`.
