//! Exercise standard media services through the real async Core dispatcher.

#[path = "../src/media_player.rs"]
mod native_media;

use async_trait::async_trait;
use hass_core::{Context, Error, MemoryStore, Runtime, ServiceCall, ServiceCallRequest};
use hass_entity::{EntityLifecycle, EntitySpec};
use hass_media_player::{
    BrowseMedia, BrowseMediaRequest, MediaCommand, MediaPlayerDispatcher, MediaPlayerEntitySpec,
    MediaPlayerFeatures, MediaPlayerProvider, MediaPlayerProviderContext, MediaPlayerResult,
    MediaPlayerSnapshot, MediaPlayerState, ProviderCommandOutcome,
};
use hass_registry::Registries;
use serde_json::{Map, Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Player {
    runtime: Runtime,
    commands: AtomicUsize,
    browses: AtomicUsize,
}

#[async_trait]
impl MediaPlayerProvider for Player {
    async fn snapshot(
        &self,
        context: &MediaPlayerProviderContext,
    ) -> MediaPlayerResult<MediaPlayerSnapshot> {
        Ok(MediaPlayerSnapshot {
            state: Some(MediaPlayerState::Paused),
            supported_features: MediaPlayerFeatures::PLAY | MediaPlayerFeatures::BROWSE_MEDIA,
            context: context.context().clone(),
            ..MediaPlayerSnapshot::default()
        })
    }

    async fn execute(
        &self,
        context: &MediaPlayerProviderContext,
        _command: MediaCommand,
    ) -> MediaPlayerResult<ProviderCommandOutcome> {
        self.commands.fetch_add(1, Ordering::SeqCst);
        self.runtime.set_state(
            context.entity_id(),
            "playing",
            Map::new(),
            context.context().clone(),
            false,
        )?;
        Ok(ProviderCommandOutcome::Executed)
    }

    async fn browse(
        &self,
        _context: &MediaPlayerProviderContext,
        _request: BrowseMediaRequest,
    ) -> MediaPlayerResult<BrowseMedia> {
        self.browses.fetch_add(1, Ordering::SeqCst);
        Ok(BrowseMedia::new(
            "Library",
            "directory",
            "library",
            "root",
            false,
            true,
        ))
    }
}

fn call(service: &str, id: &str, data: Map<String, Value>) -> ServiceCall {
    ServiceCall {
        domain: "media_player".into(),
        service: service.into(),
        data,
        target: json!({"entity_id":id}).as_object().unwrap().clone(),
        context: Context {
            user_id: Some("fixture-user".into()),
            ..Context::default()
        },
    }
}

#[tokio::test]
async fn registered_media_services_validate_before_effects_and_preserve_actual_state_context() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let lifecycle = EntityLifecycle::new(runtime.clone(), registries).unwrap();
    let dispatcher = MediaPlayerDispatcher::new(lifecycle.clone());
    let host = native_media::MediaPlayerHost::start(runtime.clone(), dispatcher.clone()).unwrap();
    let player = Arc::new(Player {
        runtime: runtime.clone(),
        commands: AtomicUsize::new(0),
        browses: AtomicUsize::new(0),
    });
    let registration = dispatcher
        .register_provider(
            "fixture-owner",
            player.clone(),
            vec![MediaPlayerEntitySpec::new(
                EntitySpec::new("media_player", "fixture", "fixture-owner", "player")
                    .with_unique_id("fixture-player")
                    .with_suggested_object_id("fixture"),
                MediaPlayerFeatures::PLAY | MediaPlayerFeatures::BROWSE_MEDIA,
            )],
        )
        .unwrap();
    let id = registration.entity_id("player").unwrap();
    let request = call("media_play", &id, Map::new());
    let context = request.context.clone();
    let result = runtime.call_service_async(request).await.unwrap();
    assert_eq!(player.commands.load(Ordering::SeqCst), 1);
    assert_eq!(result.context, context);
    assert_eq!(result.changed_states.len(), 1);
    assert_eq!(result.changed_states[0].state, "playing");
    assert_eq!(result.changed_states[0].context, context);
    let invalid = runtime
        .call_service_async(call(
            "media_play",
            &id,
            json!({"unexpected":true}).as_object().unwrap().clone(),
        ))
        .await;
    assert!(matches!(invalid, Err(Error::ServiceValidation(_))));
    let unsupported = runtime
        .call_service_async(call("media_pause", &id, Map::new()))
        .await;
    assert!(matches!(unsupported, Err(Error::ServiceValidation(_))));
    assert_eq!(player.commands.load(Ordering::SeqCst), 1);

    let denied = runtime
        .call_service_request_async(ServiceCallRequest {
            call: call("browse_media", &id, Map::new()),
            return_response: false,
            authorize_targets: None,
        })
        .await;
    assert!(matches!(denied, Err(Error::ServiceValidation(_))));
    assert_eq!(player.browses.load(Ordering::SeqCst), 0);
    let response = runtime
        .call_service_request_async(ServiceCallRequest {
            call: call("browse_media", &id, Map::new()),
            return_response: true,
            authorize_targets: None,
        })
        .await
        .unwrap();
    assert_eq!(response.response.unwrap()[&id]["title"], "Library");
    assert_eq!(player.browses.load(Ordering::SeqCst), 1);
    registration.unregister().unwrap();
    host.shutdown().unwrap();
    lifecycle.shutdown().await.unwrap();
    runtime.shutdown_service_tasks_async().await.unwrap();
}
