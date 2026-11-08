//! Direct media-player browse/search WebSocket commands.
//!
//! The wire response shapes follow Home Assistant Core at
//! `cbf2c02c9bff331537c19bff69da8dc11c3fbfbf` and the SDK serializers.

use hass_media_player::{BrowseMediaRequest, MediaPlayerError, SearchMediaRequest};
use serde_json::{Map, Value};

use crate::AppState;

/// Safe error data for the existing WebSocket error envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MediaApiError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl MediaApiError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid_format(message: &'static str) -> Self {
        Self::new("invalid_format", message)
    }
}

/// Handle only the two direct media-player browse/search commands.
///
/// The passed command includes the WebSocket `id` and `type` fields. Those,
/// together with `entity_id`, are removed before the SDK validates its strict
/// operation-specific payload. The outer WebSocket dispatcher handles command
/// IDs, authentication, and coarse role policy.
pub(crate) async fn handle_ws(
    state: &AppState,
    command: &Value,
    user_id: Option<String>,
) -> Option<Result<Value, MediaApiError>> {
    let object = command.as_object()?;
    let command_type = object.get("type")?.as_str()?;
    if !matches!(
        command_type,
        "media_player/browse_media" | "media_player/search_media"
    ) {
        return None;
    }

    Some(handle_media_command(state, command_type, object, user_id).await)
}

async fn handle_media_command(
    state: &AppState,
    command_type: &str,
    command: &Map<String, Value>,
    user_id: Option<String>,
) -> Result<Value, MediaApiError> {
    let allowed_fields: &[&str] = match command_type {
        "media_player/browse_media" => &[
            "id",
            "type",
            "entity_id",
            "media_content_type",
            "media_content_id",
        ],
        "media_player/search_media" => &[
            "id",
            "type",
            "entity_id",
            "search_query",
            "media_content_type",
            "media_content_id",
            "media_filter_classes",
        ],
        _ => unreachable!("handle_media_command receives a supported command"),
    };
    if command
        .keys()
        .any(|field| !allowed_fields.contains(&field.as_str()))
    {
        return Err(MediaApiError::invalid_format(
            "Invalid media-player command.",
        ));
    }

    let entity_id = match command.get("entity_id") {
        Some(Value::String(entity_id)) if is_valid_entity_id(entity_id) => entity_id.clone(),
        Some(_) | None => {
            return Err(MediaApiError::invalid_format(
                "Invalid media-player command.",
            ));
        }
    };

    let mut payload = command.clone();
    payload.remove("id");
    payload.remove("type");
    payload.remove("entity_id");

    match command_type {
        "media_player/browse_media" => {
            BrowseMediaRequest::parse_command(&payload).map_err(map_media_error)?;
        }
        "media_player/search_media" => {
            SearchMediaRequest::parse_command(&payload).map_err(map_media_error)?;
        }
        _ => unreachable!("handle_media_command receives a supported command"),
    }

    let context = hass_core::Context {
        user_id,
        ..hass_core::Context::default()
    };
    let operation = async move {
        let Some(dispatcher) = state.features.media_players.clone() else {
            return Err(MediaApiError::new(
                "service_not_found",
                "Media-player support is not enabled.",
            ));
        };

        match command_type {
            "media_player/browse_media" => dispatcher
                .browse_media_with_context(&entity_id, &payload, context)
                .await
                .map(|response| response.to_service_response())
                .map_err(map_media_error),
            "media_player/search_media" => dispatcher
                .search_media_with_context(&entity_id, &payload, context)
                .await
                .map(|response| response.to_service_response())
                .map_err(map_media_error),
            _ => unreachable!("handle_media_command receives a supported command"),
        }
    };

    if let Some(mut shutdown) = state.shutdown.clone() {
        tokio::select! {
            biased;
            _ = crate::oauth2::wait_for_shutdown(&mut shutdown) => {
                Err(MediaApiError::new(
                    "home_assistant_error",
                    "API is shutting down.",
                ))
            }
            result = operation => result,
        }
    } else {
        operation.await
    }
}

fn is_valid_entity_id(value: &str) -> bool {
    let Some((domain, object_id)) = value.split_once('.') else {
        return false;
    };
    !domain.is_empty()
        && !object_id.is_empty()
        && !object_id.contains('.')
        && domain
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && object_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn map_media_error(error: MediaPlayerError) -> MediaApiError {
    let code = error.code().as_str();
    let message = match error {
        MediaPlayerError::InvalidFormat(_) => "Invalid media-player command.".to_owned(),
        MediaPlayerError::UnknownService(_) => "Media-player service was not found.".to_owned(),
        MediaPlayerError::EntityNotFound => "Entity not found".to_owned(),
        MediaPlayerError::NotSupported(message)
            if message == "Player does not support browsing media" =>
        {
            "Player does not support browsing media".to_owned()
        }
        MediaPlayerError::NotSupported(message)
            if message == "Player does not support searching media" =>
        {
            "Player does not support searching media".to_owned()
        }
        MediaPlayerError::NotSupported(_) => "Media-player operation is not supported.".to_owned(),
        MediaPlayerError::NotImplemented(service) => match service {
            "browse_media" => "Integration does not support browsing media".to_owned(),
            "search_media" => "Integration does not support searching media".to_owned(),
            _ => "Media-player operation is not supported.".to_owned(),
        },
        MediaPlayerError::Cancelled => "Media-player provider was unloaded.".to_owned(),
        MediaPlayerError::Provider(_) | MediaPlayerError::Lifecycle(_) => {
            "Media-player request failed.".to_owned()
        }
    };
    MediaApiError::new(code, message)
}
