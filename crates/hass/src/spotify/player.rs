//! Spotify config-entry driver, cached media-player provider, and poll owner.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use hass_application_credentials::{ApplicationCredentialsPlatform, AuthorizationServer};
use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult,
    SetupOutcome,
};
use hass_core::{Context, Error as CoreError, Result as CoreResult, Runtime};
use hass_entity::EntitySpec;
use hass_media_player::{
    BrowseMedia, BrowseMediaRequest, MediaCommand, MediaPlayerCancellation, MediaPlayerDispatcher,
    MediaPlayerEntitySpec, MediaPlayerError, MediaPlayerProvider, MediaPlayerProviderContext,
    MediaPlayerResult, MediaPlayerSnapshot, ProviderCommandOutcome, SearchMedia,
    SearchMediaRequest,
};
use hass_oauth2_client::{ClientAuthenticationMethod, OAuthClientConfig};
use hass_oauth2_flow::{
    OAuthEntryDetails, OAuthFlowError, OAuthFlowManager, OAuthFlowProvider,
    OAuthPostAuthorizationContext,
};
use hass_registry::{DeviceEntry, DeviceRegistration, ProviderEntityInfo, Registries};
use hass_spotify::{
    AuthorizationError, Device, PlaybackState, RequestAuthorizer, SpotifyClient, SpotifyError,
    UserProfile,
};
use reqwest::{Request, RequestBuilder};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};
use url::Url;

use super::{DOMAIN, SpotifyOptions, account, browse, control};

const PLAYER_KEY: &str = "media_player";
const PLAYER_OWNER_PREFIX: &str = "spotify:";

#[derive(Clone)]
struct PlayerCache {
    profile: UserProfile,
    playback: Option<PlaybackState>,
    devices: Vec<Device>,
    playlist_name: Option<String>,
    playlist_context_uri: Option<String>,
    playlist_retry_at: Option<Instant>,
    fetched_at: DateTime<Utc>,
    playback_ok: bool,
    devices_ok: bool,
}

struct PlaylistContextUpdate {
    uri: Option<String>,
    name: Option<String>,
    retry_at: Option<Instant>,
    unauthorized: bool,
}

impl PlaylistContextUpdate {
    fn empty() -> Self {
        Self {
            uri: None,
            name: None,
            retry_at: None,
            unauthorized: false,
        }
    }
}

impl PlayerCache {
    fn available(&self) -> bool {
        self.playback_ok && self.devices_ok
    }
}

struct ActiveEntry {
    owner_id: String,
    registration: hass_media_player::MediaPlayerRegistration,
    poll_task: JoinHandle<()>,
}

/// Strong integration state. Drivers and callbacks hold only weak references
/// so config entries cannot keep the integration host alive.
pub(super) struct SpotifyState {
    pub(super) handle: Handle,
    entries: Weak<ConfigEntries>,
    registries: Registries,
    oauth: OAuthFlowManager,
    media: MediaPlayerDispatcher,
    options: SpotifyOptions,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    alive: AtomicBool,
    generation: AtomicU64,
    active: Mutex<HashMap<String, ActiveEntry>>,
    setup_in_flight: Mutex<HashSet<String>>,
    reauth_started: Mutex<HashSet<String>>,
    reauth_flows: Mutex<HashMap<String, String>>,
    reauth_tasks: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl SpotifyState {
    pub(super) fn new(
        entries: Weak<ConfigEntries>,
        registries: Registries,
        oauth: OAuthFlowManager,
        media: MediaPlayerDispatcher,
        options: SpotifyOptions,
        shutdown_tx: watch::Sender<bool>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            handle: Handle::current(),
            entries,
            registries,
            oauth,
            media,
            options,
            shutdown_tx,
            shutdown_rx,
            alive: AtomicBool::new(true),
            generation: AtomicU64::new(1),
            active: Mutex::new(HashMap::new()),
            setup_in_flight: Mutex::new(HashSet::new()),
            reauth_started: Mutex::new(HashSet::new()),
            reauth_flows: Mutex::new(HashMap::new()),
            reauth_tasks: Mutex::new(HashMap::new()),
        }
    }

    fn lock_active(&self) -> std::sync::MutexGuard<'_, HashMap<String, ActiveEntry>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_setup_in_flight(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.setup_in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_reauth_started(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.reauth_started
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_reauth_tasks(&self) -> std::sync::MutexGuard<'_, HashMap<String, JoinHandle<()>>> {
        self.reauth_tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn is_stopping(&self) -> bool {
        !self.alive.load(Ordering::Acquire)
            || *self.shutdown_rx.borrow()
            || self.shutdown_rx.has_changed().is_err()
    }

    fn is_generation_live(&self, generation: u64) -> bool {
        !self.is_stopping() && self.generation.load(Ordering::Acquire) == generation
    }

