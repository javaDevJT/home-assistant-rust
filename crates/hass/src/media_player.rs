//! Standard media-player services backed by the native async dispatcher.

use hass_core::{
    Context, Error, Result, Runtime, ServiceCall, ServiceResponseSupport, ServiceResult, State,
};
use hass_media_player::{MEDIA_PLAYER_SERVICES, MediaPlayerDispatcher, MediaPlayerError};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, sync::Arc};

pub(crate) struct MediaPlayerHost {
    runtime: Runtime,
    dispatcher: Arc<MediaPlayerDispatcher>,
    owner: String,
}

impl MediaPlayerHost {
    pub(crate) fn start(runtime: Runtime, dispatcher: MediaPlayerDispatcher) -> Result<Self> {
        let host = Self {
            runtime,
            dispatcher: Arc::new(dispatcher),
            owner: format!("media-player-{}", uuid::Uuid::new_v4()),
        };
        for &service in MEDIA_PLAYER_SERVICES {
            let matcher = Arc::downgrade(&host.dispatcher);
            let handler = Arc::downgrade(&host.dispatcher);
            let response_only = matches!(service, "browse_media" | "search_media");
            let support = if response_only {
                ServiceResponseSupport::Only
            } else {
                ServiceResponseSupport::None
            };
            let mut description = json!({"name": service.replace('_', " "), "target":{"entity":{"domain":"media_player"}}});
            if response_only {
                description["supports_response"] = json!("only");
            }
            host.runtime
                .register_entity_async_service_with_response_support(
                    &host.owner,
                    "media_player",
                    service,
                    description,
                    support,
                    Arc::new(move |id| {
                        matcher
                            .upgrade()
                            .is_some_and(|dispatcher| dispatcher.owns_entity(id))
                    }),
                    Arc::new(move |runtime, call| {
                        let handler = handler.clone();
                        Box::pin(async move {
                            let dispatcher = handler.upgrade().ok_or_else(|| {
                                Error::Service("Media players are stopped".into())
                            })?;
                            dispatch(&runtime, &dispatcher, call).await
                        })
                    }),
                )?;
        }
        Ok(host)
    }

    pub(crate) fn shutdown(self) -> Result<()> {
        self.runtime.unregister_entity_services(&self.owner)?;
        Ok(())
    }
}

impl Drop for MediaPlayerHost {
    fn drop(&mut self) {
        let _ = self.runtime.unregister_entity_services(&self.owner);
    }
}

async fn dispatch(
    runtime: &Runtime,
    dispatcher: &MediaPlayerDispatcher,
    call: ServiceCall,
) -> Result<ServiceResult> {
    let ids = match call.target.get("entity_id") {
        Some(Value::String(id)) => vec![id.clone()],
        Some(Value::Array(ids)) => ids
            .iter()
            .map(|id| {
                id.as_str().map(str::to_owned).ok_or_else(|| {
                    Error::ServiceValidation("Invalid media-player entity target".into())
                })
            })
            .collect::<Result<Vec<_>>>()?,
        _ => {
            return Err(Error::ServiceValidation(
                "Media-player service requires an entity target".into(),
            ));
        }
    };
    let before = runtime
        .states()
        .into_iter()
        .map(|state| (state.entity_id.clone(), state))
        .collect::<BTreeMap<_, _>>();
    let outcome = dispatcher
        .dispatch_service_with_context(&call.service, &call.data, &ids, call.context.clone())
        .await
        .map_err(map_error)?;
    let response = if matches!(call.service.as_str(), "browse_media" | "search_media") {
        let mut responses = Map::new();
        for response in outcome.responses {
            responses.insert(
                response.entity_id().to_owned(),
                response.to_service_response(),
            );
        }
        Some(Value::Object(responses))
    } else {
        None
    };
    Ok(ServiceResult {
        changed_states: changes_since(runtime, &before, &call.context),
        response,
        context: call.context,
    })
}

fn changes_since(
    runtime: &Runtime,
    before: &BTreeMap<String, State>,
    context: &Context,
) -> Vec<State> {
    runtime
        .states()
        .into_iter()
        .filter(|state| {
            state.context.id == context.id
                && before.get(&state.entity_id).is_none_or(|old| {
                    old.state != state.state
                        || old.attributes != state.attributes
                        || old.last_updated != state.last_updated
                })
        })
        .collect()
}

fn map_error(error: MediaPlayerError) -> Error {
    match error {
        MediaPlayerError::InvalidFormat(message) | MediaPlayerError::NotSupported(message) => {
            Error::ServiceValidation(message)
        }
        MediaPlayerError::NotImplemented(_) => {
            Error::ServiceValidation("The media player does not support this operation".into())
        }
        MediaPlayerError::EntityNotFound => {
            Error::ServiceValidation("Media-player entity was not found".into())
        }
        MediaPlayerError::UnknownService(_) => {
            Error::ServiceValidation("Unknown media-player service".into())
        }
        MediaPlayerError::Cancelled => Error::Service("Media-player provider was unloaded".into()),
        MediaPlayerError::Provider(_) => Error::Service("Media-player provider failed".into()),
        MediaPlayerError::Lifecycle(error) => error,
    }
}
