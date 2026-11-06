//! Native Home Assistant authentication backed by runtime metadata.
//!
//! User records and the local password provider are stored through
//! [`Runtime`] metadata. Public [`User`] values never contain credentials or
//! password hashes. See `README.md` for the upstream behavior covered here and
//! the remaining authentication boundaries.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use hass_core::{Error, Result, Runtime};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use totp_rs::{Algorithm as TotpAlgorithm, Builder as TotpBuilder, Secret as TotpSecret, Totp};
use uuid::Uuid;

const AUTH_DATA_KEY: &str = "hass_auth.v1";
const AUTH_SCHEMA_VERSION: u32 = 1;
const AUTH_PROVIDER_TYPE: &str = "homeassistant";
const GROUP_ID_ADMIN: &str = "system-admin";
const GROUP_ID_USER: &str = "system-users";
const GROUP_ID_READ_ONLY: &str = "system-read-only";
const TOKEN_TYPE_NORMAL: &str = "normal";
const TOKEN_TYPE_LONG_LIVED: &str = "long_lived_access_token";
const ACCESS_TOKEN_EXPIRATION_SECS: u64 = 30 * 60;
const REFRESH_TOKEN_EXPIRATION_SECS: i64 = 90 * 24 * 60 * 60;
const MAX_PASSWORD_BYTES: usize = 4096;
const MAX_USERNAME_BYTES: usize = 255;
const MAX_DISPLAY_NAME_BYTES: usize = 255;
const MAX_CLIENT_NAME_BYTES: usize = 255;
const MAX_CLIENT_ID_BYTES: usize = 2048;
const MAX_LONG_LIVED_DAYS: u64 = 3650;
const MFA_TOTP_ID: &str = "totp";
const MFA_TOTP_NAME: &str = "Time-based One Time Password";
const MFA_LOGIN_CHALLENGE_TTL_SECS: i64 = 5 * 60;
const MFA_LOGIN_GRANT_TTL_SECS: i64 = 5 * 60;
const MFA_ENROLLMENT_TTL_SECS: i64 = 10 * 60;
const MFA_MAX_RETRY_TIME: u8 = 5;
const MAX_SIGNED_PATH_EXPIRATION_SECS: u64 = 24 * 60 * 60;
const MAX_SIGNED_PATH_BYTES: usize = 8192;

/// Safe credential-provider information exposed by `auth/current_user`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserCredential {
    /// Provider type, currently `homeassistant` for local credentials.
    pub auth_provider_type: String,
    /// Provider identifier. The built-in Home Assistant provider has no ID.
    pub auth_provider_id: Option<String>,
}

/// Safe MFA module information exposed by `auth/current_user`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MfaModule {
    /// Stable module identifier.
    pub id: String,
    /// Human-readable module name.
    pub name: String,
    /// Whether the module is enabled for this user.
    pub enabled: bool,
}

/// Public user information compatible with Home Assistant's auth user shape.
///
/// This type intentionally has no password, credential data, refresh token,
/// or JWT key fields.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct User {
    /// Stable user identifier.
    pub id: String,
    /// Local provider username, when this user has local credentials.
    pub username: Option<String>,
    /// Display name returned by `auth/current_user`.
    pub name: String,
    /// Whether this is the installation owner.
    pub is_owner: bool,
    /// Whether this active user has administrative privileges.
    pub is_admin: bool,
    /// Whether the user can authenticate.
    pub is_active: bool,
    /// Whether requests are restricted to local network access, if configured.
    pub local_only: Option<bool>,
    /// Whether Home Assistant created this as a system user.
    pub system_generated: bool,
    /// Native role/group IDs.
    pub group_ids: Vec<String>,
    /// Public provider links; provider data is kept private.
    pub credentials: Vec<UserCredential>,
    /// The native TOTP module and whether it is enabled for this user.
    pub mfa_modules: Vec<MfaModule>,
}

/// Authenticated identity recovered from an access token or signed path.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AccessIdentity {
    /// Public user record associated with the credential.
    pub user: User,
    /// Active refresh-token session that issued the credential.
    pub session_id: String,
}

/// Result of checking a local password for the HTTP login flow.
///
/// `MfaRequired` includes an opaque challenge handle for server-side flow state;
/// adapters should return their own flow ID to clients instead.
#[derive(Clone, PartialEq, Eq)]
pub enum LoginAuthResult {
    /// Password was valid and the user has no enabled MFA module.
    Authenticated { user: User },
    /// Password was valid and a TOTP challenge must complete before token issue.
    MfaRequired { user: User, challenge_id: String },
}

impl fmt::Debug for LoginAuthResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authenticated { user } => formatter
                .debug_struct("Authenticated")
                .field("user", user)
                .finish(),
            Self::MfaRequired { user, .. } => formatter
                .debug_struct("MfaRequired")
                .field("user", user)
                .field("challenge_id", &"[REDACTED]")
                .finish(),
        }
    }
}

/// One-time proof that a login challenge completed its enabled MFA step.
///
/// Keep this value in server-side auth-code state. Its serialized form is an
/// opaque capability and its `Debug` output is redacted.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct MfaLoginGrant(String);

impl fmt::Debug for MfaLoginGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MfaLoginGrant([REDACTED])")
    }
}

/// TOTP setup data shown only during the authenticated enrollment flow.
#[derive(Clone, PartialEq, Eq)]
pub struct MfaEnrollment {
    /// Base32 secret for an authenticator app.
    pub secret: String,
    /// Provisioning URI suitable for an authenticator app or QR renderer.
    pub otpauth_uri: String,
}

impl fmt::Debug for MfaEnrollment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MfaEnrollment")
            .field("secret", &"[REDACTED]")
            .field("otpauth_uri", &"[REDACTED]")
            .finish()
    }
}

/// Bearer token pair returned by the native login and refresh operations.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct TokenSet {
    /// Signed JWT access token.
    pub access_token: String,
    /// OAuth token scheme, always `Bearer`.
    pub token_type: String,
    /// Access token lifetime in seconds.
    pub expires_in: u64,
    /// Opaque refresh token. Only a SHA-256 digest is persisted.
    pub refresh_token: String,
}

impl fmt::Debug for TokenSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenSet")
            .field("access_token", &"[REDACTED]")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("refresh_token", &"[REDACTED]")
            .finish()
    }
}

/// Cloneable native auth manager.
///
/// Clones share one in-memory lock and always persist candidate state before
/// making it visible to other clones.
#[derive(Clone)]
pub struct AuthManager {
    inner: Arc<AuthInner>,
}

