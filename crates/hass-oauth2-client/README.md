# hass-oauth2-client

A native OAuth authorization-code/PKCE client for provider accounts, separate
from Home Assistant user login sessions and application client registrations.

Create one `OAuthClientFactory` for a runtime storage domain and retain it while
using its clients and sessions. Supply an atomic private `OAuthTokenStore` and
optionally an injected clock. Client configuration contains the actual loaded
provider's endpoints and credentials; the crate does not infer providers from
manifests or resolve opaque custom OAuth implementations.

Authorization produces a browser URL with signed, expiring state bound to its
private flow, redirect URI and PKCE verifier. Hosts can verify the state into an
opaque proof, return an external-done step, and resolve the proof later. Code
exchange consumes the flow once. Denial, unknown/expired state, replay, malformed
responses and transport failures return sanitized typed errors.

Token POSTs refuse redirects and bound their timeout and response body. Server
and bearer-resource URLs require HTTPS, with literal loopback HTTP permitted.
Endpoint userinfo/fragments and reserved OAuth query parameters are rejected.
Debug/error values omit credentials, codes, provider bodies and URL query data.
Bearer headers replace previous authorization values and remain sensitive.

Sessions reload the current private token under a shared account lock. They
refresh within the source's 20-second expiry margin, require a fresh access
token and lifetime, preserve an omitted refresh token and unrelated provider
fields, and commit before returning a bearer request. Valid integer strings,
Unicode 16 decimal digits and finite floats follow Python integer coercion;
invalid, bool, zero and overflowing lifetimes fail explicitly. Expiry starts at
response receipt, and normalized lifetimes are stored as integers.

Use `session.start_authorization` for an existing account. The result carries
its issuing factory/client, account and revision; stale or mismatched saves
return `CredentialConflict`. A direct client flow can create only an empty
account untouched by this factory. `commit_authorization` lets a host complete
its entry and exact reauth flow in one private transaction under that revision
barrier. Execute deferred integration setup/reload after the method returns.
Callbacks must not reenter token operations for the same account or await the
factory's own shutdown barrier.

Admitted token reads, refreshes and saves are owned jobs. Canceling a waiting
caller leaves admitted commits tracked. Await `factory.shutdown()` to close
admission and join them; concurrent waiters share the same outcome and job
panics remain observable after handle reaping. Dropping the last factory owner
closes admission and aborts retained work as a cleanup fallback. External store
writers require their own ownership or CAS policy. Actual storage failure after
provider rotation can require reauthentication.

The source and host-flow boundaries are documented in
[OAUTH2-CLIENT-PORT.md](../../docs/OAUTH2-CLIENT-PORT.md). Tests use synthetic
stores/clocks and owned loopback token endpoints. No real provider, credential,
browser or device is used.
