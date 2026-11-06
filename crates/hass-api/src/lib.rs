//! Authenticated HTTP and WebSocket API for the native Home Assistant runtime.
//!
//! The listener inventory endpoint (`GET /api/events`) returns HTTP 501 because
//! `hass-core` does not expose the event-bus listener registry. Event firing and
//! WebSocket event subscriptions remain available.
//!
//! Applications that use Axum graceful shutdown should call [`router_with_shutdown`]
//! and signal its `watch::Receiver` when shutdown begins. Axum waits for active
//! request handlers while draining; this hook closes WebSocket sessions so that
//! an idle client cannot hold that drain open indefinitely.

mod http;
mod websocket;

use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};
use hass_core::Runtime;
use serde_json::Value;
use tokio::sync::{Semaphore, watch};

const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;
const MAX_WEBSOCKET_CONNECTIONS: usize = 128;

/// Shared API configuration supplied by the application.
#[derive(Clone)]
pub struct ApiState {
    pub runtime: Runtime,
    pub access_token: String,
    pub config: Value,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) api: ApiState,
    pub(crate) websocket_slots: Arc<Semaphore>,
    pub(crate) shutdown: Option<watch::Receiver<bool>>,
}

/// Build the authenticated HTTP and WebSocket API router.
///
/// The token is checked when requests arrive. An empty configured token causes
/// protected endpoints and WebSocket authentication to be rejected.
pub fn router(state: ApiState) -> Router {
    build_router(state, None)
}

/// Build the API router with a signal that closes active WebSocket sessions.
///
/// The receiver should change to `true` (or be dropped) when application
/// shutdown begins. The application can await Axum's graceful shutdown after
/// signaling the same shutdown event.
pub fn router_with_shutdown(state: ApiState, shutdown: watch::Receiver<bool>) -> Router {
    build_router(state, Some(shutdown))
}

fn build_router(state: ApiState, shutdown: Option<watch::Receiver<bool>>) -> Router {
    let state = Arc::new(AppState {
        api: state,
        websocket_slots: Arc::new(Semaphore::new(MAX_WEBSOCKET_CONNECTIONS)),
        shutdown,
    });

    let protected_routes = Router::new()
        .route("/api/", get(http::api_root))
        .route("/api/config", get(http::get_config))
        .route("/api/states", get(http::get_states))
        .route(
            "/api/states/{entity_id}",
            get(http::get_state)
                .post(http::set_state)
                .delete(http::delete_state),
        )
        .route("/api/services", get(http::get_services))
        .route("/api/services/{domain}/{service}", post(http::call_service))
        .route("/api/events", get(http::get_events))
        .route("/api/events/{event_type}", post(http::fire_event))
        .route("/api/history/period", get(http::get_history))
        .route(
            "/api/history/period/{timestamp}",
            get(http::get_history_from),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            http::authenticate,
        ));

    Router::new()
        .route("/health", get(http::health))
        .route("/api/websocket", get(websocket::upgrade))
        .merge(protected_routes)
        .layer(DefaultBodyLimit::max(MAX_HTTP_BODY_BYTES))
        .with_state(state)
}