    pub(super) fn close(&self) {
        if self.alive.swap(false, Ordering::AcqRel) {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub(super) fn stop(&self) {
        self.shutdown_tx.send_replace(true);
    }

    pub(super) async fn unload_all_active(&self) -> CoreResult<()> {
        let entry_ids = self.lock_active().keys().cloned().collect::<Vec<_>>();
        let mut errors = Vec::new();
        for entry_id in entry_ids {
            if let Err(error) = self.unload_entry(&entry_id).await {
                errors.push(format!("{entry_id}: {error}"));
            }
        }
        if let Err(error) = self.join_reauth_tasks().await {
            errors.push(error.to_string());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(CoreError::Service(format!(
                "failed to stop Spotify account tasks: {}",
                errors.join("; ")
            )))
        }
    }

    async fn join_reauth_tasks(&self) -> CoreResult<()> {
        let tasks = std::mem::take(&mut *self.lock_reauth_tasks());
        let mut errors = Vec::new();
        for (entry_id, task) in tasks {
            if let Err(error) = task.await {
                errors.push(format!("{entry_id}: {error}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(CoreError::Service(format!(
                "failed to join Spotify reauthentication jobs: {}",
                errors.join("; ")
            )))
        }
    }

    fn setup_sync(
        self: &Arc<Self>,
        runtime: &Runtime,
        entry: &ConfigEntry,
    ) -> CoreResult<SetupOutcome> {
        if self.is_stopping() {
            return Err(CoreError::Service(
                "Spotify integration is shutting down".into(),
            ));
        }
        if self.lock_active().contains_key(&entry.entry_id) {
            return Ok(SetupOutcome::Ready);
        }
        {
            let mut in_flight = self.lock_setup_in_flight();
            if !in_flight.insert(entry.entry_id.clone()) {
                return Ok(SetupOutcome::RetryAfter {
                    seconds: 1,
                    reason: Some("Spotify entry setup is already in progress".into()),
                });
            }
        }
        let _setup_guard = SetupInFlightGuard {
            state: Arc::downgrade(self),
            entry_id: entry.entry_id.clone(),
        };
        let generation = self.generation.load(Ordering::Acquire);
        match self
            .handle
            .block_on(self.setup_entry(runtime.clone(), entry.clone(), generation))
        {
            Ok(()) => Ok(SetupOutcome::Ready),
            Err(SetupFailure::RetryAfter { seconds, reason }) => Ok(SetupOutcome::RetryAfter {
                seconds,
                reason: Some(reason),
            }),
            Err(SetupFailure::Fatal(error)) => Err(error),
        }
    }

    async fn setup_entry(
        self: &Arc<Self>,
        runtime: Runtime,
        entry: ConfigEntry,
        generation: u64,
    ) -> Result<(), SetupFailure> {
        self.ensure_live_generation(generation)?;
        let session = self
            .oauth
            .session_for_entry(DOMAIN, &entry.entry_id)
            .await
            .map_err(|error| {
                SetupFailure::Fatal(CoreError::Service(format!(
                    "Spotify OAuth session is unavailable: {error}"
                )))
            })?;
        self.ensure_live_generation(generation)?;

        let client = SpotifyClient::with_options(
            self.options.api_base_url.clone(),
            Arc::new(hass_spotify::OAuthSessionAuthorizer::new(session)),
            self.options.client_options.clone(),
        )
        .map_err(|error| SetupFailure::Fatal(CoreError::Service(error.to_string())))?;

        let profile = match self
            .read_during_setup(generation, client.get_current_user())
            .await
        {
            Ok(profile) => profile,
            Err(SetupReadError::Stopped) => return Err(SetupFailure::stopping()),
            Err(SetupReadError::Spotify(error)) => {
                return Err(self.setup_api_failure(&entry, error).await);
            }
        };
        let entries = self.entries.upgrade().ok_or_else(SetupFailure::stopping)?;
        let identity_entries = entries.list_entries(Some(DOMAIN));
        if let Err(error) =
            account::entry_details(&profile, "reauth", Some(&entry), &identity_entries)
        {
            return Err(SetupFailure::Fatal(match error {
                OAuthFlowError::ProviderAbort(reason) => CoreError::InvalidInput(format!(
                    "Spotify account identity check failed: {reason}"
                )),
                other => CoreError::Service(format!(
                    "Spotify account identity could not be checked: {other}"
                )),
            }));
        }
        self.ensure_live_generation(generation)?;

        let playback = match self
            .read_during_setup(generation, client.get_playback())
            .await
        {
            Ok(playback) => playback,
            Err(SetupReadError::Stopped) => return Err(SetupFailure::stopping()),
            Err(SetupReadError::Spotify(error)) => {
                return Err(self.setup_api_failure(&entry, error).await);
            }
        };
        self.ensure_live_generation(generation)?;

        let devices = match self
            .read_during_setup(generation, client.get_devices())
            .await
        {
            Ok(devices) => devices,
            Err(SetupReadError::Stopped) => return Err(SetupFailure::stopping()),
            Err(SetupReadError::Spotify(error)) => {
                return Err(self.setup_api_failure(&entry, error).await);
            }
        };
        self.ensure_live_generation(generation)?;

        let identity = stable_entry_identity(&entry, &profile).ok_or_else(|| {
            SetupFailure::Fatal(CoreError::InvalidInput(
                "Spotify profile has no stable account identity".into(),
            ))
        })?;
        let device = self.register_account_device(&entry, &profile, &identity)?;
        let playlist = self
            .initial_playlist_context(&client, playback.as_ref(), generation)
            .await;
        if playlist.unauthorized {
            self.begin_reauth_once(&entry.entry_id);
            return Err(SetupFailure::RetryAfter {
                seconds: 60,
                reason: "Spotify authorization requires reauthentication".into(),
            });
        }
        self.ensure_live_generation(generation)?;

        let fetched_at = Utc::now();
        let player = Arc::new(SpotifyPlayer {
            entry_id: entry.entry_id.clone(),
            owner_id: owner_for_entry(&entry.entry_id),
            legacy_user_id: entry
                .data
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| profile.id.clone()),
            client,
            cache: RwLock::new(PlayerCache {
                profile,
                playback,
                devices,
                playlist_name: playlist.name,
                playlist_context_uri: playlist.uri,
                playlist_retry_at: playlist.retry_at,
                fetched_at,
                playback_ok: true,
                devices_ok: true,
            }),
            operation_gate: AsyncMutex::new(()),
            state: Arc::downgrade(self),
            generation,
            media: self.media.clone(),
            command_refresh_delay: self.options.command_refresh_delay,
        });
        let initial = player.cached_snapshot(Context::default());
        let entity = entity_spec(&entry, &device, &identity, initial.supported_features);
        let registration = self
            .media
            .register_provider(
                player.owner_id.clone(),
                Arc::clone(&player) as Arc<dyn MediaPlayerProvider>,
                vec![entity],
            )
            .map_err(|error| {
                SetupFailure::Fatal(CoreError::Service(format!(
                    "register Spotify media player: {error}"
                )))
            })?;
        if !self.is_generation_live(generation) {
            registration.unregister().ok();
            return Err(SetupFailure::stopping());
        }
        if let Err(error) = self
            .media
            .refresh_snapshot_with_context(&player.owner_id, PLAYER_KEY, Context::default())
            .await
        {
            registration.unregister().ok();
            return Err(SetupFailure::Fatal(CoreError::Service(format!(
                "publish initial Spotify player state: {error}"
            ))));
        }
        if !self.is_generation_live(generation) {
            registration.unregister().ok();
            return Err(SetupFailure::stopping());
        }

        let cancellation = registration.cancellation();
        let poll_player = Arc::clone(&player);
        let poll_task = tokio::spawn(async move {
            run_poll_task(poll_player, cancellation).await;
        });
        {
            let mut active = self.lock_active();
            if self.is_generation_live(generation) && !active.contains_key(&entry.entry_id) {
                active.insert(
                    entry.entry_id.clone(),
                    ActiveEntry {
                        owner_id: player.owner_id.clone(),
                        registration,
                        poll_task,
                    },
                );
                drop(active);
                self.lock_reauth_started().remove(&entry.entry_id);
                runtime
                    .loaded_components()
                    .record_successful_component("spotify.media_player");
                return Ok(());
            }
        }
        registration.unregister().ok();
        poll_task.abort();
        let _ = poll_task.await;
        Err(SetupFailure::stopping())
    }

    async fn read_during_setup<T>(
        &self,
        generation: u64,
        future: impl Future<Output = Result<T, SpotifyError>>,
    ) -> Result<T, SetupReadError> {
        tokio::select! {
            _ = wait_for_stop_owned(self.shutdown_rx.clone()) => Err(SetupReadError::Stopped),
            result = future => {
                if self.is_generation_live(generation) {
                    result.map_err(SetupReadError::Spotify)
                } else {
                    Err(SetupReadError::Stopped)
                }
            }
        }
    }

    fn ensure_live_generation(&self, generation: u64) -> Result<(), SetupFailure> {
        if self.is_generation_live(generation) {
            Ok(())
        } else {
            Err(SetupFailure::stopping())
        }
    }

    async fn setup_api_failure(
        self: &Arc<Self>,
        entry: &ConfigEntry,
        error: SpotifyError,
    ) -> SetupFailure {
        if is_unauthorized(&error) {
            self.begin_reauth_once(&entry.entry_id);
            return SetupFailure::RetryAfter {
                seconds: 60,
                reason: "Spotify authorization requires reauthentication".into(),
            };
        }
        if let Some(seconds) =
            retry_seconds(&error, entry.retry_attempts, self.options.playback_interval)
        {
            return SetupFailure::RetryAfter {
                seconds,
                reason: error.to_string(),
            };
        }
        SetupFailure::Fatal(CoreError::Service(format!("Spotify setup failed: {error}")))
    }

    fn register_account_device(
        &self,
        entry: &ConfigEntry,
        profile: &UserProfile,
        identity: &str,
    ) -> Result<DeviceEntry, SetupFailure> {
        let model = profile
            .product
            .as_deref()
            .filter(|product| !product.trim().is_empty())
            .map(|product| format!("Spotify {product}"));
        self.registries
            .get_or_create_device(DeviceRegistration {
                entry_type: Some("service".into()),
                config_entry_id: Some(entry.entry_id.clone()),
                identifiers: BTreeSet::from([(DOMAIN.to_owned(), identity.to_owned())]),
                name: Some("Spotify AB".into()),
                manufacturer: Some("Spotify AB".into()),
                model,
                configuration_url: Some("https://open.spotify.com".into()),
                ..DeviceRegistration::default()
            })
            .map_err(|error| {
                SetupFailure::Fatal(CoreError::Service(format!(
                    "register Spotify account device: {error}"
                )))
            })
    }

    async fn initial_playlist_context(
        &self,
        client: &SpotifyClient,
        playback: Option<&PlaybackState>,
        generation: u64,
    ) -> PlaylistContextUpdate {
        let Some(context) = playback.and_then(|playback| playback.context.as_ref()) else {
            return PlaylistContextUpdate::empty();
        };
        let Some(uri) = context.uri.as_deref().filter(|uri| !uri.is_empty()) else {
            return PlaylistContextUpdate::empty();
        };
        if is_dj_context(context.context_type.as_deref(), uri) {
            return PlaylistContextUpdate {
                uri: Some(uri.to_owned()),
                name: Some("DJ".to_owned()),
                ..PlaylistContextUpdate::empty()
            };
        }
        let Some(playlist_id) = playlist_id(uri) else {
            return PlaylistContextUpdate {
                uri: Some(uri.to_owned()),
                ..PlaylistContextUpdate::empty()
            };
        };
        let result = tokio::select! {
            _ = wait_for_stop_owned(self.shutdown_rx.clone()) => return PlaylistContextUpdate::empty(),
            result = client.get_playlist(playlist_id) => result,
        };
        if !self.is_generation_live(generation) {
            return PlaylistContextUpdate::empty();
        }
        match result {
            Ok(playlist) => PlaylistContextUpdate {
                uri: Some(uri.to_owned()),
                name: playlist.name,
                ..PlaylistContextUpdate::empty()
            },
            Err(SpotifyError::NotFound) => PlaylistContextUpdate {
                uri: Some(uri.to_owned()),
                ..PlaylistContextUpdate::empty()
            },
            Err(error) if is_unauthorized(&error) => PlaylistContextUpdate {
                unauthorized: true,
                ..PlaylistContextUpdate::empty()
            },
            Err(error) if matches!(error, SpotifyError::RateLimited { .. }) => {
                tracing::debug!(%error, "Spotify playlist context is rate limited during setup");
                PlaylistContextUpdate {
                    retry_at: Some(
                        Instant::now() + retry_delay(&error, 1, self.options.playback_interval),
                    ),
                    ..PlaylistContextUpdate::empty()
                }
            }
            Err(error) => {
                tracing::debug!(%error, "Spotify playlist context metadata is unavailable during setup");
                PlaylistContextUpdate::empty()
            }
        }
    }

    fn begin_reauth_once(self: &Arc<Self>, entry_id: &str) {
        if self.is_stopping() {
            return;
        }
        {
            let mut started = self.lock_reauth_started();
            if !started.insert(entry_id.to_owned()) {
                return;
            }
        }
        let entry_id = entry_id.to_owned();
        let manager = self.oauth.clone();
        let weak_state = Arc::downgrade(self);
        let task_entry_id = entry_id.clone();
        let previous = self.lock_reauth_tasks().remove(&entry_id);
        let guard = ReauthInFlightGuard {
            state: Arc::downgrade(self),
            entry_id: entry_id.clone(),
        };
        let task = tokio::spawn(async move {
            let _guard = guard;
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let Some(state) = weak_state.upgrade() else {
                return;
            };
            if state.is_stopping() {
                return;
            }
            let existing_flow = state
                .reauth_flows
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&task_entry_id)
                .cloned();
            drop(state);
            if let Some(flow_id) = existing_flow
                && manager.flow_is_active(&flow_id).await
            {
                return;
            }
            match manager.begin_reauth(&task_entry_id).await {
                Ok(result) => {
                    if let Some(state) = weak_state.upgrade()
                        && let Some(flow_id) = result.get("flow_id").and_then(Value::as_str)
                    {
                        state
                            .reauth_flows
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(task_entry_id.clone(), flow_id.to_owned());
                    }
                    tracing::debug!(entry_id = %task_entry_id, "Spotify reauthentication flow started");
                }
                Err(error) => {
                    tracing::debug!(entry_id = %task_entry_id, %error, "Spotify reauthentication could not be started");
                }
            }
        });
        self.lock_reauth_tasks().insert(entry_id, task);
    }

