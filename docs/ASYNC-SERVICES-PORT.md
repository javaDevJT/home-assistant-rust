# Async service contract for media players

## Scope

Bounded source audit and additive contract proposal for the next executable media-player provider. No Core implementation changes are included. `MEDIA-PLAYER-PORT.md` identifies Spotify browse/search as response-only operations and calls out the transport permission gap.

## Current contract

- [`ServiceCall`, `ServiceResult`, and `Runtime::call_service`](../crates/hass-core/src/lib.rs#L583) are synchronous. A handler borrows `Runtime` and `ServiceCall`; `ServiceResult` already carries optional JSON response, changed states, and context.
- [`register_service`, `register_owned_service`, and `register_entity_service`](../crates/hass-core/src/lib.rs#L1277) use exact domain/service keys. Owned service names reject other owners and legacy replacements; entity providers are keyed by owner and are removed by owner. The resolver is a single guarded Runtime hook.
- `call_service` snapshots registrations without holding registry locks while invoking handlers. It resolves targets once, preserves the caller context, returns an empty success for resolver no-match, validates every entity/provider assignment before dispatch, invokes each owner once with its grouped IDs, and merges changed states and responses (zero: `None`; one: value; many: array).
- [`resolve_entity_service_targets`](../crates/hass-registry/src/targets.rs#L32) normalizes explicit target maps and supported flat selectors. It rejects `entity_id` in both target and data; unrelated legacy payloads remain untouched. Runtime provider matching establishes ownership, not caller permission.
- [`HTTP call_service`](../crates/hass-api/src/http.rs#L335) currently executes `call_service` via `spawn_blocking`; `?return_response` only selects the HTTP envelope. [`WebSocket call_service`](../crates/hass-api/src/websocket.rs#L1970) also uses `spawn_blocking`, always emits response/context, and is gated by `Identity.can_control` in [`extensions.rs`](../crates/hass-api/src/extensions.rs#L220). HTTP only has a few domain-specific admin checks. `Identity` has coarse read/control roles, not entity-level authorization.
- `description` JSON is discovery metadata, not a schema or capability gate. [`schedule.get_schedule`](../crates/hass-native/src/lib.rs#L1855) already advertises `supports_response: only`, but Core does not enforce it.
- Automation actions use `hass-automation::execute_automation` → `hass-script::run_sequence`. [`call_service_cancellable`](../crates/hass-script/src/lib.rs#L1483) runs every action through `spawn_blocking` and joins synchronous work after cancellation/deadline. [`apply_scenes`](../crates/hass-script/src/lib.rs#L2509) serially calls each scene entity service in `spawn_blocking`. Script and scene service registrations themselves are synchronous actor-queue wrappers (`register_services`, line 2185).

## Additive Core API

Use owned handler inputs so async callbacks need no borrowed call data across `.await`:

```rust
pub type ServiceFuture =
    Pin<Box<dyn Future<Output = Result<ServiceResult>> + Send + 'static>>;
pub type AsyncServiceHandler =
    Arc<dyn Fn(Runtime, ServiceCall) -> ServiceFuture + Send + Sync>;
```

Add a tagged sync/async handler kind inside the existing exact-key registries; keep one ownership and dispatch model rather than parallel maps that can disagree. Add async registration variants for legacy, owned, and entity-scoped services, using the same conflict/replacement rules as their sync counterparts. Entity async handlers use the same matcher and owner grouping. Keep the existing sync registration methods intact and default their response capability to `None`.

Add typed `ServiceResponseSupport::{None, Optional, Only}` metadata to registrations, with capability-aware variants for existing sync registrations too. Entity providers sharing one exact domain/service must declare the same value or registration fails. Preserve current `description` JSON and `services()` output; expose the typed value through a capability lookup for dispatch/adapters. Migrate `schedule.get_schedule` to typed `Only` while retaining its existing discovery field.

Keep `ServiceCall` unchanged so existing struct literals and serialized results remain source-compatible. Add a request/options type outside it:

```rust
pub struct ServiceCallRequest {
    pub call: ServiceCall,
    pub return_response: bool,
    pub authorize_targets: Option<Arc<dyn Fn(&[String]) -> Result<()> + Send + Sync>>,
}
```

Provide `Runtime::call_service_async(call)` for trusted/internal dispatch and `Runtime::call_service_request_async(request)` for transports. Both use one shared resolution/dispatch path; the request form enforces `None` + requested → unsupported, `Only` + omitted → required, and either choice for `Optional`. Keep `Runtime::call_service(call)` synchronous with its current behavior for sync registrations. It must return a clear async-required error for an async-only registration; it must never `block_on` an async handler.

The async dispatcher awaits async handlers directly. For existing sync registrations, it may use Tokio's shared `spawn_blocking` pool and await the join; it must not create an executor or thread per request. A started blocking handler cannot be interrupted: retain ownership of its join through cancellation/timeout and join it before reporting completion. If the outer request future can be dropped, an owning call scope/task tracker must still reap that join; dropping a `spawn_blocking` handle detaches the work. Native async handler futures should remain request-owned and must not launch detached provider work.

## Owned concurrent call continuation

The pinned generic Home Assistant turn handler groups by domain and awaits
`asyncio.gather`, preserving the caller's context
([`homeassistant/services.py:68-130`](https://github.com/home-assistant/core/blob/cbf2c02c9bff331537c19bff69da8dc11c3fbfbf/homeassistant/components/homeassistant/services.py#L68)).
Sequential domain awaits change behavior: a held or failed provider prevents
later domains from starting. Waiting for every result before returning the
first error also differs from the source's early error response.

The 48-crate candidate adds `Runtime::spawn_service_call_async`, returning an
`OwnedServiceCall` with `result()` and a cloneable cancellation handle.
Ordinary inline async dispatch retains its caller-cancellation behavior. The
opt-in call captures active execution scope at admission, retains and observes
its task independently of result-waiter lifetime, supports explicit
cancellation, and joins during Core shutdown. Completed unexpected join
failures remain visible to shutdown; intentional cancellation and ordinary
provider errors are separate outcomes.

The generic parent starts all domain calls before awaiting results; caller
cancellation cancels children, while a first-error response leaves
already-started siblings owned by Core. Focused Core and generic-control
checks pass 46 and 14 cases respectively, including scope propagation,
first-error sibling continuation, canceled callers and completed-panic
reporting. The complete expanded workspace gate is still pending.

## Ordering and lifecycle invariants

1. Apply the transport's coarse command authorization first (`can_control`, plus existing admin-only restrictions), then validate exact service registration and response capability.
2. Normalize target selectors exactly once. Before any provider runs, authorize every normalized entity ID through one API-owned policy passed in the request options. HTTP and WS must call the same policy. The current `Identity` has no entity ACL, so define the policy explicitly before claiming per-entity restrictions; do not treat provider ownership as user authorization.
3. A no-match selector stays an empty success with no handler side effects. A denied target rejects the whole request before partial provider dispatch. Keep unknown-service and target validation errors stable across transports.
4. Pass the original causal `Context` unchanged through target resolution and into every handler. Preserve the resolver's original context for no-match and entity-service aggregate results; preserve a legacy handler's own `ServiceResult` as before. Resolve and invoke once; do not probe by calling a handler, retry sync after async, duplicate state/event writes, or emit an extra generic service event. Existing state-change events still occur once from the handlers' state writes.
5. Use a unique owner token per provider lifecycle. Re-registration is exact `(domain, service, owner)` replacement; cleanup removes only that owner's registrations. On unload, stop new calls, cancel provider-owned async work, then drain it. Do not hold registry locks across awaits. In-flight async future drop should cancel its I/O; blocking legacy handlers are only joinable, not interruptible.

## Migration map and falsifiers

- `hass-core`: add handler kind, async dispatch, typed response support, request options, and cancellation-safe legacy fallback. Preserve old sync APIs and result shapes.
- `hass-api/src/http.rs` and `websocket.rs`: call the request-aware async API; retain HTTP `?return_response` and WS result envelopes, add WS's optional `return_response` input, and share authorization/target preflight. Reject response-capability mismatches before dispatch.
- `hass-script/src/lib.rs`: replace blocking service execution in `call_service_cancellable` with the async dispatcher and select it against the existing cancellation/deadline. Await scene entity calls serially in `apply_scenes` to preserve ordering, lineage, and partial-failure behavior. Keep synchronous built-in service wrappers compatible until separately migrated.
- `hass-automation/src/lib.rs`: no second dispatcher is needed; automation actions already enter the shared script sequence runner.
- Falsify compatibility with: an existing sync caller's same result/error; sync and async registration conflict/owner cleanup; exactly-once resolver/context/provider calls; zero/one/many response merge; HTTP and WS `None`/`Optional`/`Only` cases; unauthorized entity denial before any handler; cancellation of an async handler; and cancellation while a sync fallback is running without detached work. If any caller must synchronously invoke an async-only service, migrate that caller; never bridge with `block_on`.

## Focused existing checks

- Core: `service_handler_can_reenter_runtime_registry_and_state_methods`, `owned_services_allow_same_owner_updates_and_cleanup_only_owned_entries`, `service_target_resolver_handles_no_match_unknown_service_and_legacy_passthrough`, `entity_services_split_targets_and_merge_responses_and_states`, and `entity_service_validates_every_target_before_any_handler_runs` in [`hass-core`](../crates/hass-core/src/lib.rs#L2952).
- API/script/automation: `websocket_auth_and_subscription_service_round_trip` in [`websocket.rs`](../crates/hass-api/src/websocket.rs#L2465); `actions_cover_conditions_choose_repeat_variables_stop_and_service_errors` and `scenes_use_services_snapshot_metadata_and_restore_after_restart` in [`hass-script`](../crates/hass-script/src/lib.rs#L3644); `state_trigger_condition_and_service_preserve_context` in [`hass-automation`](../crates/hass-automation/src/lib.rs#L5309).
- Add focused tests for the new capability, cross-transport authorization, and cancellation cases above. No tests were run for this read-only audit.
