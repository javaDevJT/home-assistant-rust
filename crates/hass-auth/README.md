# Native authentication

`AuthManager` is a synchronous, cloneable auth service backed only by
`Runtime` metadata under the `hass_auth.v1` key. It initializes a persistent
HS256 signing key, commits each candidate state before publishing it to other
manager clones, and keeps raw refresh tokens out of storage. Passwords use
Argon2id PHC hashes; opaque refresh tokens are persisted as SHA-256 digests
and rotate on successful client-bound refresh.

## Implemented behavior

This port follows the pinned Home Assistant Core revision
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` for the local provider and auth
user/token model:

- `homeassistant/auth/providers/homeassistant.py` normalizes provider
  usernames with trim plus case folding, separates provider usernames from
  display names, and checks credentials before completing the login flow.
- `homeassistant/auth/models.py` exposes stable user IDs, owner/admin and
  active status, provider credential links, and refresh-token sessions.
- `homeassistant/auth/const.py` defines 30-minute access tokens, 90-day normal
  refresh tokens, and the `system-admin` / `system-users` groups.
- `homeassistant/components/auth/__init__.py` defines `auth/current_user`,
  `auth/long_lived_access_token`, and refresh-session management commands.
- `homeassistant/auth/__init__.py` signs HS256 JWTs with session issuer,
  subject, issue time, and expiration claims, then validates both the JWT and
  its backing active refresh-token session.

The Rust provider stores local users separately from public user records and
includes the pinned provider link shape (`homeassistant`, no provider ID) in
`User`. The mandated Argon2id hashes replace upstream's base64-encoded bcrypt
hashes. Local credentials can be enrolled for an account without a provider
link, updated by an administrator, and changed after verifying the current
password. Access tokens are checked against a persisted session on each
validation, so refresh-session revocation and disabling an account immediately
invalidates its access tokens.

Native TOTP uses the pinned provider's defaults (SHA-1, six digits, 30-second
steps, one-step clock window) with persisted per-user secrets and one-time step
checks. Enrollment returns an authenticator secret and `otpauth://` URI only to
the explicit setup caller. Password login reports an MFA challenge instead of
issuing tokens; the HTTP adapter holds the opaque five-minute challenge and
one-use login grant in its authorization-code flow. Direct `authenticate` and
`issue_tokens` calls do not issue tokens for MFA-enabled users.

`validate_access_token_identity` returns the active user and issuing session.
`handle_ws_session` uses that session for `is_current` metadata and
delete-all-except-current. Signed paths are short-lived HS256 capabilities
bound to one active session and the exact request path plus ordered non-safe
query pairs; `width` and `height` remain unsigned as in the pinned validator.
Validation returns the same user/session identity so the API adapter can apply
its usual role and local-network rules.

Supported WebSocket commands are `auth/current_user`,
`auth/mfa_modules`, `auth/setup_mfa`, `auth/depose_mfa`, `auth/sign_path`, `auth/long_lived_access_token`, `auth/refresh_tokens`,
`auth/delete_refresh_token`, `auth/delete_all_refresh_tokens`,
`auth/refresh_token_set_expiry`, and `config/auth/list`, `create`, `update`,
and `delete`. `auth/current_user` and `config/auth/list` return their pinned
public shapes without password hashes or signing keys. `config/auth/create`
can create an account without local credentials, as upstream does; this native
API exposes the local provider enrollment and password update commands for
such accounts.

The API owns user-bound MFA setup flows, authenticator QR SVG generation, and
the password -> TOTP -> one-use authorization code -> token exchange. Setup
secrets are returned only to the current native user. An active native session
can disable its own MFA through `auth/depose_mfa`, matching the pinned command.
OTP failures share a persisted per-user retry budget across newly created
challenges; restarting the manager does not clear that budget. Transport tests
exercise enrollment, another user's denied flow lookup, replay rejection,
token exchange, and disable over actual HTTP and WebSocket connections.

## Remaining upstream behavior

- External auth providers, general credential linking, and custom group
  permission policies remain open.
- Refresh-token rotation is a requested native security behavior; pinned Home
  Assistant's standard OAuth refresh flow retains its refresh token.

WebSocket command handling returns the command result payload only; the API
adapter creates the outer `id`, `type`, `success`, and `result` envelope.