    async fn unload_entry(&self, entry_id: &str) -> CoreResult<()> {
        let Some(active) = self.lock_active().remove(entry_id) else {
            return Ok(());
        };
        let unregister_error = active
            .registration
            .unregister()
            .err()
            .map(|error| format!("unregister Spotify player {}: {error}", active.owner_id));
        let join_error = active
            .poll_task
            .await
            .err()
            .map(|error| format!("join Spotify poll task for {entry_id}: {error}"));
        match (unregister_error, join_error) {
            (None, None) => Ok(()),
            (Some(error), None) | (None, Some(error)) => Err(CoreError::Service(error)),
            (Some(unregister), Some(join)) => {
                Err(CoreError::Service(format!("{unregister}; {join}")))
            }
        }
    }

    fn remove_account_device(&self, entry: &ConfigEntry) -> CoreResult<()> {
        let identity = entry
            .unique_id
            .as_deref()
            .or_else(|| entry.data.get("account_id").and_then(Value::as_str))
            .or_else(|| entry.data.get("id").and_then(Value::as_str));
        let Some(identity) = identity else {
            return Ok(());
        };
        let device_ids = self
            .registries
            .devices()
            .into_iter()
            .filter(|device| {
                device
                    .identifiers
                    .contains(&(DOMAIN.to_owned(), identity.to_owned()))
                    && device.config_entry_id.as_deref() == Some(entry.entry_id.as_str())
            })
            .map(|device| device.id)
            .collect::<Vec<_>>();
        for device_id in device_ids {
            if self.registries.device(&device_id).is_some() {
                self.registries
                    .remove_config_entry_from_device(&device_id, &entry.entry_id)?;
            }
        }
        Ok(())
    }
}