struct AuthInner {
    runtime: Runtime,
    state: Mutex<AuthStore>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthStore {
    version: u32,
    /// The persistent HMAC key used to sign access tokens.
    jwt_secret: String,
    users: BTreeMap<String, StoredUser>,
    /// Mirrors the pinned HA local provider's `users` data, but stores PHC
    /// Argon2id hashes as required for this native implementation.
    homeassistant_provider: HomeAssistantProviderData,
    sessions: BTreeMap<String, StoredSession>,
    /// Added compatibly to schema v1; older stores deserialize as an empty map.
    #[serde(default)]
    mfa: MfaStore,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MfaStore {
    totp_users: BTreeMap<String, StoredTotpCredential>,
    pending_enrollments: BTreeMap<String, StoredTotpEnrollment>,
    login_challenges: BTreeMap<String, StoredMfaLoginChallenge>,
    login_grants: BTreeMap<String, StoredMfaLoginGrant>,
    #[serde(default)]
    retry_budgets: BTreeMap<String, MfaRetryBudget>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MfaRetryBudget {
    attempts: u8,
    expires_at: i64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredTotpCredential {
    secret: String,
    last_used_step: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredTotpEnrollment {
    secret: String,
    expires_at: i64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredMfaLoginChallenge {
    user_id: String,
    created_at: i64,
    expires_at: i64,
    attempts: u8,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredMfaLoginGrant {
    user_id: String,
    expires_at: i64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HomeAssistantProviderData {
    users: BTreeMap<String, ProviderUser>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProviderUser {
    username: String,
    password_hash: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredUser {
    id: String,
    username: Option<String>,
    name: String,
    is_owner: bool,
    is_active: bool,
    local_only: Option<bool>,
    system_generated: bool,
    group_ids: Vec<String>,
    #[serde(default)]
    mfa_enabled: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredSession {
    id: String,
    user_id: String,
    client_id: Option<String>,
    client_name: Option<String>,
    client_icon: Option<String>,
    token_type: String,
    refresh_token_hash: String,
    access_token_expiration_secs: u64,
    refresh_expires_at: Option<i64>,
    created_at: i64,
    last_used_at: Option<i64>,
}

struct SessionOptions {
    client_id: Option<String>,
    client_name: Option<String>,
    client_icon: Option<String>,
    token_type: &'static str,
    access_token_expiration_secs: u64,
    refresh_lifetime_secs: Option<i64>,
    unique_long_lived_client_name: bool,
    /// Set only for a one-time MFA login grant or a live authenticated session.
    mfa_authorized: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AccessClaims {
    /// Issuer is the refresh-token/session ID, matching Home Assistant.
    iss: String,
    /// Subject is the user ID.
    sub: String,
    iat: u64,
    exp: u64,
    /// Signed-path capabilities are not bearer access tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    purpose: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SignedPathClaims {
    /// Issuer is the refresh-token/session ID.
    iss: String,
    /// Subject is the user ID.
    sub: String,
    /// Request path covered by this capability.
    path: String,
    /// Ordered non-safe query pairs; mirrors the pinned HTTP auth middleware.
    params: Vec<(String, String)>,
    purpose: String,
    iat: u64,
    exp: u64,
}

impl AuthManager {
    /// Load persistent auth state or initialize the local auth provider.
    pub fn new(runtime: Runtime) -> Result<Self> {
        let state = match runtime.get_data(AUTH_DATA_KEY)? {
            Some(value) => {
                let state: AuthStore = serde_json::from_value(value).map_err(|_| {
                    Error::Storage("stored authentication metadata is invalid".to_owned())
                })?;
                validate_store(&state)?;
                state
            }
            None => {
                let state = AuthStore {
                    version: AUTH_SCHEMA_VERSION,
                    jwt_secret: random_secret(),
                    users: BTreeMap::new(),
                    homeassistant_provider: HomeAssistantProviderData {
                        users: BTreeMap::new(),
                    },
                    sessions: BTreeMap::new(),
                    mfa: MfaStore::default(),
                };
                persist(&runtime, &state)?;
                state
            }
        };
        Ok(Self {
            inner: Arc::new(AuthInner {
                runtime,
                state: Mutex::new(state),
            }),
        })
    }

    /// Return whether the local provider has at least one user.
    pub fn has_users(&self) -> Result<bool> {
        Ok(!self.lock_state()?.users.is_empty())
    }

    /// Return all public users ordered by ID.
    pub fn users(&self) -> Result<Vec<User>> {
        let state = self.lock_state()?;
        Ok(state.users.values().map(public_user).collect())
    }

    /// Look up a public user by stable ID.
    pub fn get_user(&self, user_id: &str) -> Result<User> {
        let state = self.lock_state()?;
        state
            .users
            .get(user_id)
            .map(public_user)
            .ok_or_else(|| Error::NotFound("user not found".to_owned()))
    }

    /// Create the first owner using the display name as a normalized username.
    pub fn bootstrap_owner(&self, name: &str, password: &str) -> Result<User> {
        let username = normalize_username(name);
        self.bootstrap_owner_with_username(&username, name, password)
    }

    /// Create the first owner with a distinct provider username and display name.
    pub fn bootstrap_owner_with_username(
        &self,
        username: &str,
        name: &str,
        password: &str,
    ) -> Result<User> {
        validate_new_username(username)?;
        validate_display_name(name)?;
        validate_password(password)?;
        let password_hash = hash_password(password)?;
        self.transaction(|state| {
            if !state.users.is_empty() {
                return Err(Error::InvalidInput(
                    "owner has already been created".to_owned(),
                ));
            }
            let normalized = normalize_username(username);
            let user = StoredUser {
                id: new_id(),
                username: Some(username.to_owned()),
                name: name.trim().to_owned(),
                is_owner: true,
                is_active: true,
                local_only: None,
                system_generated: false,
                group_ids: vec![GROUP_ID_USER.to_owned()],
                mfa_enabled: false,
            };
            state.homeassistant_provider.users.insert(
                normalized,
                ProviderUser {
                    username: username.to_owned(),
                    password_hash,
                },
            );
            state.users.insert(user.id.clone(), user.clone());
            Ok(public_user(&user))
        })
    }

    /// Create a regular user, using their display name as their username.
    pub fn create_user(
        &self,
        caller_user_id: &str,
        name: &str,
        password: &str,
        is_admin: bool,
    ) -> Result<User> {
        let username = normalize_username(name);
        self.create_user_with_username(caller_user_id, &username, name, password, is_admin)
    }

    /// Create a user with a provider username separate from their display name.
    ///
    /// Active administrators can create regular users. Only the owner may grant
    /// the administrator role.
    pub fn create_user_with_username(
        &self,
        caller_user_id: &str,
        username: &str,
        name: &str,
        password: &str,
        is_admin: bool,
    ) -> Result<User> {
        validate_new_username(username)?;
        validate_display_name(name)?;
        validate_password(password)?;
        let password_hash = hash_password(password)?;
        self.transaction(|state| {
            let caller = require_active_user(state, caller_user_id)?;
            if !user_is_admin(caller) {
                return Err(Error::InvalidInput(
                    "administrator privileges are required".to_owned(),
                ));
            }
            if is_admin && !caller.is_owner {
                return Err(Error::InvalidInput(
                    "only the owner can grant administrator privileges".to_owned(),
                ));
            }
            let normalized = normalize_username(username);
            if state.homeassistant_provider.users.contains_key(&normalized) {
                return Err(Error::InvalidInput("username already exists".to_owned()));
            }
            let mut group_ids = vec![GROUP_ID_USER.to_owned()];
            if is_admin {
                group_ids.push(GROUP_ID_ADMIN.to_owned());
            }
            let user = StoredUser {
                id: new_id(),
                username: Some(username.to_owned()),
                name: name.trim().to_owned(),
                is_owner: false,
                is_active: true,
                local_only: None,
                system_generated: false,
                group_ids,
                mfa_enabled: false,
            };
            state.homeassistant_provider.users.insert(
                normalized,
                ProviderUser {
                    username: username.to_owned(),
                    password_hash,
                },
            );
            state.users.insert(user.id.clone(), user.clone());
            Ok(public_user(&user))
        })
    }

    /// Create a Home Assistant user record without local credentials.
    ///
    /// This matches `config/auth/create`; upstream can add credentials from
    /// other providers later, while this crate only authenticates local users.
    pub fn create_user_without_credentials(
        &self,
        caller_user_id: &str,
        name: &str,
        group_ids: Option<&[String]>,
        local_only: Option<bool>,
    ) -> Result<User> {
        validate_display_name(name)?;
        let group_ids = validate_group_ids(group_ids.unwrap_or_default())?;
        self.transaction(|state| {
            let caller = require_active_user(state, caller_user_id)?;
            if !user_is_admin(caller) {
                return Err(Error::InvalidInput(
                    "administrator privileges are required".to_owned(),
                ));
            }
            if group_ids.iter().any(|group| group == GROUP_ID_ADMIN) && !caller.is_owner {
                return Err(Error::InvalidInput(
                    "only the owner can grant administrator privileges".to_owned(),
                ));
            }
            let user = StoredUser {
                id: new_id(),
                username: None,
                name: name.trim().to_owned(),
                is_owner: false,
                is_active: true,
                local_only,
                system_generated: false,
                group_ids: group_ids.clone(),
                mfa_enabled: false,
            };
            state.users.insert(user.id.clone(), user.clone());
            Ok(public_user(&user))
        })
    }

    /// Update supported public account fields, following `config/auth/update`.
    pub fn update_user(
        &self,
        caller_user_id: &str,
        user_id: &str,
        name: Option<&str>,
        is_active: Option<bool>,
        group_ids: Option<&[String]>,
        local_only: Option<bool>,
    ) -> Result<User> {
        if let Some(name) = name {
            validate_display_name(name)?;
        }
        let group_ids = group_ids.map(validate_group_ids).transpose()?;
        self.transaction(|state| {
            let (caller_is_admin, caller_is_owner) = {
                let caller = require_active_user(state, caller_user_id)?;
                (user_is_admin(caller), caller.is_owner)
            };
            if !caller_is_admin {
                return Err(Error::InvalidInput(
                    "administrator privileges are required".to_owned(),
                ));
            }
            let target = state
                .users
                .get_mut(user_id)
                .ok_or_else(|| Error::NotFound("user not found".to_owned()))?;
            if target.system_generated {
                return Err(Error::InvalidInput(
                    "system-generated users cannot be modified".to_owned(),
                ));
            }
            if target.is_owner && is_active == Some(false) {
                return Err(Error::InvalidInput(
                    "the owner account cannot be disabled".to_owned(),
                ));
            }
            if group_ids
                .as_ref()
                .is_some_and(|groups| groups.iter().any(|group| group == GROUP_ID_ADMIN))
                && !user_is_admin(target)
                && !caller_is_owner
            {
                return Err(Error::InvalidInput(
                    "only the owner can grant administrator privileges".to_owned(),
                ));
            }
            if let Some(name) = name {
                target.name = name.trim().to_owned();
            }
            if let Some(is_active) = is_active {
                target.is_active = is_active;
            }
            if let Some(group_ids) = group_ids {
                target.group_ids = group_ids;
            }
            if let Some(local_only) = local_only {
                target.local_only = Some(local_only);
            }
            let user = public_user(target);
            if !user.is_active {
                state
                    .sessions
                    .retain(|_, session| session.user_id != user_id);
                clear_pending_mfa_logins(state, user_id);
            }
            Ok(user)
        })
    }

    /// Remove a non-owner account and revoke all its sessions.
    pub fn delete_user(&self, caller_user_id: &str, user_id: &str) -> Result<()> {
        self.transaction(|state| {
            let caller = require_active_user(state, caller_user_id)?;
            if !user_is_admin(caller) {
                return Err(Error::InvalidInput(
                    "administrator privileges are required".to_owned(),
                ));
            }
            if caller_user_id == user_id {
                return Err(Error::InvalidInput(
                    "unable to delete your own account".to_owned(),
                ));
            }
            let user = state
                .users
                .get(user_id)
                .ok_or_else(|| Error::NotFound("user not found".to_owned()))?;
            if user.is_owner {
                return Err(Error::InvalidInput("unable to delete the owner".to_owned()));
            }
            if user.system_generated {
                return Err(Error::InvalidInput(
                    "system-generated users cannot be deleted".to_owned(),
                ));
            }
            let username = user.username.clone();
            state.users.remove(user_id);
            if let Some(username) = username {
                state
                    .homeassistant_provider
                    .users
                    .remove(&normalize_username(&username));
            }
            state.mfa.totp_users.remove(user_id);
            state.mfa.pending_enrollments.remove(user_id);
            state.mfa.retry_budgets.remove(user_id);
            clear_pending_mfa_logins(state, user_id);
            state
                .sessions
                .retain(|_, session| session.user_id != user_id);
            Ok(())
        })
    }

    /// Enable or disable a user. Disabling a user revokes all of their sessions.
    pub fn set_user_active(
        &self,
        caller_user_id: &str,
        user_id: &str,
        active: bool,
    ) -> Result<User> {
        self.transaction(|state| {
            let caller = require_active_user(state, caller_user_id)?;
            if !user_is_admin(caller) {
                return Err(Error::InvalidInput(
                    "administrator privileges are required".to_owned(),
                ));
            }
            let user = state
                .users
                .get_mut(user_id)
                .ok_or_else(|| Error::NotFound("user not found".to_owned()))?;
            if user.is_owner && !active {
                return Err(Error::InvalidInput(
                    "the owner account cannot be disabled".to_owned(),
                ));
            }
            user.is_active = active;
            let result = public_user(user);
            if !active {
                state
                    .sessions
                    .retain(|_, session| session.user_id != user_id);
                clear_pending_mfa_logins(state, user_id);
            }
            Ok(result)
        })
    }

    /// Enroll a local username and Argon2id password for an existing account.
    ///
    /// The account holder may enroll credentials for themselves; an active
    /// administrator may enroll them for another non-owner account.
    pub fn enroll_local_credentials(
        &self,
        caller_user_id: &str,
        user_id: &str,
        username: &str,
        password: &str,
    ) -> Result<User> {
        validate_new_username(username)?;
        validate_password(password)?;
        let password_hash = hash_password(password)?;
        self.transaction(|state| {
            let caller = require_active_user(state, caller_user_id)?;
            let caller_is_admin = user_is_admin(caller);
            let target = state
                .users
                .get(user_id)
                .ok_or_else(|| Error::NotFound("user not found".to_owned()))?;
            if caller_user_id != user_id && !caller_is_admin {
                return Err(Error::InvalidInput(
                    "administrator privileges are required".to_owned(),
                ));
            }
            if target.system_generated {
                return Err(Error::InvalidInput(
                    "system-generated users cannot enroll local credentials".to_owned(),
                ));
            }
            if target.is_owner && caller_user_id != user_id && !caller.is_owner {
                return Err(Error::InvalidInput(
                    "only the owner can modify owner credentials".to_owned(),
                ));
            }
            if target.username.is_some() {
                return Err(Error::InvalidInput(
                    "user already has local credentials".to_owned(),
                ));
            }
            let normalized = normalize_username(username);
            if state.homeassistant_provider.users.contains_key(&normalized) {
                return Err(Error::InvalidInput("username already exists".to_owned()));
            }
            let target = state.users.get_mut(user_id).expect("user checked above");
            target.username = Some(username.to_owned());
            let user = public_user(target);
            state.homeassistant_provider.users.insert(
                normalized,
                ProviderUser {
                    username: username.to_owned(),
                    password_hash,
                },
            );
            clear_pending_mfa_logins(state, user_id);
            Ok(user)
        })
    }

    /// Update local provider data as an administrator, preserving public user
    /// records and rehashing any replacement password before persistence.
    pub fn update_local_credentials(
        &self,
        caller_user_id: &str,
        user_id: &str,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<User> {
        if username.is_none() && password.is_none() {
            return Err(Error::InvalidInput(
                "a new username or password is required".to_owned(),
            ));
        }
        if let Some(username) = username {
            validate_new_username(username)?;
        }
        if let Some(password) = password {
            validate_password(password)?;
        }
        let password_hash = password.map(hash_password).transpose()?;
        self.transaction(|state| {
            let caller = require_active_user(state, caller_user_id)?;
            if !user_is_admin(caller) {
                return Err(Error::InvalidInput(
                    "administrator privileges are required".to_owned(),
                ));
            }
            let target = state
                .users
                .get(user_id)
                .ok_or_else(|| Error::NotFound("user not found".to_owned()))?;
            if target.system_generated {
                return Err(Error::InvalidInput(
                    "system-generated users cannot be modified".to_owned(),
                ));
            }
            if target.is_owner && !caller.is_owner {
                return Err(Error::InvalidInput(
                    "only the owner can modify owner credentials".to_owned(),
                ));
            }
            let old_username = target.username.clone().ok_or_else(|| {
                Error::InvalidInput("user has no local credentials to update".to_owned())
            })?;
            let old_normalized = normalize_username(&old_username);
            let mut credential = state
                .homeassistant_provider
                .users
                .remove(&old_normalized)
                .ok_or_else(|| Error::Storage("local provider record is missing".to_owned()))?;
            let new_username = username.unwrap_or(&old_username);
            let new_normalized = normalize_username(new_username);
            if new_normalized != old_normalized
                && state
                    .homeassistant_provider
                    .users
                    .contains_key(&new_normalized)
            {
                state
                    .homeassistant_provider
                    .users
                    .insert(old_normalized, credential);
                return Err(Error::InvalidInput("username already exists".to_owned()));
            }
            credential.username = new_username.to_owned();
            if let Some(password_hash) = &password_hash {
                credential.password_hash = password_hash.clone();
            }
            state
                .homeassistant_provider
                .users
                .insert(new_normalized, credential);
            let target = state.users.get_mut(user_id).expect("user checked above");
            target.username = Some(new_username.to_owned());
            let user = public_user(target);
            clear_pending_mfa_logins(state, user_id);
            Ok(user)
        })
    }

    /// Change the caller's own local password after verifying the current one.
    pub fn change_password(
        &self,
        user_id: &str,
        current_password: &str,
        new_password: &str,
    ) -> Result<()> {
        validate_password(current_password)?;
        validate_password(new_password)?;
        let password_hash = hash_password(new_password)?;
        self.transaction(|state| {
            let user = require_active_user(state, user_id)?;
            let username = user.username.as_deref().ok_or_else(invalid_auth)?;
            let credential = state
                .homeassistant_provider
                .users
                .get(&normalize_username(username))
                .ok_or_else(|| Error::Storage("local provider record is missing".to_owned()))?;
            if !verify_password(current_password, &credential.password_hash)? {
                return Err(invalid_auth());
            }
            let credential = state
                .homeassistant_provider
                .users
                .get_mut(&normalize_username(username))
                .expect("provider record checked above");
            credential.password_hash = password_hash;
            clear_pending_mfa_logins(state, user_id);
            Ok(())
        })
    }

    /// Start a password-authenticated login and request TOTP when enabled.
    ///
    /// This method never creates a token. The returned challenge handle should
    /// remain in the server-side HTTP flow; `issue_tokens_after_mfa` is the only
    /// token exchange for a user with MFA enabled.
    pub fn begin_mfa_login(&self, username: &str, password: &str) -> Result<LoginAuthResult> {
        let user = self.verify_credentials(username, password)?;
        if !user.mfa_modules.iter().any(|module| module.enabled) {
            return Ok(LoginAuthResult::Authenticated { user });
        }
        let now = now_epoch_seconds()?;
        let challenge_id = format!("ha_mfa_{}", random_secret());
        let digest = digest_secret(&challenge_id);
        self.transaction(|state| {
            let target = require_active_user(state, &user.id)?;
            if !target.mfa_enabled || !state.mfa.totp_users.contains_key(&user.id) {
                return Err(invalid_auth());
            }
            prune_expired_mfa_state(&mut state.mfa, now);
            if state
                .mfa
                .retry_budgets
                .get(&user.id)
                .is_some_and(|budget| budget.attempts >= MFA_MAX_RETRY_TIME)
                || state
                    .mfa
                    .login_challenges
                    .values()
                    .filter(|challenge| challenge.user_id == user.id)
                    .count()
                    >= 8
            {
                return Err(invalid_auth());
            }
            state.mfa.login_challenges.insert(
                digest,
                StoredMfaLoginChallenge {
                    user_id: user.id.clone(),
                    created_at: now,
                    expires_at: now.saturating_add(MFA_LOGIN_CHALLENGE_TTL_SECS),
                    attempts: 0,
                },
            );
            Ok(LoginAuthResult::MfaRequired {
                user: user.clone(),
                challenge_id,
            })
        })
    }

    /// Complete a password-authenticated TOTP challenge and return a one-use
    /// server-side grant for the later authorization-code exchange.
    pub fn complete_mfa_login(&self, challenge_id: &str, code: &str) -> Result<MfaLoginGrant> {
        let now = now_epoch_seconds()?;
        let challenge_digest = digest_secret(challenge_id);
        let grant = format!("ha_mfa_grant_{}", random_secret());
        let grant_digest = digest_secret(&grant);
        let accepted = self.transaction(|state| {
            prune_expired_mfa_state(&mut state.mfa, now);
            let Some(challenge) = state.mfa.login_challenges.get(&challenge_digest).cloned() else {
                return Ok(None);
            };
            let challenge_user_id = challenge.user_id.clone();
            if state
                .mfa
                .retry_budgets
                .get(&challenge_user_id)
                .is_some_and(|budget| budget.attempts >= MFA_MAX_RETRY_TIME)
            {
                state.mfa.login_challenges.remove(&challenge_digest);
                return Ok(None);
            }
            if challenge.expires_at <= now
                || challenge.created_at > now
                || challenge.attempts >= MFA_MAX_RETRY_TIME
            {
                state.mfa.login_challenges.remove(&challenge_digest);
                return Ok(None);
            }
            if !state
                .users
                .get(&challenge.user_id)
                .is_some_and(|user| user.is_active && user.mfa_enabled)
            {
                state.mfa.login_challenges.remove(&challenge_digest);
                return Ok(None);
            }
            let Some(credential) = state.mfa.totp_users.get_mut(&challenge.user_id) else {
                state.mfa.login_challenges.remove(&challenge_digest);
                return Ok(None);
            };
            let Some(step) = totp_matching_step(&credential.secret, code, now)? else {
                let challenge = state
                    .mfa
                    .login_challenges
                    .get_mut(&challenge_digest)
                    .expect("challenge checked above");
                challenge.attempts = challenge.attempts.saturating_add(1);
                if challenge.attempts >= MFA_MAX_RETRY_TIME {
                    state.mfa.login_challenges.remove(&challenge_digest);
                }
                record_mfa_failure(&mut state.mfa, &challenge_user_id, now);
                return Ok(None);
            };
            if credential.last_used_step.is_some_and(|used| step <= used) {
                let challenge = state
                    .mfa
                    .login_challenges
                    .get_mut(&challenge_digest)
                    .expect("challenge checked above");
                challenge.attempts = challenge.attempts.saturating_add(1);
                if challenge.attempts >= MFA_MAX_RETRY_TIME {
                    state.mfa.login_challenges.remove(&challenge_digest);
                }
                record_mfa_failure(&mut state.mfa, &challenge_user_id, now);
                return Ok(None);
            }
            credential.last_used_step = Some(step);
            state.mfa.retry_budgets.remove(&challenge_user_id);
            state.mfa.login_challenges.remove(&challenge_digest);
            state.mfa.login_grants.insert(
                grant_digest,
                StoredMfaLoginGrant {
                    user_id: challenge.user_id,
                    expires_at: now.saturating_add(MFA_LOGIN_GRANT_TTL_SECS),
                },
            );
            Ok(Some(()))
        })?;
        if accepted.is_some() {
            Ok(MfaLoginGrant(grant))
        } else {
            Err(invalid_auth())
        }
    }

    /// Exchange a valid one-use MFA grant for the bearer token pair.
    pub fn issue_tokens_after_mfa(
        &self,
        grant: MfaLoginGrant,
        client_id: &str,
        client_name: Option<&str>,
    ) -> Result<TokenSet> {
        validate_client_id(client_id)?;
        validate_optional_client_name(client_name)?;
        let now = now_epoch_seconds()?;
        let grant_digest = digest_secret(&grant.0);
        self.transaction(|state| {
            let grant = state
                .mfa
                .login_grants
                .remove(&grant_digest)
                .filter(|grant| grant.expires_at > now)
                .ok_or_else(invalid_auth)?;
            if !state
                .users
                .get(&grant.user_id)
                .is_some_and(|user| user.is_active && user.mfa_enabled)
                || !state.mfa.totp_users.contains_key(&grant.user_id)
            {
                return Err(invalid_auth());
            }
            create_session_in_state(
                state,
                &grant.user_id,
                SessionOptions {
                    client_id: Some(client_id.to_owned()),
                    client_name: client_name.map(str::to_owned),
                    client_icon: None,
                    token_type: TOKEN_TYPE_NORMAL,
                    access_token_expiration_secs: ACCESS_TOKEN_EXPIRATION_SECS,
                    refresh_lifetime_secs: Some(REFRESH_TOKEN_EXPIRATION_SECS),
                    unique_long_lived_client_name: false,
                    mfa_authorized: true,
                },
                now,
            )
        })
    }

    /// Create a new TOTP enrollment secret and provisioning URI.
    ///
    /// The returned seed is shown only to the authenticated enrollment flow;
    /// the persistent store does not include it in public user responses.
    pub fn begin_mfa_enrollment(&self, user_id: &str) -> Result<MfaEnrollment> {
        let now = now_epoch_seconds()?;
        let secret = TotpSecret::generate().to_base32();
        self.transaction(|state| {
            let user = require_active_user(state, user_id)?;
            if user.system_generated {
                return Err(Error::InvalidInput(
                    "system-generated users cannot enable MFA".to_owned(),
                ));
            }
            if user.mfa_enabled || state.mfa.totp_users.contains_key(user_id) {
                return Err(Error::InvalidInput("TOTP is already enabled".to_owned()));
            }
            let user_name = user
                .username
                .as_deref()
                .unwrap_or(&user.name)
                .replace(':', "_");
            let totp = build_totp(&secret, &user_name)?;
            let uri = totp
                .to_url()
                .map_err(|_| Error::Storage("failed to create TOTP URI".to_owned()))?;
            state.mfa.pending_enrollments.insert(
                user_id.to_owned(),
                StoredTotpEnrollment {
                    secret: secret.clone(),
                    expires_at: now.saturating_add(MFA_ENROLLMENT_TTL_SECS),
                },
            );
            Ok(MfaEnrollment {
                secret: secret.clone(),
                otpauth_uri: uri,
            })
        })
    }

    /// Confirm a pending TOTP enrollment with a code from the new secret.
    pub fn confirm_mfa_enrollment(&self, user_id: &str, code: &str) -> Result<User> {
        validate_totp_code_format(code)?;
        let now = now_epoch_seconds()?;
        self.transaction(|state| {
            let user = require_active_user(state, user_id)?;
            if user.system_generated {
                return Err(Error::InvalidInput(
                    "system-generated users cannot enable MFA".to_owned(),
                ));
            }
            let enrollment = state
                .mfa
                .pending_enrollments
                .get(user_id)
                .cloned()
                .filter(|enrollment| enrollment.expires_at > now)
                .ok_or_else(|| Error::InvalidInput("TOTP enrollment expired".to_owned()))?;
            let step = totp_matching_step(&enrollment.secret, code, now)?
                .ok_or_else(|| Error::InvalidInput("invalid TOTP code".to_owned()))?;
            state.mfa.totp_users.insert(
                user_id.to_owned(),
                StoredTotpCredential {
                    secret: enrollment.secret,
                    last_used_step: Some(step),
                },
            );
            state.mfa.pending_enrollments.remove(user_id);
            state.mfa.retry_budgets.remove(user_id);
            let user = state.users.get_mut(user_id).expect("user checked above");
            user.mfa_enabled = true;
            Ok(public_user(user))
        })
    }

    /// Disable TOTP after verifying a fresh code for the active seed.
    pub fn disable_mfa(&self, user_id: &str, code: &str) -> Result<User> {
        validate_totp_code_format(code)?;
        let now = now_epoch_seconds()?;
        self.transaction(|state| {
            let user = require_active_user(state, user_id)?;
            if user.system_generated {
                return Err(Error::InvalidInput(
                    "system-generated users cannot disable MFA".to_owned(),
                ));
            }
            let credential = state
                .mfa
                .totp_users
                .get(user_id)
                .cloned()
                .ok_or_else(|| Error::InvalidInput("TOTP is not enabled".to_owned()))?;
            let step = totp_matching_step(&credential.secret, code, now)?
                .filter(|step| credential.last_used_step.is_none_or(|used| *step > used))
                .ok_or_else(|| Error::InvalidInput("invalid or reused TOTP code".to_owned()))?;
            let _ = step;
            state.mfa.totp_users.remove(user_id);
            state.mfa.pending_enrollments.remove(user_id);
            clear_pending_mfa_logins(state, user_id);
            state.mfa.retry_budgets.remove(user_id);
            let user = state.users.get_mut(user_id).expect("user checked above");
            user.mfa_enabled = false;
            Ok(public_user(user))
        })
    }

    /// Verify local credentials without creating a session.
    ///
    /// Usernames are trimmed and case-folded like the pinned Home Assistant
    /// provider. Unknown usernames perform a dummy Argon2 verification, and
    /// disabled accounts fail with the same public error as bad credentials.
    pub fn verify_credentials(&self, username: &str, password: &str) -> Result<User> {
        validate_password(password)?;
        let normalized = normalize_username(username);
        let candidate = {
            let state = self.lock_state()?;
            let provider_user = state.homeassistant_provider.users.get(&normalized).cloned();
            let stored_user = provider_user.as_ref().and_then(|_| {
                state
                    .users
                    .values()
                    .find(|user| {
                        user.username
                            .as_deref()
                            .is_some_and(|username| normalize_username(username) == normalized)
                    })
                    .cloned()
            });
            (provider_user, stored_user)
        };

        match candidate {
            (Some(provider_user), Some(user)) => {
                if verify_password(password, &provider_user.password_hash)? && user.is_active {
                    Ok(public_user(&user))
                } else {
                    Err(invalid_auth())
                }
            }
            _ => {
                let dummy_hash = dummy_password_hash()?;
                let _ = verify_password(password, &dummy_hash)?;
                Err(invalid_auth())
            }
        }
    }

    /// Convenience login API that verifies credentials and creates a bearer pair.
    ///
    /// HTTP auth-code flows should call [`Self::verify_credentials`] first and
    /// issue a token pair only after redeeming their one-time authorization code.
    pub fn authenticate(&self, username: &str, password: &str) -> Result<TokenSet> {
        let user = self.verify_credentials(username, password)?;
        if user.mfa_modules.iter().any(|module| module.enabled) {
            return Err(invalid_auth());
        }
        self.issue_tokens(
            &user.id,
            "urn:home-assistant:native",
            Some("Native Home Assistant"),
        )
    }

    /// Issue a new session with a 30-minute JWT and a 90-day refresh token.
    pub fn issue_tokens(
        &self,
        user_id: &str,
        client_id: &str,
        client_name: Option<&str>,
    ) -> Result<TokenSet> {
        validate_client_id(client_id)?;
        validate_optional_client_name(client_name)?;
        self.create_session(
            user_id,
            SessionOptions {
                client_id: Some(client_id.to_owned()),
                client_name: client_name.map(str::to_owned),
                client_icon: None,
                token_type: TOKEN_TYPE_NORMAL,
                access_token_expiration_secs: ACCESS_TOKEN_EXPIRATION_SECS,
                refresh_lifetime_secs: Some(REFRESH_TOKEN_EXPIRATION_SECS),
                unique_long_lived_client_name: false,
                mfa_authorized: false,
            },
        )
    }

    /// Rotate a refresh token for the OAuth client that originally received it.
    ///
    /// A successful refresh persists a new token digest before returning the new
    /// opaque token. Replaying the old token then fails. The access token remains
    /// valid until its own expiry unless the session is revoked or the user is
    /// disabled.
    pub fn refresh_tokens_for_client(
        &self,
        refresh_token: &str,
        client_id: &str,
    ) -> Result<TokenSet> {
        validate_client_id(client_id)?;
        let old_digest = digest_refresh_token(refresh_token);
        let now = now_epoch_seconds()?;
        let mut state = self.lock_state()?;
        let mut candidate = state.clone();
        let session = candidate
            .sessions
            .values_mut()
            .find(|session| constant_time_eq_hex(&session.refresh_token_hash, &old_digest))
            .ok_or_else(invalid_auth)?;
        if session.client_id.as_deref() != Some(client_id)
            || session
                .refresh_expires_at
                .is_some_and(|expires| expires <= now)
        {
            return Err(invalid_auth());
        }
        let user = candidate
            .users
            .get(&session.user_id)
            .filter(|user| user.is_active)
            .ok_or_else(invalid_auth)?;
        let user_id = user.id.clone();
        let session_id = session.id.clone();
        let lifetime = session.access_token_expiration_secs;
        let refresh_token = format!("ha_{}", random_secret());
        session.refresh_token_hash = digest_refresh_token(&refresh_token);
        session.last_used_at = Some(now);
        let access_token =
            create_access_token(&candidate.jwt_secret, &session_id, &user_id, lifetime, now)?;
        persist(&self.inner.runtime, &candidate)?;
        *state = candidate;
        Ok(TokenSet {
            access_token,
            token_type: "Bearer".to_owned(),
            expires_in: lifetime,
            refresh_token,
        })
    }

    /// Revoke the session associated with an opaque refresh token.
    ///
    /// The provided token is SHA-256 hashed and compared in constant time; the
    /// raw token is never written to runtime metadata.
    pub fn revoke_refresh_token(&self, refresh_token: &str) -> Result<bool> {
        let digest = digest_refresh_token(refresh_token);
        self.transaction(|state| {
            let session_id = state
                .sessions
                .values()
                .find(|session| constant_time_eq_hex(&session.refresh_token_hash, &digest))
                .map(|session| session.id.clone());
            match session_id {
                Some(session_id) => Ok(state.sessions.remove(&session_id).is_some()),
                None => Ok(false),
            }
        })
    }

    /// Revoke every session for a user. Active administrators may revoke their
    /// own sessions; only the owner may revoke another administrator's sessions.
    pub fn revoke_user_sessions(&self, caller_user_id: &str, user_id: &str) -> Result<usize> {
        self.transaction(|state| {
            let caller = require_active_user(state, caller_user_id)?;
            if !user_is_admin(caller) {
                return Err(Error::InvalidInput(
                    "administrator privileges are required".to_owned(),
                ));
            }
            let target = state
                .users
                .get(user_id)
                .ok_or_else(|| Error::NotFound("user not found".to_owned()))?;
            if user_is_admin(target) && caller_user_id != user_id && !caller.is_owner {
                return Err(Error::InvalidInput(
                    "only the owner can revoke another administrator's sessions".to_owned(),
                ));
            }
            let before = state.sessions.len();
            state
                .sessions
                .retain(|_, session| session.user_id != user_id);
            Ok(before - state.sessions.len())
        })
    }

    /// Validate JWT signature, expiry, session revocation, and current user status.
    pub fn validate_access_token(&self, token: &str) -> Result<User> {
        Ok(self.validate_access_token_identity(token)?.user)
    }

    /// Validate an access token and return the exact active session that issued it.
    pub fn validate_access_token_identity(&self, token: &str) -> Result<AccessIdentity> {
        let now = now_epoch_seconds()?;
        let state = self.lock_state()?;
        let claims = decode_access_claims(&state.jwt_secret, token)?;
        active_identity(&state, &claims.iss, &claims.sub, now)
    }

    /// Validate an HTTP signed-path capability and return its active identity.
    /// `path` is the original raw path and query string without `authSig`.
    /// Query pairs are decoded and ordered; `width` and `height` are ignored as
    /// in Home Assistant's signed-request validator.
    pub fn validate_signed_request(&self, path: &str, auth_sig: &str) -> Result<AccessIdentity> {
        let (request_path, request_params) = signed_path_parts(path)?;
        let now = now_epoch_seconds()?;
        let state = self.lock_state()?;
        let claims = decode::<SignedPathClaims>(
            auth_sig,
            &DecodingKey::from_secret(state.jwt_secret.as_bytes()),
            &jwt_validation(),
        )
        .map_err(|_| invalid_auth())?
        .claims;
        let now_timestamp = u64::try_from(now).map_err(|_| invalid_auth())?;
        if claims.purpose != "signed_path"
            || claims.path != request_path
            || claims.params != request_params
            || claims.iat > now_timestamp
            || claims.exp <= claims.iat
            || claims.exp - claims.iat > MAX_SIGNED_PATH_EXPIRATION_SECS
        {
            return Err(invalid_auth());
        }
        active_identity(&state, &claims.iss, &claims.sub, now)
    }

    /// Sign a short-lived path capability bound to the current session.
    ///
    /// `path` must be an absolute local path and must not already contain an
    /// `authSig` parameter. The returned string appends the signed capability
    /// using Home Assistant's `authSig` query name.
    pub fn create_signed_path(
        &self,
        user_id: &str,
        current_session: &str,
        path: &str,
        expires_seconds: u64,
    ) -> Result<String> {
        let (request_path, params) = signed_path_parts(path)?;
        if expires_seconds == 0 || expires_seconds > MAX_SIGNED_PATH_EXPIRATION_SECS {
            return Err(Error::InvalidInput(format!(
                "signed path expiry must be between 1 and {MAX_SIGNED_PATH_EXPIRATION_SECS} seconds"
            )));
        }
        let now = now_epoch_seconds()?;
        let state = self.lock_state()?;
        let identity = active_identity(&state, current_session, user_id, now)?;
        let _ = identity;
        let issued_at = u64::try_from(now)
            .map_err(|_| Error::Storage("system clock is before the Unix epoch".to_owned()))?;
        let expires_at = issued_at.saturating_add(expires_seconds);
        let claims = SignedPathClaims {
            iss: current_session.to_owned(),
            sub: user_id.to_owned(),
            path: request_path,
            params,
            purpose: "signed_path".to_owned(),
            iat: issued_at,
            exp: expires_at,
        };
        let signature = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .map_err(|_| Error::Storage("failed to sign path capability".to_owned()))?;
        let separator = if path.contains('?') { '&' } else { '?' };
        Ok(format!("{path}{separator}authSig={signature}"))
    }

    fn validate_current_session(&self, user_id: &str, session_id: &str) -> Result<()> {
        let now = now_epoch_seconds()?;
        let state = self.lock_state()?;
        active_identity(&state, session_id, user_id, now).map(|_| ())
    }

    /// Handle supported Home Assistant `auth/*` WebSocket commands.
    ///
    /// Returns only the command's result payload. The WebSocket adapter owns the
    /// outer `id` / `type` / `success` result envelope.
    pub fn handle_ws(&self, command: &Value, caller_user_id: &str) -> Result<Value> {
        self.handle_ws_session(command, caller_user_id, None)
    }

    /// Handle auth WebSocket commands with the connection's active session ID.
    ///
    /// This lets session metadata accurately identify the caller's refresh
    /// token and supports deleting every other session while keeping this one.
    pub fn handle_ws_session(
        &self,
        command: &Value,
        caller_user_id: &str,
        current_session: Option<&str>,
    ) -> Result<Value> {
        let command_type = command
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::InvalidInput("auth command type is required".to_owned()))?;
        self.require_active(caller_user_id)?;
        if let Some(current_session) = current_session {
            self.validate_current_session(caller_user_id, current_session)?;
        }
        match command_type {
            "config/auth/list" => {
                ensure_command_fields(command, &[])?;
                self.require_admin(caller_user_id)?;
                Ok(Value::Array(
                    self.users()?.iter().map(config_user_payload).collect(),
                ))
            }
            "config/auth/create" => {
                ensure_command_fields(command, &["name", "group_ids", "local_only"])?;
                let name = required_string(command, "name")?;
                let group_ids = optional_string_array(command, "group_ids")?;
                let local_only = optional_bool(command, "local_only")?;
                let user = self.create_user_without_credentials(
                    caller_user_id,
                    name,
                    group_ids.as_deref(),
                    local_only,
                )?;
                Ok(json!({"user": config_user_payload(&user)}))
            }
            "config/auth/update" => {
                ensure_command_fields(
                    command,
                    &["user_id", "name", "is_active", "group_ids", "local_only"],
                )?;
                let user_id = required_string(command, "user_id")?;
                let name = optional_string(command, "name")?;
                let is_active = optional_bool(command, "is_active")?;
                let group_ids = optional_string_array(command, "group_ids")?;
                let local_only = optional_bool(command, "local_only")?;
                let user = self.update_user(
                    caller_user_id,
                    user_id,
                    name,
                    is_active,
                    group_ids.as_deref(),
                    local_only,
                )?;
                Ok(json!({"user": config_user_payload(&user)}))
            }
            "config/auth/delete" => {
                ensure_command_fields(command, &["user_id"])?;
                let user_id = required_string(command, "user_id")?;
                self.delete_user(caller_user_id, user_id)?;
                Ok(Value::Null)
            }
            "auth/current_user" => {
                ensure_command_fields(command, &[])?;
                let user = self.get_user(caller_user_id)?;
                Ok(current_user_payload(&user))
            }
            "auth/mfa_modules" => {
                ensure_command_fields(command, &[])?;
                let user = self.get_user(caller_user_id)?;
                serde_json::to_value(user.mfa_modules)
                    .map_err(|_| Error::Storage("failed to serialize MFA modules".to_owned()))
            }
            "auth/depose_mfa" => {
                ensure_command_fields(command, &["mfa_module_id"])?;
                if required_string(command, "mfa_module_id")? != MFA_TOTP_ID {
                    return Err(Error::NotFound("MFA module is not found".into()));
                }
                let session_id = current_session.ok_or_else(invalid_auth)?;
                let now = now_epoch_seconds()?;
                self.transaction(|state| {
                    active_identity(state, session_id, caller_user_id, now)?;
                    let user = require_active_user(state, caller_user_id)?;
                    if user.system_generated {
                        return Err(Error::InvalidInput("System users cannot change MFA".into()));
                    }
                    state.mfa.totp_users.remove(caller_user_id);
                    state.mfa.pending_enrollments.remove(caller_user_id);
                    state.mfa.retry_budgets.remove(caller_user_id);
                    clear_pending_mfa_logins(state, caller_user_id);
                    state
                        .users
                        .get_mut(caller_user_id)
                        .expect("active user checked")
                        .mfa_enabled = false;
                    Ok(json!("done"))
                })
            }
            "auth/long_lived_access_token" => {
                ensure_command_fields(command, &["client_name", "lifespan", "client_icon"])?;
                let client_name = command
                    .get("client_name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::InvalidInput("client_name is required".to_owned()))?;
                validate_client_name(client_name)?;
                let lifespan = command
                    .get("lifespan")
                    .and_then(Value::as_u64)
                    .filter(|days| *days > 0 && *days <= MAX_LONG_LIVED_DAYS)
                    .ok_or_else(|| {
                        Error::InvalidInput(format!(
                            "lifespan must be between 1 and {MAX_LONG_LIVED_DAYS} days"
                        ))
                    })?;
                let client_icon = command
                    .get("client_icon")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let set = self.create_session(
                    caller_user_id,
                    SessionOptions {
                        client_id: None,
                        client_name: Some(client_name.to_owned()),
                        client_icon,
                        token_type: TOKEN_TYPE_LONG_LIVED,
                        access_token_expiration_secs: lifespan.saturating_mul(24 * 60 * 60),
                        refresh_lifetime_secs: None,
                        unique_long_lived_client_name: true,
                        mfa_authorized: current_session.is_some(),
                    },
                )?;
                Ok(Value::String(set.access_token))
            }
            "auth/sign_path" => {
                ensure_command_fields(command, &["path", "expires"])?;
                let path = required_string(command, "path")?;
                let expires = match command.get("expires") {
                    None => 30,
                    Some(value) => value.as_u64().ok_or_else(|| {
                        Error::InvalidInput("expires must be a positive integer".to_owned())
                    })?,
                };
                let current_session = current_session.ok_or_else(|| {
                    Error::InvalidInput(
                        "signing a path requires the active WebSocket session".to_owned(),
                    )
                })?;
                let signed =
                    self.create_signed_path(caller_user_id, current_session, path, expires)?;
                Ok(json!({"path": signed}))
            }
            "auth/refresh_tokens" => {
                ensure_command_fields(command, &[])?;
                self.refresh_token_metadata(caller_user_id, current_session)
            }
            "auth/delete_refresh_token" => {
                ensure_command_fields(command, &["refresh_token_id"])?;
                let session_id = command
                    .get("refresh_token_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        Error::InvalidInput("refresh_token_id is required".to_owned())
                    })?;
                self.revoke_session_id(caller_user_id, session_id)?;
                Ok(json!({}))
            }
            "auth/delete_all_refresh_tokens" => {
                ensure_command_fields(command, &["token_type", "delete_current_token"])?;
                let delete_current = command
                    .get("delete_current_token")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                if !delete_current && current_session.is_none() {
                    return Err(Error::InvalidInput(
                        "preserving the current token requires the active session ID".to_owned(),
                    ));
                }
                let token_type = command.get("token_type").and_then(Value::as_str);
                self.revoke_user_sessions_of_type(
                    caller_user_id,
                    token_type,
                    current_session,
                    delete_current,
                )?;
                Ok(json!({}))
            }
            "auth/refresh_token_set_expiry" => {
                ensure_command_fields(command, &["refresh_token_id", "enable_expiry"])?;
                let session_id = command
                    .get("refresh_token_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        Error::InvalidInput("refresh_token_id is required".to_owned())
                    })?;
                let enable_expiry = command
                    .get("enable_expiry")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| Error::InvalidInput("enable_expiry is required".to_owned()))?;
                self.set_session_expiry(caller_user_id, session_id, enable_expiry)?;
                Ok(json!({}))
            }
            _ => Err(Error::InvalidInput(format!(
                "unsupported authentication command: {command_type}"
            ))),
        }
    }

    fn create_session(&self, user_id: &str, options: SessionOptions) -> Result<TokenSet> {
        let now = now_epoch_seconds()?;
        self.transaction(|state| create_session_in_state(state, user_id, options, now))
    }

    fn require_active(&self, user_id: &str) -> Result<()> {
        let state = self.lock_state()?;
        require_active_user(&state, user_id).map(|_| ())
    }

    fn require_admin(&self, user_id: &str) -> Result<()> {
        let state = self.lock_state()?;
        let user = require_active_user(&state, user_id)?;
        if user_is_admin(user) {
            Ok(())
        } else {
            Err(Error::InvalidInput(
                "administrator privileges are required".to_owned(),
            ))
        }
    }

    fn revoke_session_id(&self, caller_user_id: &str, session_id: &str) -> Result<()> {
        self.transaction(|state| {
            require_active_user(state, caller_user_id)?;
            let session = state
                .sessions
                .get(session_id)
                .filter(|session| session.user_id == caller_user_id)
                .ok_or_else(|| Error::NotFound("refresh token not found".to_owned()))?;
            let id = session.id.clone();
            state.sessions.remove(&id);
            Ok(())
        })
    }

    fn revoke_user_sessions_of_type(
        &self,
        user_id: &str,
        token_type: Option<&str>,
        current_session: Option<&str>,
        delete_current: bool,
    ) -> Result<usize> {
        self.transaction(|state| {
            require_active_user(state, user_id)?;
            let before = state.sessions.len();
            state.sessions.retain(|_, session| {
                session.user_id != user_id
                    || token_type.is_some_and(|kind| session.token_type != kind)
                    || (!delete_current && Some(session.id.as_str()) == current_session)
            });
            Ok(before - state.sessions.len())
        })
    }

    fn set_session_expiry(
        &self,
        user_id: &str,
        session_id: &str,
        enable_expiry: bool,
    ) -> Result<()> {
        let now = now_epoch_seconds()?;
        self.transaction(|state| {
            require_active_user(state, user_id)?;
            let session = state
                .sessions
                .get_mut(session_id)
                .filter(|session| session.user_id == user_id)
                .ok_or_else(|| Error::NotFound("refresh token not found".to_owned()))?;
            session.refresh_expires_at = if enable_expiry {
                Some(now.saturating_add(REFRESH_TOKEN_EXPIRATION_SECS))
            } else {
                None
            };
            Ok(())
        })
    }

    fn refresh_token_metadata(
        &self,
        user_id: &str,
        current_session: Option<&str>,
    ) -> Result<Value> {
        let state = self.lock_state()?;
        require_active_user(&state, user_id)?;
        let tokens: Vec<Value> = state
            .sessions
            .values()
            .filter(|session| session.user_id == user_id)
            .map(|session| {
                json!({
                    "auth_provider_type": AUTH_PROVIDER_TYPE,
                    "client_icon": session.client_icon,
                    "client_id": session.client_id,
                    "client_name": session.client_name,
                    "created_at": iso8601(session.created_at),
                    "expire_at": session.refresh_expires_at.map(iso8601),
                    "id": session.id,
                    "is_current": Some(session.id.as_str()) == current_session,
                    "last_used_at": session.last_used_at.map(iso8601),
                    "last_used_ip": Value::Null,
                    "type": session.token_type,
                })
            })
            .collect();
        Ok(Value::Array(tokens))
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, AuthStore>> {
        self.inner
            .state
            .lock()
            .map_err(|_| Error::Storage("authentication state lock was poisoned".to_owned()))
    }

    fn transaction<T>(&self, operation: impl FnOnce(&mut AuthStore) -> Result<T>) -> Result<T> {
        let mut state = self.lock_state()?;
        let mut candidate = state.clone();
        let result = operation(&mut candidate)?;
        persist(&self.inner.runtime, &candidate)?;
        *state = candidate;
        Ok(result)
    }
}

fn create_session_in_state(
    state: &mut AuthStore,
    user_id: &str,
    options: SessionOptions,
    now: i64,
) -> Result<TokenSet> {
    let user = require_active_user(state, user_id)?;
    if user.mfa_enabled && !options.mfa_authorized {
        return Err(invalid_auth());
    }
    if options.unique_long_lived_client_name
        && state.sessions.values().any(|session| {
            session.user_id == user_id
                && session.token_type == TOKEN_TYPE_LONG_LIVED
                && session.client_name == options.client_name
        })
    {
        return Err(Error::InvalidInput(
            "a long-lived token with this client name already exists".to_owned(),
        ));
    }
    let user_id = user.id.clone();
    let session_id = new_id();
    let refresh_token = format!("ha_{}", random_secret());
    let session = StoredSession {
        id: session_id.clone(),
        user_id: user_id.clone(),
        client_id: options.client_id,
        client_name: options.client_name,
        client_icon: options.client_icon,
        token_type: options.token_type.to_owned(),
        refresh_token_hash: digest_refresh_token(&refresh_token),
        access_token_expiration_secs: options.access_token_expiration_secs,
        refresh_expires_at: options
            .refresh_lifetime_secs
            .map(|lifetime| now.saturating_add(lifetime)),
        created_at: now,
        last_used_at: Some(now),
    };
    let access_token = create_access_token(
        &state.jwt_secret,
        &session_id,
        &user_id,
        options.access_token_expiration_secs,
        now,
    )?;
    state.sessions.insert(session_id, session);
    Ok(TokenSet {
        access_token,
        token_type: "Bearer".to_owned(),
        expires_in: options.access_token_expiration_secs,
        refresh_token,
    })
}

fn jwt_validation() -> Validation {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.leeway = 0;
    validation.validate_aud = false;
    validation
}

fn decode_access_claims(secret: &str, token: &str) -> Result<AccessClaims> {
    let claims = decode::<AccessClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &jwt_validation(),
    )
    .map(|token_data| token_data.claims)
    .map_err(|_| invalid_auth())?;
    if claims.purpose.is_some() {
        return Err(invalid_auth());
    }
    Ok(claims)
}

fn active_identity(
    state: &AuthStore,
    session_id: &str,
    user_id: &str,
    now: i64,
) -> Result<AccessIdentity> {
    let session = state
        .sessions
        .get(session_id)
        .filter(|session| session.user_id == user_id)
        .filter(|session| {
            session
                .refresh_expires_at
                .is_none_or(|expires| expires > now)
        })
        .ok_or_else(invalid_auth)?;
    let user = state
        .users
        .get(&session.user_id)
        .filter(|user| user.is_active)
        .ok_or_else(invalid_auth)?;
    Ok(AccessIdentity {
        user: public_user(user),
        session_id: session.id.clone(),
    })
}

fn build_totp(secret: &str, account_name: &str) -> Result<Totp> {
    let secret = TotpSecret::try_from_base32(secret)
        .map_err(|_| Error::Storage("stored TOTP secret is invalid".to_owned()))?;
    TotpBuilder::new()
        .with_secret(secret)
        .with_algorithm(TotpAlgorithm::SHA1)
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(30)
        .with_issuer(Some("Home Assistant"))
        .with_account_name(account_name.to_owned())
        .build()
        .map_err(|_| Error::Storage("failed to initialize TOTP".to_owned()))
}

fn totp_matching_step(secret: &str, code: &str, now: i64) -> Result<Option<u64>> {
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(None);
    }
    let secret = TotpSecret::try_from_base32(secret)
        .map_err(|_| Error::Storage("stored TOTP secret is invalid".to_owned()))?;
    let totp = TotpBuilder::new()
        .with_secret(secret)
        .with_algorithm(TotpAlgorithm::SHA1)
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(30)
        .build()
        .map_err(|_| Error::Storage("failed to initialize TOTP".to_owned()))?;
    let timestamp = u64::try_from(now)
        .map_err(|_| Error::Storage("system clock is before the Unix epoch".to_owned()))?;
    Ok(totp.check(code, timestamp))
}

fn validate_totp_code_format(code: &str) -> Result<()> {
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::InvalidInput(
            "TOTP code must contain exactly six digits".to_owned(),
        ));
    }
    Ok(())
}

fn digest_secret(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_totp_secret(secret: &str) -> bool {
    TotpSecret::try_from_base32(secret).is_ok_and(|secret| secret.as_bytes().len() == 20)
}

fn clear_pending_mfa_logins(state: &mut AuthStore, user_id: &str) {
    state
        .mfa
        .login_challenges
        .retain(|_, challenge| challenge.user_id != user_id);
    state
        .mfa
        .login_grants
        .retain(|_, grant| grant.user_id != user_id);
}

fn prune_expired_mfa_state(mfa: &mut MfaStore, now: i64) {
    mfa.retry_budgets
        .retain(|_, budget| budget.expires_at > now);
    mfa.pending_enrollments
        .retain(|_, enrollment| enrollment.expires_at > now);
    mfa.login_challenges
        .retain(|_, challenge| challenge.expires_at > now && challenge.created_at <= now);
    mfa.login_grants.retain(|_, grant| grant.expires_at > now);
}

fn record_mfa_failure(mfa: &mut MfaStore, user_id: &str, now: i64) {
    let budget = mfa
        .retry_budgets
        .entry(user_id.into())
        .or_insert(MfaRetryBudget {
            attempts: 0,
            expires_at: now.saturating_add(300),
        });
    budget.attempts = budget.attempts.saturating_add(1);
    if budget.attempts >= MFA_MAX_RETRY_TIME {
        mfa.login_challenges
            .retain(|_, challenge| challenge.user_id != user_id);
    }
}

fn signed_path_parts(path: &str) -> Result<(String, Vec<(String, String)>)> {
    validate_signed_path_input(path)?;
    let (request_path, query) = path.split_once('?').unwrap_or((path, ""));
    let params = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _)| !matches!(key.as_ref(), "width" | "height" | "authSig"))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    Ok((request_path.to_owned(), params))
}

fn validate_signed_path_input(path: &str) -> Result<()> {
    let invalid = || Error::InvalidInput("signed path is invalid".to_owned());
    if path.is_empty()
        || path.len() > MAX_SIGNED_PATH_BYTES
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('#')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path.ends_with('?')
        || path.ends_with('&')
    {
        return Err(invalid());
    }
    if let Some((_, query)) = path.split_once('?')
        && url::form_urlencoded::parse(query.as_bytes()).any(|(key, _)| key == "authSig")
    {
        return Err(invalid());
    }
    Ok(())
}

fn public_user(user: &StoredUser) -> User {
    User {
        id: user.id.clone(),
        username: user.username.clone(),
        name: user.name.clone(),
        is_owner: user.is_owner,
        is_admin: user_is_admin(user),
        is_active: user.is_active,
        local_only: user.local_only,
        system_generated: user.system_generated,
        group_ids: user.group_ids.clone(),
        credentials: user
            .username
            .as_ref()
            .map(|_| {
                vec![UserCredential {
                    auth_provider_type: AUTH_PROVIDER_TYPE.to_owned(),
                    auth_provider_id: None,
                }]
            })
            .unwrap_or_default(),
        mfa_modules: vec![MfaModule {
            id: MFA_TOTP_ID.to_owned(),
            name: MFA_TOTP_NAME.to_owned(),
            enabled: user.mfa_enabled,
        }],
    }
}

fn current_user_payload(user: &User) -> Value {
    json!({
        "id": user.id,
        "name": user.name,
        "is_owner": user.is_owner,
        "is_admin": user.is_admin,
        "credentials": user.credentials,
        "mfa_modules": user.mfa_modules,
    })
}

fn config_user_payload(user: &User) -> Value {
    json!({
        "id": user.id,
        "username": user.username,
        "name": user.name,
        "is_owner": user.is_owner,
        "is_active": user.is_active,
        "local_only": user.local_only,
        "system_generated": user.system_generated,
        "group_ids": user.group_ids,
        "credentials": user.credentials.iter().map(|credential| {
            json!({"type": credential.auth_provider_type})
        }).collect::<Vec<_>>(),
    })
}

fn user_is_admin(user: &StoredUser) -> bool {
    user.is_owner || (user.is_active && user.group_ids.iter().any(|group| group == GROUP_ID_ADMIN))
}

fn require_active_user<'a>(state: &'a AuthStore, user_id: &str) -> Result<&'a StoredUser> {
    state
        .users
        .get(user_id)
        .filter(|user| user.is_active)
        .ok_or_else(invalid_auth)
}

fn validate_store(state: &AuthStore) -> Result<()> {
    if state.version != AUTH_SCHEMA_VERSION {
        return Err(Error::Storage(format!(
            "unsupported authentication metadata version {}",
            state.version
        )));
    }
    if state.jwt_secret.len() != 64
        || !state
            .jwt_secret
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(Error::Storage(
            "stored JWT signing key is invalid".to_owned(),
        ));
    }
    for (id, user) in &state.users {
        let has_consistent_local_credential = user.username.as_deref().is_none_or(|username| {
            let normalized = normalize_username(username);
            normalized == username
                && state
                    .homeassistant_provider
                    .users
                    .get(&normalized)
                    .is_some_and(|credential| credential.username == username)
        });
        if id != &user.id
            || !has_consistent_local_credential
            || user.mfa_enabled != state.mfa.totp_users.contains_key(id)
        {
            return Err(Error::Storage(
                "stored authentication user data is inconsistent".to_owned(),
            ));
        }
    }
    for (username, user) in &state.homeassistant_provider.users {
        if username != &normalize_username(&user.username)
            || !state
                .users
                .values()
                .any(|stored| stored.username.as_deref() == Some(user.username.as_str()))
        {
            return Err(Error::Storage(
                "stored Home Assistant provider data is inconsistent".to_owned(),
            ));
        }
        // Parse once on startup so corrupted hashes are reported as storage
        // failures rather than as ordinary bad passwords during a login.
        PasswordHash::new(&user.password_hash)
            .map_err(|_| Error::Storage("stored password hash is invalid".to_owned()))?;
    }
    for (id, session) in &state.sessions {
        if id != &session.id
            || !state.users.contains_key(&session.user_id)
            || session.refresh_token_hash.len() != 64
            || !session
                .refresh_token_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || session.access_token_expiration_secs == 0
        {
            return Err(Error::Storage(
                "stored authentication session data is inconsistent".to_owned(),
            ));
        }
    }
    for (user_id, credential) in &state.mfa.totp_users {
        if !state
            .users
            .get(user_id)
            .is_some_and(|user| user.mfa_enabled && !user.system_generated)
            || !valid_totp_secret(&credential.secret)
        {
            return Err(Error::Storage(
                "stored TOTP credential data is inconsistent".to_owned(),
            ));
        }
    }
    for (user_id, enrollment) in &state.mfa.pending_enrollments {
        if !state
            .users
            .get(user_id)
            .is_some_and(|user| !user.system_generated && !user.mfa_enabled)
            || !valid_totp_secret(&enrollment.secret)
        {
            return Err(Error::Storage(
                "stored TOTP enrollment data is inconsistent".to_owned(),
            ));
        }
    }
    for (digest, challenge) in &state.mfa.login_challenges {
        if !is_sha256_digest(digest)
            || !state
                .users
                .get(&challenge.user_id)
                .is_some_and(|user| user.is_active && user.mfa_enabled)
            || challenge.attempts >= MFA_MAX_RETRY_TIME
        {
            return Err(Error::Storage(
                "stored MFA login challenge data is inconsistent".to_owned(),
            ));
        }
    }
    for (digest, grant) in &state.mfa.login_grants {
        if !is_sha256_digest(digest)
            || !state
                .users
                .get(&grant.user_id)
                .is_some_and(|user| user.is_active && user.mfa_enabled)
        {
            return Err(Error::Storage(
                "stored MFA login grant data is inconsistent".to_owned(),
            ));
        }
    }
    Ok(())
}

fn persist(runtime: &Runtime, state: &AuthStore) -> Result<()> {
    let value = serde_json::to_value(state)
        .map_err(|_| Error::Storage("failed to serialize authentication state".to_owned()))?;
    runtime.set_data(AUTH_DATA_KEY, &value)
}

fn validate_new_username(username: &str) -> Result<()> {
    if username.is_empty()
        || username.len() > MAX_USERNAME_BYTES
        || username.chars().any(char::is_control)
        || normalize_username(username) != username
    {
        return Err(Error::InvalidInput(
            "username must be non-empty, trimmed, case-folded, and at most 255 bytes".to_owned(),
        ));
    }
    Ok(())
}

fn validate_display_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.trim().len() > MAX_DISPLAY_NAME_BYTES
        || name.chars().any(char::is_control)
    {
        return Err(Error::InvalidInput(
            "display name must be non-empty and at most 255 bytes".to_owned(),
        ));
    }
    Ok(())
}

fn validate_password(password: &str) -> Result<()> {
    if password.is_empty() || password.len() > MAX_PASSWORD_BYTES {
        return Err(Error::InvalidInput(
            "password must be non-empty and at most 4096 bytes".to_owned(),
        ));
    }
    Ok(())
}

fn validate_client_id(client_id: &str) -> Result<()> {
    if client_id.trim().is_empty()
        || client_id.len() > MAX_CLIENT_ID_BYTES
        || client_id.chars().any(char::is_control)
    {
        return Err(Error::InvalidInput("client_id is invalid".to_owned()));
    }
    Ok(())
}

fn validate_client_name(client_name: &str) -> Result<()> {
    if client_name.trim().is_empty()
        || client_name.len() > MAX_CLIENT_NAME_BYTES
        || client_name.chars().any(char::is_control)
    {
        return Err(Error::InvalidInput("client_name is invalid".to_owned()));
    }
    Ok(())
}

fn validate_optional_client_name(client_name: Option<&str>) -> Result<()> {
    if let Some(client_name) = client_name {
        validate_client_name(client_name)?;
    }
    Ok(())
}

fn normalize_username(username: &str) -> String {
    let mut normalized = String::new();
    for character in username.trim().chars() {
        match character {
            // Python's str.casefold(), used by the pinned provider, expands
            // sharp-s and folds Greek final sigma.
            '\u{00df}' | '\u{1e9e}' => normalized.push_str("ss"),
            '\u{03c2}' => normalized.push('\u{03c3}'),
            _ => normalized.extend(character.to_lowercase()),
        }
    }
    normalized
}

fn validate_group_ids(group_ids: &[String]) -> Result<Vec<String>> {
    if group_ids.len() > 64 {
        return Err(Error::InvalidInput("too many user groups".to_owned()));
    }
    let mut validated = Vec::new();
    for group_id in group_ids {
        if !matches!(
            group_id.as_str(),
            GROUP_ID_ADMIN | GROUP_ID_USER | GROUP_ID_READ_ONLY
        ) {
            return Err(Error::InvalidInput(format!(
                "unsupported user group: {group_id}"
            )));
        }
        if !validated.contains(group_id) {
            validated.push(group_id.clone());
        }
    }
    Ok(validated)
}

fn ensure_command_fields(command: &Value, allowed_fields: &[&str]) -> Result<()> {
    let object = command
        .as_object()
        .ok_or_else(|| Error::InvalidInput("auth command must be an object".to_owned()))?;
    for field in object.keys() {
        if field == "id" || field == "type" || allowed_fields.contains(&field.as_str()) {
            continue;
        }
        return Err(Error::InvalidInput(format!(
            "unsupported field in authentication command: {field}"
        )));
    }
    Ok(())
}

fn required_string<'a>(command: &'a Value, key: &str) -> Result<&'a str> {
    command
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidInput(format!("{key} is required and must be a string")))
}

