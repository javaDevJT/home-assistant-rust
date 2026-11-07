//! Authenticated HTTP and WebSocket API for the native Home Assistant runtime.
//!
//! Event inventory reports actual public wildcard receivers; integration
//! dispatcher traffic stays private. Consumers apply narrower filters locally.
//!
//! Applications that use Axum graceful shutdown should call [`router_with_shutdown`]
//! and signal its `watch::Receiver` when shutdown begins. Axum waits for active
//! request handlers while draining; this hook closes WebSocket sessions so that
//! an idle client cannot hold that drain open indefinitely.

mod config_entries;
mod extensions;
mod history;
mod http;
mod images;
mod logger;
mod login;
pub mod network;
mod persons;
mod subscriptions;
mod tags;
mod webhooks;
mod websocket;
mod zones;

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

/// Concrete native subsystems exposed through HTTP and WebSocket adapters.
#[derive(Clone, Default)]
pub struct ApiFeatures {
    pub network: network::NetworkPolicy,
    pub auth: Option<hass_auth::AuthManager>,
    pub registries: Option<hass_registry::Registries>,
    pub templates: Option<hass_template::TemplateEngine>,
    pub config_entries: Option<hass_config::ConfigEntries>,
    pub notifications: Option<hass_notifications::NotificationManager>,
    pub zones: Option<hass_zone::ZoneManager>,
    pub persons: Option<hass_person::PersonManager>,
    pub images: Option<hass_image_upload::ImageManager>,
    pub webhooks: Option<hass_webhook::WebhookManager>,
    pub tags: Option<hass_tag::TagManager>,
    pub logger: Option<hass_logger::LoggerManager>,
    pub logger_catalog: Option<hass_logger::IntegrationCatalogProvider>,
    pub system_log: Option<hass_system_log::SystemLogManager>,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) api: ApiState,
    pub(crate) websocket_slots: Arc<Semaphore>,
    pub(crate) shutdown: Option<watch::Receiver<bool>>,
    pub(crate) features: ApiFeatures,
    pub(crate) login: Arc<login::LoginFlows>,
    _service_target_guard: Option<hass_core::ServiceTargetResolverGuard>,
}

/// Build the authenticated HTTP and WebSocket API router.
///
/// The token is checked when requests arrive. An empty configured token causes
/// protected endpoints and WebSocket authentication to be rejected.
pub fn router(state: ApiState) -> Router {
    build_router(state, ApiFeatures::default(), None)
}

/// Build the API router with a signal that closes active WebSocket sessions.
///
/// The receiver should change to `true` (or be dropped) when application
/// shutdown begins. The application can await Axum's graceful shutdown after
/// signaling the same shutdown event.
pub fn router_with_shutdown(state: ApiState, shutdown: watch::Receiver<bool>) -> Router {
    build_router(state, ApiFeatures::default(), Some(shutdown))
}

pub fn router_with_features(
    state: ApiState,
    features: ApiFeatures,
    shutdown: Option<watch::Receiver<bool>>,
) -> Router {
    build_router(state, features, shutdown)
}

fn build_router(
    state: ApiState,
    features: ApiFeatures,
    shutdown: Option<watch::Receiver<bool>>,
) -> Router {
    let registries = features.registries.clone();
    let service_target_guard = state
        .runtime
        .install_service_target_resolver_if_absent(Arc::new(move |runtime, call| {
            Ok(!matches!(
                hass_registry::targets::resolve_entity_service_targets(
                    runtime,
                    registries.as_ref(),
                    call
                )?,
                hass_registry::targets::TargetResolution::NoMatches
            ))
        }));
    let state = Arc::new(AppState {
        api: state,
        websocket_slots: Arc::new(Semaphore::new(MAX_WEBSOCKET_CONNECTIONS)),
        shutdown,
        features,
        login: Arc::new(login::LoginFlows::default()),
        _service_target_guard: service_target_guard,
    });

    let protected_routes = Router::new()
        .route(
            "/api/config/config_entries/entry",
            get(config_entries::get_entries),
        )
        .route(
            "/api/config/config_entries/entry/{entry_id}",
            axum::routing::delete(config_entries::delete_entry),
        )
        .route(
            "/api/config/config_entries/entry/{entry_id}/reload",
            post(config_entries::reload_entry),
        )
        .route(
            "/api/config/config_entries/flow",
            post(config_entries::init_flow),
        )
        .route(
            "/api/config/config_entries/flow/{flow_id}",
            get(config_entries::get_flow)
                .post(config_entries::submit_flow)
                .delete(config_entries::abort_flow),
        )
        .route(
            "/api/config/config_entries/flow_handlers",
            get(config_entries::flow_handlers),
        )
        .route(
            "/api/config/config_entries/options/flow",
            post(config_entries::init_options_flow),
        )
        .route(
            "/api/config/config_entries/options/flow/{flow_id}",
            get(config_entries::get_flow)
                .post(config_entries::submit_flow)
                .delete(config_entries::abort_flow),
        )
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
        .route("/api/template", post(extensions::render_template))
        .route(
            "/api/image/upload",
            post(images::upload).layer(DefaultBodyLimit::max(images::MAX_UPLOAD_BODY)),
        )
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
        .route(
            "/api/webhook/{webhook_id}",
            get(webhooks::handle)
                .post(webhooks::handle)
                .put(webhooks::handle),
        )
        .route("/api/image/serve/{image_id}/{filename}", get(images::serve))
        .route("/api/websocket", get(websocket::upgrade))
        .route("/auth/providers", get(login::providers))
        .route("/auth/login_flow", post(login::create_flow))
        .route(
            "/auth/login_flow/{flow_id}",
            post(login::submit_flow).delete(login::delete_flow),
        )
        .route("/auth/token", post(login::token))
        .route("/api/onboarding", get(login::onboarding))
        .route("/api/onboarding/users", post(login::onboard_user))
        .merge(protected_routes)
        .layer(DefaultBodyLimit::max(MAX_HTTP_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            extensions::resolve_origin,
        ))
        .with_state(state)
}