struct ReauthInFlightGuard {
    state: Weak<SpotifyState>,
    entry_id: String,
}

impl Drop for ReauthInFlightGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state.lock_reauth_started().remove(&self.entry_id);
        }
    }
}

struct SetupInFlightGuard {
    state: Weak<SpotifyState>,
    entry_id: String,
}

impl Drop for SetupInFlightGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state.lock_setup_in_flight().remove(&self.entry_id);
        }
    }
}

enum SetupReadError {
    Spotify(SpotifyError),
    Stopped,
}

enum SetupFailure {
    RetryAfter { seconds: u64, reason: String },
    Fatal(CoreError),
}

impl SetupFailure {
    fn stopping() -> Self {
        Self::Fatal(CoreError::Service(
            "Spotify setup was cancelled during shutdown".into(),
        ))
    }
}

pub(super) struct SpotifyConfigEntryDriver {
    state: Weak<SpotifyState>,
    options: SpotifyOptions,
}

impl SpotifyConfigEntryDriver {
    pub(super) fn new(state: Weak<SpotifyState>) -> Self {
        let options = state
            .upgrade()
            .map(|state| state.options.clone())
            .unwrap_or_default();
        Self { state, options }
    }

    fn upgraded(&self) -> CoreResult<Arc<SpotifyState>> {
        self.state.upgrade().ok_or_else(|| {
            CoreError::Service("Spotify integration driver is no longer active".into())
        })
    }
}

impl ConfigEntryDriver for SpotifyConfigEntryDriver {
    fn domain(&self) -> &str {
        DOMAIN
    }

    fn integration_type(&self) -> &str {
        "service"
    }

    fn supports_reconfigure(&self) -> bool {
        true
    }

    fn version(&self) -> (u32, u32) {
        (1, 1)
    }

    fn setup(&self, runtime: &Runtime, entry: &ConfigEntry) -> CoreResult<SetupOutcome> {
        self.upgraded()?.setup_sync(runtime, entry)
    }

    fn unload(&self, _runtime: &Runtime, entry: &ConfigEntry) -> CoreResult<()> {
        let Ok(state) = self.upgraded() else {
            return Ok(());
        };
        state.handle.block_on(state.unload_entry(&entry.entry_id))
    }

    fn config_flow(&self, _request: &ConfigFlowRequest) -> CoreResult<ConfigFlowResult> {
        Err(CoreError::InvalidInput(
            "Spotify config flows are owned by the native OAuth host".into(),
        ))
    }

    fn remove(&self, _runtime: &Runtime, entry: &ConfigEntry) -> CoreResult<()> {
        let Ok(state) = self.upgraded() else {
            return Ok(());
        };
        state.handle.block_on(state.unload_entry(&entry.entry_id))?;
        state.remove_account_device(entry)
    }
}