fn optional_string<'a>(command: &'a Value, key: &str) -> Result<Option<&'a str>> {
    command
        .get(key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| Error::InvalidInput(format!("{key} must be a string when provided")))
        })
        .transpose()
}

fn optional_bool(command: &Value, key: &str) -> Result<Option<bool>> {
    command
        .get(key)
        .map(|value| {
            value.as_bool().ok_or_else(|| {
                Error::InvalidInput(format!("{key} must be a boolean when provided"))
            })
        })
        .transpose()
}

fn optional_string_array(command: &Value, key: &str) -> Result<Option<Vec<String>>> {
    command
        .get(key)
        .map(|value| {
            let values = value
                .as_array()
                .ok_or_else(|| Error::InvalidInput(format!("{key} must be an array")))?;
            values
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_owned).ok_or_else(|| {
                        Error::InvalidInput(format!("{key} entries must be strings"))
                    })
                })
                .collect()
        })
        .transpose()
}

fn hash_password(password: &str) -> Result<String> {
    let salt = random_password_salt()?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| Error::Storage("failed to hash password".to_owned()))
}

fn verify_password(password: &str, hash: &str) -> Result<bool> {
    let parsed = PasswordHash::new(hash)
        .map_err(|_| Error::Storage("stored password hash is invalid".to_owned()))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

fn dummy_password_hash() -> Result<String> {
    static DUMMY_HASH: OnceLock<std::result::Result<String, String>> = OnceLock::new();
    match DUMMY_HASH.get_or_init(|| {
        let salt = random_password_salt().map_err(|error| error.to_string())?;
        Argon2::default()
            .hash_password(b"not-a-real-home-assistant-password", &salt)
            .map(|hash| hash.to_string())
            .map_err(|_| "failed to initialize dummy password hash".to_owned())
    }) {
        Ok(hash) => Ok(hash.clone()),
        Err(message) => Err(Error::Storage(message.clone())),
    }
}

fn create_access_token(
    jwt_secret: &str,
    session_id: &str,
    user_id: &str,
    lifetime_secs: u64,
    now: i64,
) -> Result<String> {
    let issued_at = u64::try_from(now)
        .map_err(|_| Error::Storage("system clock is before the Unix epoch".to_owned()))?;
    let claims = AccessClaims {
        iss: session_id.to_owned(),
        sub: user_id.to_owned(),
        iat: issued_at,
        exp: issued_at.saturating_add(lifetime_secs),
        purpose: None,
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(jwt_secret.as_bytes()),
    )
    .map_err(|_| Error::Storage("failed to sign access token".to_owned()))
}

fn digest_refresh_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn constant_time_eq_hex(left: &str, right: &str) -> bool {
    left.as_bytes().ct_eq(right.as_bytes()).unwrap_u8() == 1
}

fn random_secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn random_password_salt() -> Result<SaltString> {
    // UUID v4 uses the OS CSPRNG. Two UUIDs provide a 32-byte random salt
    // without exposing password-hash internals or keeping an RNG in the manager.
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(first.as_bytes());
    bytes[16..].copy_from_slice(second.as_bytes());
    SaltString::encode_b64(&bytes)
        .map_err(|_| Error::Storage("failed to generate password salt".to_owned()))
}

fn new_id() -> String {
    Uuid::new_v4().simple().to_string()
}

fn now_epoch_seconds() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .map_err(|_| Error::Storage("system clock is before the Unix epoch".to_owned()))
}

