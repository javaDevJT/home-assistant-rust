//! Native Spotify provider integration.
//!
//! `SpotifyIntegration` owns the OAuth provider registration, config-entry
//! restoration and retry worker. Each loaded entry owns one account player and
//! its joined polling task. The media-player dispatcher owns entity lifecycle
//! publication; provider snapshots only read the current cache.

mod account;
mod browse;
mod control;
mod player;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use hass_config::{ConfigEntries, ConfigEntryState};
use hass_core::{Error as CoreError, Result as CoreResult, Runtime};
use hass_media_player::MediaPlayerDispatcher;
use hass_oauth2_flow::{OAuthFlowHost, OAuthFlowProvider, OAuthFlowRegistrationGuard};
use hass_registry::Registries;
use hass_spotify::{DEFAULT_API_BASE_URL, SpotifyClientOptions};
use tokio::task::JoinHandle;
use tokio::time::{self, MissedTickBehavior};
use url::Url;

use player::{SpotifyConfigEntryDriver, SpotifyState};

pub(crate) const DOMAIN: &str = "spotify";
const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const RETRY_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Trusted construction options for native startup and loopback fixtures.
/// These values are never read from YAML, config-flow input, or a service call.
#[derive(Clone, Debug)]
pub(crate) struct SpotifyOptions {
    authorize_url: Url,
    token_url: Url,
    api_base_url: Url,
    client_options: SpotifyClientOptions,
    playback_interval: Duration,
    device_interval: Duration,
    command_refresh_delay: Duration,
}

impl Default for SpotifyOptions {
    fn default() -> Self {
        Self {
            authorize_url: Url::parse(AUTHORIZE_URL).expect("fixed Spotify authorize URL"),
            token_url: Url::parse(TOKEN_URL).expect("fixed Spotify token URL"),
            api_base_url: Url::parse(DEFAULT_API_BASE_URL).expect("fixed Spotify API URL"),
            client_options: SpotifyClientOptions::default(),
            playback_interval: Duration::from_secs(30),
            device_interval: Duration::from_secs(5 * 60),
            command_refresh_delay: Duration::from_secs(1),
        }
    }
}

impl SpotifyOptions {
    fn validate(&self) -> CoreResult<()> {
        if self.playback_interval.is_zero()
            || self.device_interval.is_zero()
            || self.command_refresh_delay > Duration::from_secs(30)
            || self.playback_interval > Duration::from_secs(24 * 60 * 60)
            || self.device_interval > Duration::from_secs(24 * 60 * 60)
        {
            return Err(CoreError::InvalidInput(
                "Spotify runtime intervals are outside the supported bounds".into(),
            ));
        }
        for (label, endpoint) in [
            ("authorization", &self.authorize_url),
            ("token", &self.token_url),
        ] {
            let loopback_http = endpoint.scheme() == "http"
                && matches!(endpoint.host_str(), Some("127.0.0.1" | "::1"));
            if endpoint.scheme() != "https" && !loopback_http {
                return Err(CoreError::InvalidInput(format!(
                    "Spotify {label} endpoint must use HTTPS or a numeric loopback address"
                )));
            }
        }
        Ok(())
    }
}

/// Owns the real Spotify OAuth provider, restored config entries, and their
/// background polling and retry work.
pub(crate) struct SpotifyIntegration {
    entries: Arc<ConfigEntries>,
    state: Arc<SpotifyState>,
    registration: Option<OAuthFlowRegistrationGuard>,
    retry_stop: Option<tokio::sync::watch::Sender<bool>>,
    retry_task: Option<JoinHandle<()>>,
    restore_task: Option<JoinHandle<()>>,
    cleanup_started: bool,
}

impl SpotifyIntegration {
    /// Register Spotify and restore existing entries on the blocking pool.
    pub(crate) async fn start(
        runtime: Runtime,
        entries: ConfigEntries,
        registries: Registries,
        oauth_host: &OAuthFlowHost,
        media: MediaPlayerDispatcher,
    ) -> CoreResult<Self> {
        Self::start_with_options(
            runtime,
            entries,
            registries,
            oauth_host,
            media,
            SpotifyOptions::default(),
        )
        .await
    }

    /// Register Spotify with trusted options, primarily for loopback fixtures.
    pub(crate) async fn start_with_options(
        _runtime: Runtime,
        entries: ConfigEntries,
        registries: Registries,
        oauth_host: &OAuthFlowHost,
        media: MediaPlayerDispatcher,
        options: SpotifyOptions,
    ) -> CoreResult<Self> {
        options.validate()?;
        let entries = Arc::new(entries);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let state = Arc::new(SpotifyState::new(
            Arc::downgrade(&entries),
            registries,
            oauth_host.manager(),
            media,
            options,
            shutdown_tx,
            shutdown_rx,
        ));
        let provider = Arc::new(SpotifyConfigEntryDriver::new(Arc::downgrade(&state)));
        let registration = oauth_host
            .register_provider(Arc::clone(&provider) as Arc<dyn OAuthFlowProvider>)
            .map_err(|error| {
                CoreError::Service(format!("register Spotify OAuth provider: {error}"))
            })?;

        // Install the owner before the first suspension so canceled startup
        // still closes admission and joins restoration before unregistering.
        let mut integration = Self {
            entries,
            state,
            registration: Some(registration),
            retry_stop: None,
            retry_task: None,
            restore_task: None,
            cleanup_started: false,
        };
        let entries_for_restore = Arc::clone(&integration.entries);
        let state_for_restore = Arc::clone(&integration.state);
        integration.restore_task = Some(tokio::task::spawn_blocking(move || {
            restore_spotify_entries(&entries_for_restore, &state_for_restore);
        }));
        let restore_result = integration
            .restore_task
            .as_mut()
            .expect("restore worker")
            .await;
        integration.restore_task.take();
        if let Err(error) = restore_result {
            let _ = integration.shutdown().await;
            return Err(CoreError::Service(format!(
                "join restored Spotify config entries: {error}"
            )));
        }

        let (retry_stop_tx, retry_stop_rx) = tokio::sync::watch::channel(false);
        let retry_entries = Arc::clone(&integration.entries);
        let retry_state = Arc::clone(&integration.state);
        let retry_task = tokio::spawn(async move {
            retry_scheduler(retry_entries, retry_state, retry_stop_rx).await;
        });

        integration.retry_stop = Some(retry_stop_tx);
        integration.retry_task = Some(retry_task);
        Ok(integration)
    }