impl OAuthFlowProvider for SpotifyConfigEntryDriver {
    fn application_credentials_platform(&self) -> ApplicationCredentialsPlatform {
        let authorize_url = self.options.authorize_url.as_str().to_owned();
        let token_url = self.options.token_url.as_str().to_owned();
        ApplicationCredentialsPlatform::new().with_authorization_server_factory(move |_| {
            let authorize_url = authorize_url.clone();
            let token_url = token_url.clone();
            Box::pin(async move {
                Ok(AuthorizationServer {
                    authorize_url,
                    token_url,
                })
            })
        })
    }

    fn oauth_scopes(&self, _request: &ConfigFlowRequest) -> Vec<String> {
        [
            "user-modify-playback-state",
            "user-read-playback-state",
            "user-read-private",
            "playlist-read-private",
            "playlist-read-collaborative",
            "user-library-read",
            "user-top-read",
            "user-read-playback-position",
            "user-read-recently-played",
            "user-follow-read",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    fn oauth_client_config(
        &self,
        implementation: &hass_application_credentials::LocalOAuth2Implementation,
    ) -> Result<OAuthClientConfig, OAuthFlowError> {
        let authorize = Url::parse(&implementation.authorization_server.authorize_url)
            .map_err(|_| OAuthFlowError::InvalidConfiguration)?;
        let token = Url::parse(&implementation.authorization_server.token_url)
            .map_err(|_| OAuthFlowError::InvalidConfiguration)?;
        OAuthClientConfig::new(
            implementation.client_id.clone(),
            Some(implementation.client_secret().to_owned()),
            authorize,
            token,
        )
        .map(|config| {
            config.with_authentication_method(ClientAuthenticationMethod::ClientSecretBasic)
        })
        .map_err(Into::into)
    }

    fn post_authorize<'a>(
        &'a self,
        context: OAuthPostAuthorizationContext,
    ) -> BoxFuture<'a, Result<OAuthEntryDetails, OAuthFlowError>> {
        Box::pin(async move {
            let state = self.state.upgrade().ok_or(OAuthFlowError::ShuttingDown)?;
            if state.is_stopping() {
                return Err(OAuthFlowError::ShuttingDown);
            }
            let generation = state.generation.load(Ordering::Acquire);
            let client = SpotifyClient::with_options(
                state.options.api_base_url.clone(),
                Arc::new(FailClosedAuthorizer),
                state.options.client_options.clone(),
            )
            .map_err(|_| OAuthFlowError::ProviderFailure)?;
            let profile = tokio::select! {
                _ = wait_for_stop_owned(state.shutdown_rx.clone()) => {
                    return Err(OAuthFlowError::ShuttingDown);
                }
                result = client.get_current_user_with_token(&context.token) => {
                    result.map_err(|_| OAuthFlowError::ProviderFailure)?
                }
            };
            if !state.is_generation_live(generation) {
                return Err(OAuthFlowError::ShuttingDown);
            }
            let entries = state
                .entries
                .upgrade()
                .ok_or(OAuthFlowError::ShuttingDown)?;
            account::entry_details(
                &profile,
                &context.request.source,
                context.existing_entry.as_ref(),
                &entries.list_entries(Some(DOMAIN)),
            )
        })
    }
}

/// The post-authorization client can only use the one-shot token method. Its
/// general request path fails closed, so it can never synthesize authorization.
struct FailClosedAuthorizer;

#[async_trait]
impl RequestAuthorizer for FailClosedAuthorizer {
    async fn authorize(&self, _request: RequestBuilder) -> Result<Request, AuthorizationError> {
        Err(AuthorizationError::InvalidRequest)
    }
}

struct SpotifyPlayer {
    entry_id: String,
    owner_id: String,
    legacy_user_id: Option<String>,
    client: SpotifyClient,
    cache: RwLock<PlayerCache>,
    operation_gate: AsyncMutex<()>,
    state: Weak<SpotifyState>,
    generation: u64,
    media: MediaPlayerDispatcher,
    command_refresh_delay: Duration,
}

impl SpotifyPlayer {
    fn lock_cache(&self) -> std::sync::RwLockReadGuard<'_, PlayerCache> {
        self.cache.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_cache(&self) -> std::sync::RwLockWriteGuard<'_, PlayerCache> {
        self.cache.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn state(&self) -> Option<Arc<SpotifyState>> {
        self.state.upgrade()
    }

    fn is_live(&self) -> bool {
        self.state()
            .is_some_and(|state| state.is_generation_live(self.generation))
    }

    fn cached_snapshot(&self, context: Context) -> MediaPlayerSnapshot {
        let cache = self.lock_cache().clone();
        let mut snapshot = super::super::spotify_projection::snapshot(
            &cache.profile,
            cache.playback.as_ref(),
            &cache.devices,
            cache.playlist_name.as_deref(),
            cache.fetched_at,
            context,
        );
        snapshot.available = cache.available();
        snapshot
    }

    async fn api_playback(
        &self,
        cancellation: Option<&MediaPlayerCancellation>,
    ) -> Result<Option<PlaybackState>, SpotifyError> {
        let _gate = self.operation_gate.lock().await;
        if !self.is_live() || cancellation.is_some_and(MediaPlayerCancellation::is_cancelled) {
            return Err(SpotifyError::Authorization(AuthorizationError::Unavailable));
        }
        self.client.get_playback().await
    }

    async fn api_devices(
        &self,
        cancellation: Option<&MediaPlayerCancellation>,
    ) -> Result<Vec<Device>, SpotifyError> {
        let _gate = self.operation_gate.lock().await;
        if !self.is_live() || cancellation.is_some_and(MediaPlayerCancellation::is_cancelled) {
            return Err(SpotifyError::Authorization(AuthorizationError::Unavailable));
        }
        self.client.get_devices().await
    }