fn iso8601(timestamp: i64) -> String {
    // This value is only emitted for WebSocket metadata; checked conversion
    // keeps malformed stored timestamps from panicking a command handler.
    match chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0) {
        Some(value) => value.to_rfc3339(),
        None => timestamp.to_string(),
    }
}

fn invalid_auth() -> Error {
    Error::InvalidInput("invalid authentication credentials".to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use hass_core::{Error, MemoryStore, Result, Runtime, Store};
    use hass_storage::SqliteStore;
    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::{
        AUTH_DATA_KEY, AuthManager, AuthStore, SignedPathClaims, build_totp, create_access_token,
        now_epoch_seconds, totp_matching_step,
    };

    fn manager() -> (Arc<MemoryStore>, AuthManager) {
        let store = Arc::new(MemoryStore::default());
        let runtime = Runtime::new(store.clone()).expect("runtime");
        let auth = AuthManager::new(runtime.clone()).expect("auth manager");
        (store, auth)
    }

    fn totp_code_at(secret: &str, timestamp: i64) -> String {
        build_totp(secret, "test-account")
            .expect("build TOTP")
            .generate(u64::try_from(timestamp).expect("positive timestamp"))
            .to_string()
    }

    #[test]
    fn totp_uses_the_pinned_sha1_six_digit_thirty_second_fixture() {
        // RFC 6238's SHA-1 seed encodes to this Base32 value. pyotp's default
        // settings used by Home Assistant are SHA1, six digits, 30 seconds.
        let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        assert_eq!(totp_matching_step(secret, "287082", 59).unwrap(), Some(1));
        assert_eq!(totp_matching_step(secret, "287082", 120).unwrap(), None);
    }

    #[test]
    fn bootstrap_credentials_roles_and_disabled_users() {
        let (_, auth) = manager();
        assert!(!auth.has_users().expect("has users"));
        let owner = auth
            .bootstrap_owner_with_username("owner", "Home Owner", "owner-password")
            .expect("owner");
        assert!(owner.is_owner);
        assert!(owner.is_admin);
        assert_eq!(owner.name, "Home Owner");
        assert!(
            !serde_json::to_string(&owner)
                .expect("public user JSON")
                .contains("password_hash")
        );

        let user = auth
            .create_user_with_username(&owner.id, "alex", "Alex Display", "user-password", false)
            .expect("create regular user");
        assert_eq!(user.name, "Alex Display");
        assert!(!user.is_admin);
        assert_eq!(
            auth.verify_credentials("ALEX", "user-password")
                .expect("login")
                .id,
            user.id
        );
        assert!(auth.verify_credentials("alex", "wrong").is_err());
        assert!(
            auth.create_user_with_username(&user.id, "mallory", "Mallory", "pw", false)
                .is_err()
        );

        let admin = auth
            .create_user_with_username(&owner.id, "admin", "Administrator", "admin-password", true)
            .expect("create admin");
        assert!(admin.is_admin);
        assert!(
            auth.create_user_with_username(&admin.id, "other-admin", "Other Admin", "pw", true)
                .is_err()
        );
        assert!(
            auth.create_user_with_username(&admin.id, "reader", "Reader", "reader-password", false)
                .is_ok()
        );

        auth.set_user_active(&owner.id, &user.id, false)
            .expect("disable user");
        assert!(auth.verify_credentials("alex", "user-password").is_err());
        assert!(auth.has_users().expect("has users"));
        assert!(auth.set_user_active(&owner.id, &owner.id, false).is_err());
    }

    #[test]
    fn jwt_tampering_expiry_and_user_revocation_are_rejected() {
        let (_, auth) = manager();
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "owner-password")
            .expect("owner");
        let tokens = auth
            .issue_tokens(&owner.id, "https://client.example/", Some("test client"))
            .expect("issue tokens");
        assert_eq!(
            auth.validate_access_token(&tokens.access_token).unwrap().id,
            owner.id
        );

        let mut tampered = tokens.access_token.clone().into_bytes();
        let position = tampered.len() - 1;
        tampered[position] = if tampered[position] == b'A' {
            b'B'
        } else {
            b'A'
        };
        assert!(
            auth.validate_access_token(std::str::from_utf8(&tampered).expect("ASCII JWT"))
                .is_err()
        );

        let state = auth.lock_state().expect("state lock");
        let session_id = state
            .sessions
            .values()
            .find(|session| session.user_id == owner.id)
            .expect("session")
            .id
            .clone();
        let expired = create_access_token(
            &state.jwt_secret,
            &session_id,
            &owner.id,
            1,
            now_epoch_seconds().expect("clock") - 5,
        )
        .expect("expired JWT");
        drop(state);
        assert!(auth.validate_access_token(&expired).is_err());

        auth.set_user_active(&owner.id, &owner.id, false)
            .expect_err("cannot disable owner");
        let regular = auth
            .create_user_with_username(&owner.id, "reader", "Reader", "reader-password", false)
            .expect("reader");
        let reader_tokens = auth
            .issue_tokens(&regular.id, "reader-client", None)
            .expect("reader session");
        auth.set_user_active(&owner.id, &regular.id, false)
            .expect("disable reader");
        assert!(
            auth.validate_access_token(&reader_tokens.access_token)
                .is_err()
        );
    }

    #[test]
    fn refresh_rotation_replay_revocation_and_restart() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("auth.sqlite3");
        let runtime = Runtime::new(Arc::new(SqliteStore::open(&path).expect("open database")))
            .expect("runtime");
        let auth = AuthManager::new(runtime.clone()).expect("auth manager");
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "owner-password")
            .expect("owner");
        let original = auth
            .issue_tokens(&owner.id, "client-a", Some("client A"))
            .expect("issue tokens");
        let rotated = auth
            .refresh_tokens_for_client(&original.refresh_token, "client-a")
            .expect("rotate refresh token");
        assert!(
            auth.refresh_tokens_for_client(&original.refresh_token, "client-a")
                .is_err()
        );
        assert!(
            auth.refresh_tokens_for_client(&rotated.refresh_token, "another-client")
                .is_err()
        );
        assert!(auth.validate_access_token(&rotated.access_token).is_ok());

        drop(auth);
        drop(runtime);
        let runtime = Runtime::new(Arc::new(SqliteStore::open(&path).expect("reopen database")))
            .expect("reopened runtime");
        let auth = AuthManager::new(runtime).expect("reloaded auth manager");
        assert_eq!(
            auth.validate_access_token(&rotated.access_token)
                .unwrap()
                .id,
            owner.id
        );
        assert!(
            auth.revoke_refresh_token(&rotated.refresh_token)
                .expect("revoke refresh token")
        );
        assert!(auth.validate_access_token(&rotated.access_token).is_err());
        assert!(
            !auth
                .revoke_refresh_token(&rotated.refresh_token)
                .expect("revoke again")
        );
    }

    #[test]
    fn websocket_current_user_and_long_lived_tokens_use_native_sessions() {
        let (_, auth) = manager();
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "owner-password")
            .expect("owner");
        let payload = auth
            .handle_ws(&json!({"type":"auth/current_user"}), &owner.id)
            .expect("current user");
        assert_eq!(payload["id"], owner.id);
        assert_eq!(payload["name"], "Owner");
        assert_eq!(
            payload["credentials"][0]["auth_provider_type"],
            "homeassistant"
        );
        assert_eq!(
            payload["mfa_modules"],
            json!([{"id":"totp","name":"Time-based One Time Password","enabled":false}])
        );
        assert_eq!(
            auth.handle_ws(&json!({"type":"auth/mfa_modules"}), &owner.id)
                .expect("available MFA modules"),
            payload["mfa_modules"]
        );

        let access = auth
            .handle_ws(
                &json!({
                    "type":"auth/long_lived_access_token",
                    "client_name":"sensor bridge",
                    "lifespan":365
                }),
                &owner.id,
            )
            .expect("long lived token")
            .as_str()
            .expect("access token string")
            .to_owned();
        assert_eq!(auth.validate_access_token(&access).unwrap().id, owner.id);
        let sessions = auth
            .handle_ws(&json!({"type":"auth/refresh_tokens"}), &owner.id)
            .expect("refresh token metadata");
        assert_eq!(sessions.as_array().unwrap().len(), 1);
        let session_id = sessions[0]["id"].as_str().expect("session ID");
        auth.handle_ws(
            &json!({"type":"auth/delete_refresh_token","refresh_token_id":session_id}),
            &owner.id,
        )
        .expect("revoke session");
        assert!(auth.validate_access_token(&access).is_err());
    }

    #[test]
    fn websocket_config_auth_commands_persist_roles_updates_and_deletion() {
        let (_, auth) = manager();
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "owner-password")
            .expect("owner");

        let created = auth
            .handle_ws(
                &json!({
                    "type":"config/auth/create",
                    "name":"No Local Login",
                    "group_ids":["system-users"],
                    "local_only":true
                }),
                &owner.id,
            )
            .expect("create no-credential user");
        let user = &created["user"];
        let user_id = user["id"].as_str().expect("created user ID").to_owned();
        assert_eq!(user["username"], Value::Null);
        assert_eq!(user["credentials"], json!([]));
        assert_eq!(user["local_only"], true);
        assert!(!created.to_string().contains("password_hash"));

        let listing = auth
            .handle_ws(&json!({"type":"config/auth/list"}), &owner.id)
            .expect("list users");
        assert_eq!(listing.as_array().unwrap().len(), 2);

        let updated = auth
            .handle_ws(
                &json!({
                    "type":"config/auth/update",
                    "user_id":user_id,
                    "name":"Updated Display Name",
                    "is_active":false,
                    "group_ids":["system-read-only"],
                    "local_only":false
                }),
                &owner.id,
            )
            .expect("update user");
        assert_eq!(updated["user"]["name"], "Updated Display Name");
        assert_eq!(updated["user"]["is_active"], false);
        assert_eq!(updated["user"]["group_ids"], json!(["system-read-only"]));
        assert!(!auth.get_user(&user_id).unwrap().is_active);

        let admin = auth
            .handle_ws(
                &json!({
                    "type":"config/auth/create",
                    "name":"Admin",
                    "group_ids":["system-admin","system-users"]
                }),
                &owner.id,
            )
            .expect("owner creates admin");
        let admin_id = admin["user"]["id"].as_str().expect("admin ID").to_owned();
        assert!(auth.get_user(&admin_id).unwrap().is_admin);
        assert!(
            auth.handle_ws(
                &json!({
                    "type":"config/auth/create",
                    "name":"Unauthorized Admin",
                    "group_ids":["system-admin"]
                }),
                &admin_id,
            )
            .is_err()
        );

        assert!(
            auth.handle_ws(
                &json!({"type":"config/auth/delete","user_id":owner.id}),
                &owner.id,
            )
            .is_err()
        );
        assert_eq!(
            auth.handle_ws(
                &json!({"type":"config/auth/delete","user_id":user_id}),
                &owner.id,
            )
            .expect("delete user"),
            Value::Null
        );
        assert!(auth.get_user(&user_id).is_err());
    }

    #[test]
    fn session_identity_marks_current_and_preserves_it_when_deleting_others() {
        let (_, auth) = manager();
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "owner-password")
            .expect("owner");
        let first = auth
            .issue_tokens(&owner.id, "client-a", Some("first"))
            .expect("first session");
        let second = auth
            .issue_tokens(&owner.id, "client-b", Some("second"))
            .expect("second session");
        let first_identity = auth
            .validate_access_token_identity(&first.access_token)
            .expect("first identity");
        assert_eq!(first_identity.user.id, owner.id);
        assert_eq!(
            first_identity.session_id,
            auth.lock_state()
                .expect("state lock")
                .sessions
                .values()
                .find(|session| session.client_id.as_deref() == Some("client-a"))
                .expect("first session record")
                .id
        );

        let sessions = auth
            .handle_ws_session(
                &json!({"type":"auth/refresh_tokens"}),
                &owner.id,
                Some(&first_identity.session_id),
            )
            .expect("session metadata");
        assert_eq!(
            sessions
                .as_array()
                .expect("session array")
                .iter()
                .filter(|session| session["is_current"] == true)
                .count(),
            1
        );
        assert!(sessions.as_array().unwrap().iter().any(|session| {
            session["id"] == first_identity.session_id && session["is_current"] == true
        }));

        auth.handle_ws_session(
            &json!({"type":"auth/delete_all_refresh_tokens","delete_current_token":false}),
            &owner.id,
            Some(&first_identity.session_id),
        )
        .expect("delete other sessions");
        assert!(auth.validate_access_token(&first.access_token).is_ok());
        assert!(auth.validate_access_token(&second.access_token).is_err());
        assert!(
            auth.handle_ws(
                &json!({"type":"auth/delete_all_refresh_tokens","delete_current_token":false}),
                &owner.id,
            )
            .is_err()
        );
    }

    #[test]
    fn local_credentials_enroll_update_change_password_and_redact_public_data() {
        let (store, auth) = manager();
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "owner-password")
            .expect("owner");
        let user = auth
            .create_user_without_credentials(&owner.id, "Alex", None, None)
            .expect("user without local credentials");
        let enrolled = auth
            .enroll_local_credentials(&owner.id, &user.id, "alex", "first-password")
            .expect("enroll local credentials");
        assert_eq!(enrolled.username.as_deref(), Some("alex"));
        assert!(auth.verify_credentials("ALEX", "first-password").is_ok());
        assert!(auth.verify_credentials("alex", "wrong-password").is_err());

        let updated = auth
            .update_local_credentials(
                &owner.id,
                &user.id,
                Some("alex.new"),
                Some("second-password"),
            )
            .expect("update local provider data");
        assert_eq!(updated.username.as_deref(), Some("alex.new"));
        assert!(auth.verify_credentials("alex", "second-password").is_err());
        assert!(
            auth.verify_credentials("alex.new", "second-password")
                .is_ok()
        );
        assert!(
            auth.change_password(&user.id, "wrong-password", "third-password")
                .is_err()
        );
        auth.change_password(&user.id, "second-password", "third-password")
            .expect("change password");
        assert!(
            auth.verify_credentials("alex.new", "second-password")
                .is_err()
        );
        assert!(
            auth.verify_credentials("alex.new", "third-password")
                .is_ok()
        );

        let public = serde_json::to_string(&auth.get_user(&user.id).expect("public user"))
            .expect("public user JSON");
        assert!(!public.contains("third-password"));
        assert!(!public.contains("password_hash"));
        let persisted = store
            .get_data(AUTH_DATA_KEY)
            .expect("read auth metadata")
            .expect("auth metadata");
        let persisted = persisted.to_string();
        assert!(!persisted.contains("first-password"));
        assert!(!persisted.contains("second-password"));
        assert!(!persisted.contains("third-password"));
    }

    #[test]
    fn totp_enrollment_challenge_replay_expiry_disable_and_restart() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("mfa-auth.sqlite3");
        let store = Arc::new(SqliteStore::open(&path).expect("open database"));
        let runtime = Runtime::new(store.clone()).expect("runtime");
        let auth = AuthManager::new(runtime.clone()).expect("auth manager");
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "owner-password")
            .expect("owner");
        let user = auth
            .create_user_with_username(&owner.id, "alex", "Alex", "user-password", false)
            .expect("user");
        let enrollment = auth
            .begin_mfa_enrollment(&user.id)
            .expect("begin enrollment");
        assert_eq!(enrollment.secret.len(), 32);
        assert!(enrollment.otpauth_uri.starts_with("otpauth://totp/"));
        assert!(enrollment.otpauth_uri.contains(&enrollment.secret));
        let before = auth.get_user(&user.id).expect("pre-enrollment user");
        assert!(!before.mfa_modules[0].enabled);
        assert!(
            !serde_json::to_string(&before)
                .expect("public user JSON")
                .contains(&enrollment.secret)
        );
        assert!(auth.confirm_mfa_enrollment(&user.id, "wrong").is_err());

        let now = now_epoch_seconds().expect("clock");
        let enrollment_code = totp_code_at(&enrollment.secret, now);
        let enabled_user = auth
            .confirm_mfa_enrollment(&user.id, &enrollment_code)
            .expect("confirm enrollment");
        assert!(enabled_user.mfa_modules[0].enabled);
        let persisted = store
            .get_data(AUTH_DATA_KEY)
            .expect("read metadata")
            .expect("auth metadata");
        assert!(persisted.to_string().contains(&enrollment.secret));

        let sessions_before = auth.lock_state().expect("state").sessions.len();
        assert!(auth.authenticate("alex", "user-password").is_err());
        assert!(auth.issue_tokens(&user.id, "direct-client", None).is_err());
        assert_eq!(
            auth.lock_state().expect("state").sessions.len(),
            sessions_before
        );
        assert!(auth.begin_mfa_login("alex", "wrong-password").is_err());

        let challenge_id = match auth
            .begin_mfa_login("alex", "user-password")
            .expect("begin login")
        {
            super::LoginAuthResult::MfaRequired { challenge_id, .. } => challenge_id,
            other => panic!("expected MFA challenge, got {other:?}"),
        };
        let next_step = (now / 30 + 1) * 30;
        let login_code = totp_code_at(&enrollment.secret, next_step);
        let grant = auth
            .complete_mfa_login(&challenge_id, &login_code)
            .expect("complete TOTP challenge");
        assert_eq!(
            store
                .get_data(AUTH_DATA_KEY)
                .expect("read metadata")
                .unwrap()["mfa"]["login_grants"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
        let tokens = auth
            .issue_tokens_after_mfa(grant.clone(), "mfa-client", Some("MFA client"))
            .expect("issue tokens after MFA");
        let identity = auth
            .validate_access_token_identity(&tokens.access_token)
            .expect("validated MFA token");
        assert_eq!(identity.user.id, user.id);
        assert!(
            auth.issue_tokens_after_mfa(grant, "mfa-client", None)
                .is_err()
        );

        let replay_challenge = match auth
            .begin_mfa_login("alex", "user-password")
            .expect("begin replay challenge")
        {
            super::LoginAuthResult::MfaRequired { challenge_id, .. } => challenge_id,
            other => panic!("expected MFA challenge, got {other:?}"),
        };
        assert!(
            auth.complete_mfa_login(&replay_challenge, &login_code)
                .is_err()
        );

        let expired_challenge = match auth
            .begin_mfa_login("alex", "user-password")
            .expect("begin expiring challenge")
        {
            super::LoginAuthResult::MfaRequired { challenge_id, .. } => challenge_id,
            other => panic!("expected MFA challenge, got {other:?}"),
        };
        let expired_digest = super::digest_secret(&expired_challenge);
        auth.lock_state()
            .expect("state lock")
            .mfa
            .login_challenges
            .get_mut(&expired_digest)
            .expect("challenge record")
            .expires_at = now - 1;
        assert!(
            auth.complete_mfa_login(&expired_challenge, &login_code)
                .is_err()
        );

        drop(auth);
        drop(runtime);
        drop(store);
        let runtime = Runtime::new(Arc::new(SqliteStore::open(&path).expect("reopen database")))
            .expect("runtime after restart");
        let reloaded = AuthManager::new(runtime).expect("reload auth");
        assert!(reloaded.get_user(&user.id).unwrap().mfa_modules[0].enabled);
    }

    #[test]
    fn totp_disable_requires_a_fresh_valid_code() {
        let (_, auth) = manager();
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "owner-password")
            .expect("owner");
        let user = auth
            .create_user_with_username(&owner.id, "alex", "Alex", "user-password", false)
            .expect("user");
        let enrollment = auth.begin_mfa_enrollment(&user.id).expect("enrollment");
        let now = now_epoch_seconds().expect("clock");
        let enrollment_code = totp_code_at(&enrollment.secret, now);
        auth.confirm_mfa_enrollment(&user.id, &enrollment_code)
            .expect("confirm enrollment");
        assert!(auth.disable_mfa(&user.id, &enrollment_code).is_err());

        let unused_step = (now / 30 + 1) * 30;
        let fresh_code = totp_code_at(&enrollment.secret, unused_step);
        let user = auth
            .disable_mfa(&user.id, &fresh_code)
            .expect("disable with a fresh code from the accepted adjacent step");
        assert!(!user.mfa_modules[0].enabled);
    }

    #[test]
    fn signed_path_is_bound_to_exact_path_expiry_user_and_session() {
        let (_, auth) = manager();
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "owner-password")
            .expect("owner");
        let tokens = auth
            .issue_tokens(&owner.id, "signed-path-client", None)
            .expect("issue session");
        let identity = auth
            .validate_access_token_identity(&tokens.access_token)
            .expect("identity");
        let path = "/api/states?entity_id=sensor.temperature&width=100";
        let signed = auth
            .create_signed_path(&owner.id, &identity.session_id, path, 60)
            .expect("sign path");
        assert!(signed.starts_with(path));
        let signature = signed.split_once("authSig=").expect("authSig parameter").1;
        let signed_identity = auth
            .validate_signed_request(path, signature)
            .expect("validate signed path");
        assert_eq!(signed_identity.user.id, owner.id);
        assert_eq!(signed_identity.session_id, identity.session_id);
        assert!(auth.validate_access_token(signature).is_err());
        assert!(
            auth.validate_signed_request(
                "/api/states?entity_id=sensor.temperature&width=400&height=240",
                signature,
            )
            .is_ok()
        );
        assert!(
            auth.validate_signed_request("/api/states?entity_id=light.kitchen", signature)
                .is_err()
        );
        assert!(
            auth.create_signed_path(&owner.id, "wrong-session", path, 60)
                .is_err()
        );
        assert!(
            auth.create_signed_path(&owner.id, &identity.session_id, "/api/data?authSig=x", 60)
                .is_err()
        );
        assert!(
            auth.create_signed_path(&owner.id, &identity.session_id, path, 0)
                .is_err()
        );

        let ws_signed = auth
            .handle_ws_session(
                &json!({"type":"auth/sign_path","path":path,"expires":60}),
                &owner.id,
                Some(&identity.session_id),
            )
            .expect("WebSocket sign path");
        let ws_url = ws_signed["path"].as_str().expect("signed URL");
        let ws_signature = ws_url.split_once("authSig=").unwrap().1;
        assert!(auth.validate_signed_request(path, ws_signature).is_ok());

        let now = now_epoch_seconds().expect("clock");
        let expired = {
            let state = auth.lock_state().expect("state");
            let claims = SignedPathClaims {
                iss: identity.session_id.clone(),
                sub: owner.id.clone(),
                path: "/api/states".to_owned(),
                params: super::signed_path_parts(path).unwrap().1,
                purpose: "signed_path".to_owned(),
                iat: u64::try_from(now - 60).unwrap(),
                exp: u64::try_from(now - 1).unwrap(),
            };
            jsonwebtoken::encode(
                &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
                &claims,
                &jsonwebtoken::EncodingKey::from_secret(state.jwt_secret.as_bytes()),
            )
            .expect("expired signed URL")
        };
        assert!(auth.validate_signed_request(path, &expired).is_err());
    }

    #[test]
    fn persistence_failure_does_not_initialize_or_publish_auth_state() {
        let store = Arc::new(FailingMetadataStore::new(true));
        let runtime = Runtime::new(store.clone()).expect("runtime");
        assert!(AuthManager::new(runtime).is_err());
        assert!(store.inner.get_data(AUTH_DATA_KEY).unwrap().is_none());

        let store = Arc::new(FailingMetadataStore::new(false));
        let runtime = Runtime::new(store.clone()).expect("runtime");
        let auth = AuthManager::new(runtime).expect("auth manager");
        store.fail_writes.store(true, Ordering::SeqCst);
        assert!(
            auth.bootstrap_owner_with_username("owner", "Owner", "owner-password")
                .is_err()
        );
        assert!(!auth.has_users().expect("unchanged memory state"));
        let persisted: AuthStore = serde_json::from_value(
            store
                .inner
                .get_data(AUTH_DATA_KEY)
                .unwrap()
                .expect("initial metadata"),
        )
        .expect("initial auth state");
        assert!(persisted.users.is_empty());
    }

    struct FailingMetadataStore {
        inner: MemoryStore,
        fail_writes: AtomicBool,
    }

    impl FailingMetadataStore {
        fn new(fail_writes: bool) -> Self {
            Self {
                inner: MemoryStore::default(),
                fail_writes: AtomicBool::new(fail_writes),
            }
        }
    }

    impl Store for FailingMetadataStore {
        fn load_states(&self) -> Result<Vec<hass_core::State>> {
            self.inner.load_states()
        }

        fn put_state(
            &self,
            state: &hass_core::State,
            event: Option<&hass_core::Event>,
        ) -> Result<()> {
            self.inner.put_state(state, event)
        }

        fn remove_state(&self, entity_id: &str, event: &hass_core::Event) -> Result<()> {
            self.inner.remove_state(entity_id, event)
        }

        fn append_event(&self, event: &hass_core::Event) -> Result<()> {
            self.inner.append_event(event)
        }

        fn get_data(&self, key: &str) -> Result<Option<Value>> {
            self.inner.get_data(key)
        }

        fn set_data(&self, key: &str, value: &Value) -> Result<()> {
            if self.fail_writes.load(Ordering::SeqCst) {
                Err(Error::Storage(
                    "injected auth metadata write failure".to_owned(),
                ))
            } else {
                self.inner.set_data(key, value)
            }
        }

        fn delete_data(&self, key: &str) -> Result<bool> {
            self.inner.delete_data(key)
        }

        fn repack(&self) -> hass_core::Result<()> {
            self.inner.repack()
        }

        fn history_query(
            &self,
            query: &hass_core::HistoryQuery,
        ) -> hass_core::Result<Vec<hass_core::HistorySeries>> {
            self.inner.history_query(query)
        }
        fn event_history_query(
            &self,
            query: &hass_core::EventHistoryQuery,
        ) -> hass_core::Result<Vec<hass_core::Event>> {
            self.inner.event_history_query(query)
        }
        fn purge_before(
            &self,
            cutoff: chrono::DateTime<chrono::Utc>,
        ) -> hass_core::Result<hass_core::PurgeResult> {
            self.inner.purge_before(cutoff)
        }

        fn history(
            &self,
            entity_id: &str,
            start: chrono::DateTime<chrono::Utc>,
            end: chrono::DateTime<chrono::Utc>,
        ) -> Result<Vec<hass_core::State>> {
            self.inner.history(entity_id, start, end)
        }
    }

    #[test]
    fn malformed_persistent_auth_state_fails_closed() {
        let store = Arc::new(MemoryStore::default());
        store
            .set_data(AUTH_DATA_KEY, &json!({"version":999,"jwt_secret":"bad"}))
            .expect("write malformed state");
        let runtime = Runtime::new(store).expect("runtime");
        assert!(matches!(AuthManager::new(runtime), Err(Error::Storage(_))));
    }

    #[test]
    fn pre_mfa_schema_v1_store_loads_with_mfa_disabled_defaults() {
        let legacy: AuthStore = serde_json::from_value(json!({
            "version": 1,
            "jwt_secret": "a".repeat(64),
            "users": {
                "legacy-user": {
                    "id": "legacy-user",
                    "username": null,
                    "name": "Legacy User",
                    "is_owner": false,
                    "is_active": true,
                    "local_only": null,
                    "system_generated": false,
                    "group_ids": []
                }
            },
            "homeassistant_provider": {"users": {}},
            "sessions": {}
        }))
        .expect("deserialize pre-MFA metadata");
        super::validate_store(&legacy).expect("validate pre-MFA metadata");
        assert!(!legacy.users["legacy-user"].mfa_enabled);
        assert!(legacy.mfa.totp_users.is_empty());
    }
}