    /// Close admission, unload every entry, join polling and retry work, then
    /// unregister the provider after OAuth resolvers have drained.
    pub(crate) async fn shutdown(mut self) -> CoreResult<()> {
        self.begin_cleanup()
            .expect("Spotify cleanup has one owner")
            .await
            .map_err(|error| CoreError::Service(format!("join Spotify cleanup: {error}")))?
    }

    fn begin_cleanup(&mut self) -> Option<JoinHandle<CoreResult<()>>> {
        if self.cleanup_started {
            return None;
        }
        self.cleanup_started = true;
        self.state.close();
        self.state.stop();
        if let Some(stop) = self.retry_stop.take() {
            stop.send_replace(true);
        }
        let restore_task = self.restore_task.take();
        let retry_task = self.retry_task.take();
        let entries = Arc::clone(&self.entries);
        let state = Arc::clone(&self.state);
        let registration = self.registration.take();
        // The runtime retains this coordinator even when its caller drops
        // the JoinHandle. It owns every worker join and the provider lease.
        Some(self.state.handle.spawn(async move {
            let mut errors = Vec::new();
            for (name, task) in [("restore", restore_task), ("retry", retry_task)] {
                if let Some(task) = task
                    && let Err(error) = task.await
                {
                    errors.push(format!("join Spotify {name} worker: {error}"));
                }
            }
            let unload_result =
                tokio::task::spawn_blocking(move || cleanup_active_entries(&entries, &state)).await;
            match unload_result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error.to_string()),
                Err(error) => errors.push(format!("join Spotify shutdown worker: {error}")),
            }
            if let Some(registration) = registration {
                registration.unregister().await;
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(CoreError::Service(errors.join("; ")))
            }
        }))
    }
}

impl Drop for SpotifyIntegration {
    fn drop(&mut self) {
        if let Some(task) = self.begin_cleanup() {
            drop(task);
        }
    }
}

fn restore_spotify_entries(entries: &ConfigEntries, state: &SpotifyState) {
    for entry in entries.list_entries(Some(DOMAIN)) {
        if state.is_stopping()
            || entry.disabled_by.is_some()
            || entry.state == ConfigEntryState::Loaded
        {
            continue;
        }
        if entry.state == ConfigEntryState::SetupRetry {
            if entry
                .retry_at
                .is_some_and(|retry_at| retry_at <= chrono::Utc::now())
                && let Err(error) = entries.retry_entry(&entry.entry_id)
            {
                tracing::warn!(entry_id = %entry.entry_id, error = %error, "Spotify config entry retry did not complete");
            }
            continue;
        }
        if let Err(error) = entries.setup_entry(&entry.entry_id) {
            tracing::warn!(entry_id = %entry.entry_id, error = %error, "Spotify config entry setup did not complete");
        }
    }
}

async fn retry_scheduler(
    entries: Arc<ConfigEntries>,
    state: Arc<SpotifyState>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut interval = time::interval(RETRY_POLL_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = wait_for_stop(&mut stop) => break,
            _ = interval.tick() => {
                if state.is_stopping() {
                    break;
                }
                let entries = Arc::clone(&entries);
                let state_for_tick = Arc::clone(&state);
                if let Err(error) = tokio::task::spawn_blocking(move || {
                    for entry in entries.list_entries(Some(DOMAIN)) {
                        if state_for_tick.is_stopping() {
                            break;
                        }
                        if entry.disabled_by.is_none()
                            && entry.state == ConfigEntryState::SetupRetry
                            && entry.retry_at.is_some_and(|retry_at| retry_at <= chrono::Utc::now())
                            && let Err(error) = entries.retry_entry(&entry.entry_id)
                        {
                            tracing::warn!(entry_id = %entry.entry_id, error = %error, "Spotify config entry retry failed");
                        }
                    }
                }).await {
                    tracing::warn!(%error, "Spotify retry worker join failed");
                }
            }
        }
    }
}

async fn wait_for_stop(receiver: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *receiver.borrow_and_update() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

fn cleanup_active_entries(entries: &ConfigEntries, state: &SpotifyState) -> CoreResult<()> {
    let mut errors = Vec::new();
    for entry in entries.list_entries(Some(DOMAIN)) {
        if entry.state == ConfigEntryState::Loaded
            && let Err(error) = entries.unload_entry(&entry.entry_id)
        {
            errors.push(format!("{}: {error}", entry.entry_id));
        }
    }
    if let Err(error) = state.handle.block_on(state.unload_all_active()) {
        errors.push(error.to_string());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(CoreError::Service(format!(
            "failed to unload Spotify config entries: {}",
            errors.join("; ")
        )))
    }
}