    async fn update_playlist_context(
        &self,
        playback: Option<&PlaybackState>,
        cancellation: Option<&MediaPlayerCancellation>,
    ) -> PlaylistContextUpdate {
        let Some(context) = playback.and_then(|playback| playback.context.as_ref()) else {
            return PlaylistContextUpdate::empty();
        };
        let Some(uri) = context.uri.as_deref().filter(|uri| !uri.is_empty()) else {
            return PlaylistContextUpdate::empty();
        };
        {
            let cache = self.lock_cache();
            if cache.playlist_context_uri.as_deref() == Some(uri) {
                return PlaylistContextUpdate {
                    uri: cache.playlist_context_uri.clone(),
                    name: cache.playlist_name.clone(),
                    retry_at: cache.playlist_retry_at,
                    unauthorized: false,
                };
            }
        }
        if is_dj_context(context.context_type.as_deref(), uri) {
            return PlaylistContextUpdate {
                uri: Some(uri.to_owned()),
                name: Some("DJ".to_owned()),
                ..PlaylistContextUpdate::empty()
            };
        }
        let Some(playlist_id) = playlist_id(uri) else {
            return PlaylistContextUpdate {
                uri: Some(uri.to_owned()),
                ..PlaylistContextUpdate::empty()
            };
        };
        let retry_at = self.lock_cache().playlist_retry_at;
        if retry_at.is_some_and(|retry_at| retry_at > Instant::now()) {
            return PlaylistContextUpdate {
                retry_at,
                ..PlaylistContextUpdate::empty()
            };
        }
        let result = tokio::select! {
            _ = async {
                if let Some(cancellation) = cancellation {
                    cancellation.cancelled().await;
                } else if let Some(state) = self.state() {
                    wait_for_stop_owned(state.shutdown_rx.clone()).await;
                }
            } => return PlaylistContextUpdate::empty(),
            result = async {
                let _gate = self.operation_gate.lock().await;
                self.client.get_playlist(playlist_id).await
            } => result,
        };
        match result {
            Ok(playlist) => PlaylistContextUpdate {
                uri: Some(uri.to_owned()),
                name: playlist.name,
                ..PlaylistContextUpdate::empty()
            },
            Err(SpotifyError::NotFound) => PlaylistContextUpdate {
                uri: Some(uri.to_owned()),
                ..PlaylistContextUpdate::empty()
            },
            Err(error) if is_unauthorized(&error) => PlaylistContextUpdate {
                unauthorized: true,
                ..PlaylistContextUpdate::empty()
            },
            Err(error) if matches!(error, SpotifyError::RateLimited { .. }) => {
                tracing::debug!(entry_id = %self.entry_id, %error, "Spotify playlist context is rate limited");
                PlaylistContextUpdate {
                    retry_at: Some(
                        Instant::now()
                            + retry_delay(
                                &error,
                                1,
                                self.state().map_or(Duration::from_secs(30), |state| {
                                    state.options.playback_interval
                                }),
                            ),
                    ),
                    ..PlaylistContextUpdate::empty()
                }
            }
            Err(error) => {
                tracing::debug!(entry_id = %self.entry_id, %error, "Spotify playlist context metadata is unavailable");
                PlaylistContextUpdate::empty()
            }
        }
    }

    fn record_playback_success(
        &self,
        playback: Option<PlaybackState>,
        playlist: PlaylistContextUpdate,
    ) {
        let unauthorized = playlist.unauthorized;
        {
            let mut cache = self.write_cache();
            cache.playback = playback;
            cache.playlist_context_uri = playlist.uri;
            cache.playlist_name = playlist.name;
            cache.playlist_retry_at = playlist.retry_at;
            cache.fetched_at = Utc::now();
            cache.playback_ok = !unauthorized;
        }
        if unauthorized && let Some(state) = self.state() {
            state.begin_reauth_once(&self.entry_id);
        }
    }

    fn record_devices_success(&self, devices: Vec<Device>) {
        let mut cache = self.write_cache();
        cache.devices = devices;
        cache.fetched_at = Utc::now();
        cache.devices_ok = true;
    }

    fn record_playback_failure(&self) {
        self.write_cache().playback_ok = false;
    }

    fn record_devices_failure(&self) {
        self.write_cache().devices_ok = false;
    }

    async fn publish(&self, context: Context) {
        if !self.is_live() {
            return;
        }
        if let Err(error) = self
            .media
            .refresh_snapshot_with_context(&self.owner_id, PLAYER_KEY, context)
            .await
            && !matches!(
                error,
                MediaPlayerError::Cancelled | MediaPlayerError::EntityNotFound
            )
        {
            tracing::debug!(entry_id = %self.entry_id, %error, "Spotify cached state publication failed");
        }
    }

    async fn mark_unauthorized(&self, context: &MediaPlayerProviderContext) {
        self.record_playback_failure();
        if let Some(state) = self.state() {
            state.begin_reauth_once(&self.entry_id);
        }
        self.publish(context.context().clone()).await;
    }

    async fn refresh_playback_after_write(&self, context: &MediaPlayerProviderContext) {
        if tokio::select! {
            _ = context.cancelled() => true,
            _ = sleep(self.command_refresh_delay) => false,
        } || !self.is_live()
        {
            return;
        }
        let result = tokio::select! {
            _ = context.cancelled() => return,
            result = self.api_playback(None) => result,
        };
        let mut unauthorized_refresh = false;
        match result {
            Ok(playback) => {
                let playlist = tokio::select! {
                    _ = context.cancelled() => return,
                    result = self.update_playlist_context(playback.as_ref(), None) => result,
                };
                if !self.is_live() || context.is_cancelled() {
                    return;
                }
                self.record_playback_success(playback, playlist);
            }
            Err(error) if is_unauthorized(&error) => {
                self.record_playback_failure();
                unauthorized_refresh = true;
            }
            Err(error) => {
                self.record_playback_failure();
                tracing::debug!(entry_id = %self.entry_id, %error, "Spotify playback refresh after command failed");
            }
        }
        if !self.is_live() || context.is_cancelled() {
            return;
        }
        if let Err(error) = self
            .media
            .refresh_snapshot_with_context(&self.owner_id, PLAYER_KEY, context.context().clone())
            .await
            && !matches!(
                error,
                MediaPlayerError::Cancelled | MediaPlayerError::EntityNotFound
            )
        {
            tracing::debug!(entry_id = %self.entry_id, %error, "Spotify post-command snapshot publication failed");
        }
        if unauthorized_refresh
            && let Some(state) = self.state()
            && !context.is_cancelled()
            && self.is_live()
        {
            state.begin_reauth_once(&self.entry_id);
        }
    }
}

#[async_trait]
impl MediaPlayerProvider for SpotifyPlayer {
    async fn snapshot(
        &self,
        context: &MediaPlayerProviderContext,
    ) -> MediaPlayerResult<MediaPlayerSnapshot> {
        if !self.is_live() || context.is_cancelled() {
            return Err(MediaPlayerError::Cancelled);
        }
        Ok(self.cached_snapshot(context.context().clone()))
    }

    async fn execute(
        &self,
        context: &MediaPlayerProviderContext,
        command: MediaCommand,
    ) -> MediaPlayerResult<ProviderCommandOutcome> {
        if !self.is_live() || context.is_cancelled() {
            return Err(MediaPlayerError::Cancelled);
        }
        let cache = self.lock_cache().clone();
        let result = {
            let _gate = tokio::select! {
                _ = context.cancelled() => return Err(MediaPlayerError::Cancelled),
                guard = self.operation_gate.lock() => guard,
            };
            if !self.is_live() || context.is_cancelled() {
                return Err(MediaPlayerError::Cancelled);
            }
            control::execute(
                &self.client,
                command,
                &cache.devices,
                cache.playback.as_ref(),
                self.legacy_user_id.as_deref(),
            )
            .await
        };
        match result {
            Ok(outcome) => {
                // The pinned refresh-after decorator runs after every
                // successful call, including an unknown-source no-op.
                self.refresh_playback_after_write(context).await;
                Ok(outcome)
            }
            Err(error) if is_unauthorized(&error) => {
                self.mark_unauthorized(context).await;
                Err(MediaPlayerError::Provider(error.to_string()))
            }
            Err(error) => Err(MediaPlayerError::Provider(error.to_string())),
        }
    }

    async fn browse(
        &self,
        context: &MediaPlayerProviderContext,
        request: BrowseMediaRequest,
    ) -> MediaPlayerResult<BrowseMedia> {
        if !self.is_live() || context.is_cancelled() {
            return Err(MediaPlayerError::Cancelled);
        }
        let cache = self.lock_cache().clone();
        let result = tokio::select! {
            _ = context.cancelled() => return Err(MediaPlayerError::Cancelled),
            result = async {
                let _gate = self.operation_gate.lock().await;
                browse::browse(&self.client, &cache.profile, request).await
            } => result,
        };
        result.map_err(|error| MediaPlayerError::Provider(error.to_string()))
    }

    async fn search(
        &self,
        _context: &MediaPlayerProviderContext,
        _request: SearchMediaRequest,
    ) -> MediaPlayerResult<SearchMedia> {
        Err(MediaPlayerError::NotImplemented("search_media"))
    }
}

async fn run_poll_task(player: Arc<SpotifyPlayer>, cancellation: MediaPlayerCancellation) {
    let Some(state) = player.state() else {
        return;
    };
    let options = state.options.clone();
    let shutdown_rx = state.shutdown_rx.clone();
    drop(state);
    let mut playback_deadline = Instant::now() + options.playback_interval;
    let mut device_deadline = Instant::now() + options.device_interval;
    let mut playback_failures = 0_u32;
    let mut device_failures = 0_u32;
    loop {
        if !player.is_live() || cancellation.is_cancelled() {
            break;
        }
        let next_deadline = playback_deadline.min(device_deadline);
        tokio::select! {
            _ = cancellation.cancelled() => break,
            _ = wait_for_stop_owned(shutdown_rx.clone()) => break,
            _ = sleep_until(next_deadline) => {}
        }
        if !player.is_live() || cancellation.is_cancelled() {
            break;
        }

        let now = Instant::now();
        if playback_deadline <= now {
            let result = tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = wait_for_stop_owned(shutdown_rx.clone()) => break,
                result = player.api_playback(Some(&cancellation)) => result,
            };
            match result {
                Ok(playback) => {
                    playback_failures = 0;
                    let playlist = player
                        .update_playlist_context(playback.as_ref(), Some(&cancellation))
                        .await;
                    if !player.is_live() || cancellation.is_cancelled() {
                        break;
                    }
                    player.record_playback_success(playback.clone(), playlist);
                    let next_delay =
                        playback_poll_delay(playback.as_ref(), options.playback_interval);
                    playback_deadline = Instant::now() + next_delay;
                }
                Err(error) if is_unauthorized(&error) => {
                    playback_failures = playback_failures.saturating_add(1);
                    player.record_playback_failure();
                    if let Some(state) = player.state() {
                        state.begin_reauth_once(&player.entry_id);
                    }
                    playback_deadline = Instant::now() + Duration::from_secs(60 * 60);
                }
                Err(error) => {
                    playback_failures = playback_failures.saturating_add(1);
                    player.record_playback_failure();
                    playback_deadline = Instant::now()
                        + retry_delay(&error, playback_failures, options.playback_interval);
                    tracing::debug!(entry_id = %player.entry_id, %error, "Spotify playback poll failed");
                }
            }
            if !player.is_live() || cancellation.is_cancelled() {
                break;
            }
            player.publish(Context::default()).await;
        }

        if device_deadline <= Instant::now() {
            let result = tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = wait_for_stop_owned(shutdown_rx.clone()) => break,
                result = player.api_devices(Some(&cancellation)) => result,
            };
            match result {
                Ok(devices) => {
                    device_failures = 0;
                    player.record_devices_success(devices);
                    device_deadline = Instant::now() + options.device_interval;
                }
                Err(error) if is_unauthorized(&error) => {
                    device_failures = device_failures.saturating_add(1);
                    player.record_devices_failure();
                    if let Some(state) = player.state() {
                        state.begin_reauth_once(&player.entry_id);
                    }
                    device_deadline = Instant::now() + Duration::from_secs(60 * 60);
                }
                Err(error) => {
                    device_failures = device_failures.saturating_add(1);
                    player.record_devices_failure();
                    device_deadline = Instant::now()
                        + retry_delay(&error, device_failures, options.device_interval);
                    tracing::debug!(entry_id = %player.entry_id, %error, "Spotify device poll failed");
                }
            }
            if !player.is_live() || cancellation.is_cancelled() {
                break;
            }
            player.publish(Context::default()).await;
        }
    }
}

fn entity_spec(
    entry: &ConfigEntry,
    device: &DeviceEntry,
    identity: &str,
    features: hass_media_player::MediaPlayerFeatures,
) -> MediaPlayerEntitySpec {
    let info = ProviderEntityInfo {
        has_entity_name: true,
        supported_features: features.bits(),
        ..ProviderEntityInfo::default()
    };
    let spec = EntitySpec::new(
        "media_player",
        DOMAIN,
        owner_for_entry(&entry.entry_id),
        PLAYER_KEY,
    )
    .with_unique_id(identity.to_owned())
    .with_config_entry_id(entry.entry_id.clone())
    .with_device_id(device.id.clone())
    .with_suggested_object_id("spotify")
    .with_info(info);
    MediaPlayerEntitySpec::new(spec, features)
}

fn stable_entry_identity(entry: &ConfigEntry, profile: &UserProfile) -> Option<String> {
    entry
        .unique_id
        .clone()
        .or_else(|| {
            entry
                .data
                .get("account_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| {
            entry
                .data
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| profile.account_id.clone())
        .or_else(|| profile.id.clone())
        .filter(|value| !value.trim().is_empty())
}

fn owner_for_entry(entry_id: &str) -> String {
    format!("{PLAYER_OWNER_PREFIX}{entry_id}")
}

fn playlist_id(uri: &str) -> Option<&str> {
    uri.strip_prefix("spotify:playlist:")
        .filter(|id| !id.is_empty())
}

fn is_dj_context(context_type: Option<&str>, uri: &str) -> bool {
    context_type.is_some_and(|kind| kind.eq_ignore_ascii_case("dj"))
        || uri.to_ascii_lowercase().contains(":dj:")
        || uri.to_ascii_lowercase().ends_with(":dj")
}

fn playback_poll_delay(playback: Option<&PlaybackState>, normal: Duration) -> Duration {
    let Some(playback) = playback.filter(|playback| playback.is_playing == Some(true)) else {
        return normal;
    };
    let Some(item) = playback.item.as_ref() else {
        return normal;
    };
    let (Some(duration_ms), Some(progress_ms)) = (item.duration_ms, playback.progress_ms) else {
        return normal;
    };
    let remaining_ms = duration_ms.saturating_sub(progress_ms);
    if remaining_ms < normal.as_millis().min(u128::from(u64::MAX)) as u64 {
        Duration::from_millis(remaining_ms.saturating_add(1_000).max(1))
    } else {
        normal
    }
}

fn is_unauthorized(error: &SpotifyError) -> bool {
    matches!(
        error,
        SpotifyError::Unauthorized
            | SpotifyError::Authorization(AuthorizationError::ReauthenticationRequired)
    )
}

fn retry_seconds(error: &SpotifyError, attempts: u32, base: Duration) -> Option<u64> {
    if is_unauthorized(error) || is_retryable(error) {
        Some(retry_delay(error, attempts, base).as_secs().max(1))
    } else {
        None
    }
}

fn is_retryable(error: &SpotifyError) -> bool {
    matches!(
        error,
        SpotifyError::RateLimited { .. }
            | SpotifyError::Transient { .. }
            | SpotifyError::Transport
            | SpotifyError::Timeout
            | SpotifyError::Authorization(AuthorizationError::Unavailable)
            | SpotifyError::Authorization(AuthorizationError::Storage)
    )
}

fn retry_delay(error: &SpotifyError, attempts: u32, base: Duration) -> Duration {
    if let SpotifyError::RateLimited {
        retry_after: Some(retry_after),
        ..
    } = error
    {
        return (*retry_after).clamp(Duration::from_secs(1), Duration::from_secs(60 * 60));
    }
    let exponent = attempts.saturating_sub(1).min(8);
    let factor = 1_u32 << exponent;
    base.saturating_mul(factor)
        .clamp(Duration::from_secs(1), Duration::from_secs(5 * 60))
}

async fn wait_for_stop_owned(mut receiver: watch::Receiver<bool>) {
    loop {
        if *receiver.borrow_and_update() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use hass_spotify::SpotifyObject;

    #[test]
    fn playback_poll_uses_near_track_end_interval_only_while_playing() {
        let normal = Duration::from_secs(30);
        let playback = PlaybackState {
            is_playing: Some(true),
            progress_ms: Some(89_000),
            item: Some(SpotifyObject {
                duration_ms: Some(90_000),
                ..SpotifyObject::default()
            }),
            ..PlaybackState::default()
        };
        assert_eq!(
            playback_poll_delay(Some(&playback), normal),
            Duration::from_secs(2)
        );
        let paused = PlaybackState {
            is_playing: Some(false),
            ..playback
        };
        assert_eq!(playback_poll_delay(Some(&paused), normal), normal);
        assert_eq!(playback_poll_delay(None, normal), normal);
    }

    #[test]
    fn retry_after_is_bounded_and_honored() {
        let error = SpotifyError::RateLimited {
            retry_after: Some(Duration::from_secs(7)),
            reason: hass_spotify::RateLimitReason::Unknown,
        };
        assert_eq!(
            retry_delay(&error, 4, Duration::from_secs(30)),
            Duration::from_secs(7)
        );
        let too_long = SpotifyError::RateLimited {
            retry_after: Some(Duration::from_secs(8 * 60 * 60)),
            reason: hass_spotify::RateLimitReason::Unknown,
        };
        assert_eq!(
            retry_delay(&too_long, 4, Duration::from_secs(30)),
            Duration::from_secs(60 * 60)
        );
    }
}
