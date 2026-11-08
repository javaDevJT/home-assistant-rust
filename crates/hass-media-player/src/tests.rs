use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use hass_core::{MemoryStore, Runtime};
use hass_entity::{EntityLifecycle, EntitySpec};
use hass_registry::{EntityUpdate, Registries};
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, oneshot};

use super::*;

#[test]
fn pinned_feature_bits_state_values_and_gate_groups_match_contract() {
    assert_eq!(MediaPlayerFeatures::PAUSE.bits(), 1);
    assert_eq!(MediaPlayerFeatures::SEEK.bits(), 2);
    assert_eq!(MediaPlayerFeatures::VOLUME_SET.bits(), 4);
    assert_eq!(MediaPlayerFeatures::VOLUME_MUTE.bits(), 8);
    assert_eq!(MediaPlayerFeatures::PREVIOUS_TRACK.bits(), 16);
    assert_eq!(MediaPlayerFeatures::NEXT_TRACK.bits(), 32);
    assert_eq!(MediaPlayerFeatures::TURN_ON.bits(), 128);
    assert_eq!(MediaPlayerFeatures::TURN_OFF.bits(), 256);
    assert_eq!(MediaPlayerFeatures::PLAY_MEDIA.bits(), 512);
    assert_eq!(MediaPlayerFeatures::VOLUME_STEP.bits(), 1024);
    assert_eq!(MediaPlayerFeatures::SELECT_SOURCE.bits(), 2048);
    assert_eq!(MediaPlayerFeatures::STOP.bits(), 4096);
    assert_eq!(MediaPlayerFeatures::CLEAR_PLAYLIST.bits(), 8192);
    assert_eq!(MediaPlayerFeatures::PLAY.bits(), 16384);
    assert_eq!(MediaPlayerFeatures::SHUFFLE_SET.bits(), 32768);
    assert_eq!(MediaPlayerFeatures::SELECT_SOUND_MODE.bits(), 65536);
    assert_eq!(MediaPlayerFeatures::BROWSE_MEDIA.bits(), 131072);
    assert_eq!(MediaPlayerFeatures::REPEAT_SET.bits(), 262144);
    assert_eq!(MediaPlayerFeatures::GROUPING.bits(), 524288);
    assert_eq!(MediaPlayerFeatures::MEDIA_ANNOUNCE.bits(), 1048576);
    assert_eq!(MediaPlayerFeatures::MEDIA_ENQUEUE.bits(), 2097152);
    assert_eq!(MediaPlayerFeatures::SEARCH_MEDIA.bits(), 4194304);

    assert_eq!(MediaPlayerState::Off.as_str(), "off");
    assert_eq!(MediaPlayerState::On.as_str(), "on");
    assert_eq!(MediaPlayerState::Idle.as_str(), "idle");
    assert_eq!(MediaPlayerState::Playing.as_str(), "playing");
    assert_eq!(MediaPlayerState::Paused.as_str(), "paused");
    assert_eq!(MediaPlayerState::Buffering.as_str(), "buffering");

    let volume_up = MediaCommand::parse_service("volume_up", &Map::new()).unwrap();
    assert_eq!(
        volume_up.feature_requirements().alternatives(),
        &[
            MediaPlayerFeatures::VOLUME_SET,
            MediaPlayerFeatures::VOLUME_STEP
        ]
    );
    assert!(volume_up.is_supported_by(MediaPlayerFeatures::VOLUME_SET));
    assert!(volume_up.is_supported_by(MediaPlayerFeatures::VOLUME_STEP));
    assert!(!volume_up.is_supported_by(MediaPlayerFeatures::NONE));

    let toggle = MediaCommand::parse_service("toggle", &Map::new()).unwrap();
    assert!(!toggle.is_supported_by(MediaPlayerFeatures::TURN_ON));
    assert!(!toggle.is_supported_by(MediaPlayerFeatures::TURN_OFF));
    assert!(toggle.is_supported_by(MediaPlayerFeatures::TURN_ON | MediaPlayerFeatures::TURN_OFF));

    let play_pause = MediaCommand::parse_service("media_play_pause", &Map::new()).unwrap();
    assert!(!play_pause.is_supported_by(MediaPlayerFeatures::PLAY));
    assert!(!play_pause.is_supported_by(MediaPlayerFeatures::PAUSE));
    assert!(play_pause.is_supported_by(MediaPlayerFeatures::PLAY | MediaPlayerFeatures::PAUSE));

    // The pinned runtime omits a service gate for browse_media even though
    // YAML metadata and the direct WS command require BROWSE_MEDIA.
    let browse = MediaCommand::parse_service("browse_media", &Map::new()).unwrap();
    assert!(browse.is_supported_by(MediaPlayerFeatures::NONE));
    assert_eq!(
        browse.target_metadata_requirements().alternatives(),
        &[MediaPlayerFeatures::BROWSE_MEDIA]
    );
}

#[test]
fn command_parser_validates_required_types_ranges_and_legacy_media_fields() {
    let volume =
        MediaCommand::parse_service("volume_set", &object(json!({"volume_level": 0.75}))).unwrap();
    assert!(matches!(
        volume.kind,
        MediaCommandKind::VolumeSet { volume_level } if volume_level == 0.75
    ));
    assert!(
        MediaCommand::parse_service("volume_set", &object(json!({"volume_level": -0.01})),)
            .is_err()
    );
    assert!(
        MediaCommand::parse_service("volume_set", &object(json!({"volume_level": 1.01})),).is_err()
    );
    assert!(
        MediaCommand::parse_service("volume_set", &object(json!({"volume_level": "0.5"})),)
            .is_err()
    );
    assert!(MediaCommand::parse_service("volume_set", &Map::new()).is_err());

    assert!(
        MediaCommand::parse_service("media_seek", &object(json!({"seek_position": -1})),).is_err()
    );
    assert!(
        MediaCommand::parse_service("media_seek", &object(json!({"seek_position": "12"})),)
            .is_err()
    );
    assert!(
        MediaCommand::parse_service("repeat_set", &object(json!({"repeat": "context"})),).is_err()
    );
    assert!(MediaCommand::parse_service("shuffle_set", &object(json!({"shuffle": 1})),).is_err());

    let play = MediaCommand::parse_service(
        "play_media",
        &object(json!({
            "media_content_type": "music",
            "media_content_id": "spotify:track:fixture",
            "enqueue": false,
            "extra": {"device_id": "cached-device"}
        })),
    )
    .unwrap();
    let MediaCommandKind::PlayMedia(play) = play.kind else {
        panic!("expected play_media command");
    };
    assert_eq!(play.enqueue, Some(MediaEnqueue::Play));
    assert_eq!(play.media_content_type, "music");
    assert_eq!(play.media_content_id, "spotify:track:fixture");
    assert_eq!(play.extra["device_id"], "cached-device");

    let promoted = MediaCommand::parse_service(
        "play_media",
        &object(json!({
            "media": {"media_content_type": "podcast", "media_content_id": "episode"},
            "enqueue": "add"
        })),
    )
    .unwrap();
    assert!(matches!(
        promoted.kind,
        MediaCommandKind::PlayMedia(PlayMediaCommand {
            enqueue: Some(MediaEnqueue::Add),
            ..
        })
    ));
    assert!(
        MediaCommand::parse_service(
            "play_media",
            &object(json!({
                "media": {"media_content_type": "music", "media_content_id": "x"},
                "media_content_type": "music",
                "media_content_id": "x"
            })),
        )
        .is_err()
    );
    assert!(
        MediaCommand::parse_service(
            "play_media",
            &object(json!({
                "media": {"media_content_type": "music", "media_content_id": "x"},
                "enqueue": "add",
                "announce": false
            })),
        )
        .is_err()
    );

    assert!(
        MediaCommand::parse_service(
            "search_media",
            &object(json!({"search_query": "jazz", "media_filter_classes": ["album", "bad"]})),
        )
        .is_err()
    );
    assert!(
        MediaCommand::parse_service(
            "search_media",
            &object(json!({"media_filter_classes": ["album"]})),
        )
        .is_err()
    );
    assert!(
        MediaCommand::parse_service("join", &object(json!({"group_members": ["invalid-id"]})),)
            .is_err()
    );
    assert!(
        MediaCommand::parse_service(
            "join",
            &object(json!({"group_members": ["media_player.room_2"]})),
        )
        .is_ok()
    );
    assert!(MediaCommand::parse_service("missing", &Map::new()).is_err());
}

#[test]
fn snapshot_projection_omits_nulls_preserves_position_time_and_capability_gates() {
    let timestamp = Utc
        .timestamp_opt(1_736_000_000, 123_456_789)
        .single()
        .unwrap();
    let snapshot = MediaPlayerSnapshot {
        state: Some(MediaPlayerState::Paused),
        supported_features: MediaPlayerFeatures::SELECT_SOURCE
            | MediaPlayerFeatures::SELECT_SOUND_MODE,
        media_position: Some(12.5),
        media_position_updated_at: Some(timestamp),
        volume_level: Some(0.42),
        entity_picture: Some("/api/media_player_proxy/media_player.test".into()),
        entity_picture_local: Some("/api/media_player_proxy/media_player.test".into()),
        source: None,
        source_list: Some(vec!["Living Room".into()]),
        sound_mode_list: Some(Vec::new()),
        ..MediaPlayerSnapshot::default()
    };
    let projected = snapshot.to_entity_snapshot();
    assert_eq!(projected.state.as_deref(), Some("paused"));
    assert!(projected.available);
    assert_eq!(
        projected.attributes["supported_features"],
        json!(2048 | 65536)
    );
    assert_eq!(projected.attributes["media_position"], json!(12.5));
    assert_eq!(
        projected.attributes["media_position_updated_at"],
        json!(timestamp.to_rfc3339_opts(SecondsFormat::AutoSi, true))
    );
    assert_eq!(projected.attributes["volume_level"], json!(0.42));
    assert_eq!(
        projected.attributes["entity_picture"],
        json!("/api/media_player_proxy/media_player.test")
    );
    assert_eq!(
        projected.attributes["entity_picture_local"],
        json!("/api/media_player_proxy/media_player.test")
    );
    assert_eq!(projected.attributes["source_list"], json!(["Living Room"]));
    assert!(!projected.attributes.contains_key("source"));
    assert!(!projected.attributes.contains_key("sound_mode_list"));
    assert!(!projected.attributes.contains_key("media_title"));
    let no_timestamp = MediaPlayerSnapshot {
        media_position: Some(4.0),
        ..MediaPlayerSnapshot::default()
    }
    .to_entity_snapshot();
    assert!(
        !no_timestamp
            .attributes
            .contains_key("media_position_updated_at")
    );
    assert!(!no_timestamp.attributes.contains_key("entity_picture"));
    assert!(!no_timestamp.attributes.contains_key("entity_picture_local"));

    let no_feature = MediaPlayerSnapshot {
        supported_features: MediaPlayerFeatures::NONE,
        source_list: Some(vec!["not advertised".into()]),
        sound_mode_list: Some(vec!["not advertised".into()]),
        ..snapshot.clone()
    }
    .to_entity_snapshot();
    assert!(!no_feature.attributes.contains_key("source_list"));
    assert!(!no_feature.attributes.contains_key("sound_mode_list"));

    let off = MediaPlayerSnapshot {
        state: Some(MediaPlayerState::Off),
        media_title: Some("stale title".into()),
        volume_level: Some(0.5),
        source: Some("stale source".into()),
        source_list: Some(vec!["Room".into()]),
        supported_features: MediaPlayerFeatures::SELECT_SOURCE,
        ..MediaPlayerSnapshot::default()
    }
    .to_entity_snapshot();
    assert_eq!(off.state.as_deref(), Some("off"));
    assert!(!off.attributes.contains_key("media_title"));
    assert!(!off.attributes.contains_key("volume_level"));
    assert!(!off.attributes.contains_key("source"));
    assert!(!off.attributes.contains_key("entity_picture"));
    assert!(!off.attributes.contains_key("entity_picture_local"));
    assert_eq!(off.attributes["source_list"], json!(["Room"]));
}

#[test]
fn service_schema_rejects_undeclared_fields_and_removes_editor_metadata() {
    for (service, data) in [
        ("media_play", json!({"unexpected":true})),
        ("volume_set", json!({"volume_level":0.5,"bogus":true})),
        (
            "play_media",
            json!({"media_content_type":"music","media_content_id":"track","bogus":true}),
        ),
    ] {
        assert!(matches!(
            MediaCommand::parse_service(service, &object(data)),
            Err(MediaPlayerError::InvalidFormat(_))
        ));
    }
    let command = MediaCommand::parse_service(
        "media_play",
        &object(json!({"metadata":{"title":"Editor only"}})),
    )
    .unwrap();
    assert!(command.extra.is_empty());
    let command = MediaCommand::parse_service("play_media", &object(json!({"media_content_type":"music","media_content_id":"track","extra":{"device_id":"fixture"}}))).unwrap();
    let MediaCommandKind::PlayMedia(play) = command.kind else {
        panic!("expected play-media")
    };
    assert_eq!(play.extra["device_id"], "fixture");
}

#[test]
fn grouping_and_local_picture_preserve_source_null_and_off_rules() {
    let mut snapshot = MediaPlayerSnapshot {
        supported_features: MediaPlayerFeatures::GROUPING,
        entity_picture: Some("https://images.example/remote.jpg".into()),
        ..MediaPlayerSnapshot::default()
    };
    let unknown = snapshot.to_entity_snapshot();
    assert_eq!(unknown.attributes.get("group_members"), Some(&Value::Null));
    assert!(unknown.attributes.contains_key("entity_picture"));
    assert!(!unknown.attributes.contains_key("entity_picture_local"));
    snapshot.entity_picture_local = Some("/api/media_player_proxy/media_player.fixture".into());
    assert!(
        snapshot
            .to_entity_snapshot()
            .attributes
            .contains_key("entity_picture_local")
    );
    snapshot.supported_features = MediaPlayerFeatures::NONE;
    snapshot.group_members = Some(vec!["media_player.one".into(), "media_player.two".into()]);
    snapshot.state = Some(MediaPlayerState::Off);
    let off = snapshot.to_entity_snapshot();
    assert_eq!(
        off.attributes["group_members"],
        json!(["media_player.one", "media_player.two"])
    );
    assert!(!off.attributes.contains_key("entity_picture"));
    assert!(!off.attributes.contains_key("entity_picture_local"));
}

#[test]
fn browse_and_search_payloads_match_pinned_tree_serialization() {
    let mut root = BrowseMedia::new("Albums", "directory", "library", "albums", false, true);
    let mut child = BrowseMedia::new("Jazz", "genre", "music", "genre:jazz", false, true);
    child.children.push(BrowseMedia::new(
        "Blue Train",
        "album",
        "music",
        "album:blue-train",
        true,
        false,
    ));
    child.not_shown = 12;
    root.children.push(child);
    root.not_shown = 2;
    root.can_search = true;
    root.search_media_classes = Some(vec!["album".into(), "artist".into()]);
    root.thumbnail = Some("/browse_media/library/albums".into());

    let browse = root.to_service_response();
    assert_eq!(browse["not_shown"], json!(2));
    assert_eq!(browse["children_media_class"], json!("genre"));
    assert_eq!(browse["children"][0]["title"], json!("Jazz"));
    assert!(browse["children"][0].get("children").is_none());
    assert!(browse["children"][0].get("not_shown").is_none());
    assert_eq!(browse["search_media_classes"], json!(["album", "artist"]));
    assert_eq!(browse["thumbnail"], json!("/browse_media/library/albums"));

    let search = SearchMedia::new(vec![root]).to_service_response();
    assert_eq!(search["result"].as_array().unwrap().len(), 1);
    assert_eq!(search["result"][0]["children"][0]["title"], json!("Jazz"));
    assert!(search["result"][0]["children"][0].get("children").is_none());
    assert!(search.get("version").is_none());
}

type FakeExecuteHook = Arc<dyn Fn(&MediaPlayerProviderContext) + Send + Sync>;

#[derive(Default)]
struct FakeProvider {
    snapshots: StdMutex<BTreeMap<String, MediaPlayerSnapshot>>,
    executions: StdMutex<Vec<(String, MediaCommand)>>,
    contexts: StdMutex<Vec<Context>>,
    browse_result: StdMutex<Option<BrowseMedia>>,
    search_result: StdMutex<Option<SearchMedia>>,
    command_outcome: StdMutex<ProviderCommandOutcome>,
    execute_hook: StdMutex<Option<FakeExecuteHook>>,
}

impl FakeProvider {
    fn set_snapshot(&self, key: &str, snapshot: MediaPlayerSnapshot) {
        self.snapshots
            .lock()
            .unwrap()
            .insert(key.to_owned(), snapshot);
    }

    fn calls(&self) -> Vec<(String, MediaCommand)> {
        self.executions.lock().unwrap().clone()
    }

    fn set_hook(&self, hook: FakeExecuteHook) {
        *self.execute_hook.lock().unwrap() = Some(hook);
    }

    fn set_command_outcome(&self, outcome: ProviderCommandOutcome) {
        *self.command_outcome.lock().unwrap() = outcome;
    }
}

#[async_trait]
impl MediaPlayerProvider for FakeProvider {
    async fn snapshot(
        &self,
        context: &MediaPlayerProviderContext,
    ) -> MediaPlayerResult<MediaPlayerSnapshot> {
        self.snapshots
            .lock()
            .unwrap()
            .get(context.provider_key())
            .cloned()
            .ok_or_else(|| MediaPlayerError::Provider("missing fake snapshot".into()))
    }

    async fn execute(
        &self,
        context: &MediaPlayerProviderContext,
        command: MediaCommand,
    ) -> MediaPlayerResult<ProviderCommandOutcome> {
        self.contexts
            .lock()
            .unwrap()
            .push(context.context().clone());
        self.executions
            .lock()
            .unwrap()
            .push((context.entity_id().to_owned(), command));
        let hook = self.execute_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(context);
        }
        Ok(*self.command_outcome.lock().unwrap())
    }

    async fn browse(
        &self,
        _context: &MediaPlayerProviderContext,
        _request: BrowseMediaRequest,
    ) -> MediaPlayerResult<BrowseMedia> {
        Ok(self
            .browse_result
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| {
                BrowseMedia::new("Root", "directory", "fixture", "root", false, true)
            }))
    }

    async fn search(
        &self,
        _context: &MediaPlayerProviderContext,
        _request: SearchMediaRequest,
    ) -> MediaPlayerResult<SearchMedia> {
        Ok(self
            .search_result
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| SearchMedia::new(Vec::new())))
    }
}

#[tokio::test]
async fn service_dispatch_preserves_host_context_for_provider_callbacks() {
    let harness = Harness::new();
    let provider = Arc::new(FakeProvider::default());
    let registration = harness
        .dispatcher
        .register_provider(
            "owner",
            provider.clone(),
            vec![entity_spec("owner", "one", "fixture")],
        )
        .unwrap();
    let entity_id = registration.entity_id("one").unwrap();
    let context = Context {
        id: "service-context".into(),
        parent_id: Some("parent-context".into()),
        user_id: Some("fixture-user".into()),
    };
    harness
        .dispatcher
        .dispatch_service_with_context(
            "media_play",
            &Map::new(),
            std::slice::from_ref(&entity_id),
            context.clone(),
        )
        .await
        .unwrap();
    assert_eq!(provider.contexts.lock().unwrap().as_slice(), &[context]);
    registration.unregister().unwrap();
    harness.shutdown().await;
}

struct BlockingSnapshotProvider {
    started: Notify,
    result: StdMutex<Option<oneshot::Receiver<MediaPlayerSnapshot>>>,
}

#[async_trait]
impl MediaPlayerProvider for BlockingSnapshotProvider {
    async fn snapshot(
        &self,
        _context: &MediaPlayerProviderContext,
    ) -> MediaPlayerResult<MediaPlayerSnapshot> {
        self.started.notify_one();
        let result = self.result.lock().unwrap().take().unwrap();
        result
            .await
            .map_err(|error| MediaPlayerError::Provider(error.to_string()))
    }

    async fn execute(
        &self,
        _context: &MediaPlayerProviderContext,
        _command: MediaCommand,
    ) -> MediaPlayerResult<ProviderCommandOutcome> {
        Ok(ProviderCommandOutcome::Executed)
    }
}

struct Harness {
    runtime: Runtime,
    registries: Registries,
    lifecycle: EntityLifecycle,
    dispatcher: MediaPlayerDispatcher,
}

impl Harness {
    fn new() -> Self {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let registries = Registries::new(runtime.clone()).unwrap();
        let lifecycle = EntityLifecycle::new(runtime.clone(), registries.clone()).unwrap();
        let dispatcher = MediaPlayerDispatcher::new(lifecycle.clone());
        Self {
            runtime,
            registries,
            lifecycle,
            dispatcher,
        }
    }

    async fn shutdown(self) {
        self.lifecycle.shutdown().await.unwrap();
    }
}

fn entity_spec(owner: &str, key: &str, platform: &str) -> MediaPlayerEntitySpec {
    MediaPlayerEntitySpec::new(
        EntitySpec::new("sensor", platform, owner, key)
            .with_unique_id(format!("{owner}-{key}"))
            .with_suggested_object_id(format!("{owner}_{key}")),
        MediaPlayerFeatures::PLAY
            | MediaPlayerFeatures::PAUSE
            | MediaPlayerFeatures::PLAY_MEDIA
            | MediaPlayerFeatures::BROWSE_MEDIA
            | MediaPlayerFeatures::SEARCH_MEDIA,
    )
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("lifecycle change was not observed");
}

#[tokio::test]
async fn published_unavailable_snapshot_skips_service_and_recovery_restores_dispatch() {
    let harness = Harness::new();
    let provider = Arc::new(FakeProvider::default());
    provider.set_snapshot(
        "one",
        MediaPlayerSnapshot {
            available: false,
            supported_features: MediaPlayerFeatures::PLAY,
            ..MediaPlayerSnapshot::default()
        },
    );
    let registration = harness
        .dispatcher
        .register_provider(
            "owner",
            provider.clone(),
            vec![entity_spec("owner", "one", "fixture")],
        )
        .unwrap();
    let id = registration.entity_id("one").unwrap();
    harness
        .dispatcher
        .refresh_snapshot("owner", "one")
        .await
        .unwrap();
    let result = harness
        .dispatcher
        .dispatch_service("media_play", &Map::new(), std::slice::from_ref(&id))
        .await
        .unwrap();
    assert_eq!(result.skipped_unavailable, 1);
    assert!(result.dispatched_entity_ids.is_empty());
    assert!(provider.calls().is_empty());
    provider.set_snapshot(
        "one",
        MediaPlayerSnapshot {
            available: true,
            state: Some(MediaPlayerState::Playing),
            supported_features: MediaPlayerFeatures::PLAY,
            ..MediaPlayerSnapshot::default()
        },
    );
    harness
        .dispatcher
        .refresh_snapshot("owner", "one")
        .await
        .unwrap();
    let result = harness
        .dispatcher
        .dispatch_service("media_play", &Map::new(), std::slice::from_ref(&id))
        .await
        .unwrap();
    assert_eq!(result.dispatched_entity_ids, vec![id]);
    assert_eq!(provider.calls().len(), 1);
    registration.unregister().unwrap();
    harness.shutdown().await;
}

#[tokio::test]
async fn provider_owners_target_independently_and_release_only_their_handles() {
    let harness = Harness::new();
    let provider_a = Arc::new(FakeProvider::default());
    let provider_b = Arc::new(FakeProvider::default());
    let registration_a = harness
        .dispatcher
        .register_provider(
            "owner_a",
            provider_a.clone(),
            vec![entity_spec("owner_a", "one", "a")],
        )
        .unwrap();
    let registration_b = harness
        .dispatcher
        .register_provider(
            "owner_b",
            provider_b.clone(),
            vec![entity_spec("owner_b", "one", "b")],
        )
        .unwrap();
    let id_a = registration_a.entity_id("one").unwrap();
    let id_b = registration_b.entity_id("one").unwrap();
    let targets = vec![id_a.clone(), id_b.clone()];

    let outcome = harness
        .dispatcher
        .dispatch_service("media_play", &Map::new(), &targets)
        .await
        .unwrap();
    assert_eq!(outcome.dispatched_entity_ids.len(), 2);
    assert_eq!(provider_a.calls().len(), 1);
    assert_eq!(provider_b.calls().len(), 1);

    let cancellation_a = registration_a.cancellation();
    let cancellation_waiter = tokio::spawn(async move { cancellation_a.cancelled().await });
    assert_eq!(registration_a.unregister().unwrap(), 1);
    tokio::time::timeout(Duration::from_secs(1), cancellation_waiter)
        .await
        .expect("owner cancellation was not signaled")
        .unwrap();
    assert!(registration_a.entity_id("one").is_none());
    assert!(registration_a.is_cancelled());
    assert!(!registration_b.is_cancelled());
    let remaining = harness
        .dispatcher
        .dispatch_service("media_play", &Map::new(), &targets)
        .await
        .unwrap();
    assert_eq!(remaining.dispatched_entity_ids, vec![id_b]);
    assert_eq!(provider_a.calls().len(), 1);
    assert_eq!(provider_b.calls().len(), 2);
    if let Some(state) = harness.runtime.get_state(&id_a) {
        assert_eq!(state.state, "unavailable");
    }

    registration_b.unregister().unwrap();
    harness.shutdown().await;
}

#[tokio::test]
async fn lifecycle_rename_disable_reenable_and_release_use_current_handle_state() {
    let harness = Harness::new();
    let provider = Arc::new(FakeProvider::default());
    provider.set_snapshot(
        "one",
        MediaPlayerSnapshot {
            state: Some(MediaPlayerState::Playing),
            supported_features: MediaPlayerFeatures::PLAY,
            media_position: Some(19.0),
            media_position_updated_at: Some(Utc::now()),
            media_title: Some("Fixture track".into()),
            entity_picture: Some("/api/media_player_proxy/media_player.fixture".into()),
            entity_picture_local: Some("/api/media_player_proxy/media_player.fixture".into()),
            ..MediaPlayerSnapshot::default()
        },
    );
    let registration = harness
        .dispatcher
        .register_provider(
            "owner",
            provider.clone(),
            vec![entity_spec("owner", "one", "fixture")],
        )
        .unwrap();
    let original_id = registration.entity_id("one").unwrap();
    harness
        .dispatcher
        .refresh_snapshot("owner", "one")
        .await
        .unwrap();
    assert_eq!(
        registration.supported_features("one"),
        Some(MediaPlayerFeatures::PLAY)
    );
    assert_eq!(
        harness.runtime.get_state(&original_id).unwrap().state,
        "playing"
    );

    harness
        .registries
        .update_entity(
            &original_id,
            EntityUpdate {
                new_entity_id: Some("media_player.renamed_fixture".into()),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_until(|| registration.entity_id("one").as_deref() == Some("media_player.renamed_fixture"))
        .await;
    let renamed = registration.entity_id("one").unwrap();
    let stale_target = harness
        .dispatcher
        .dispatch_service(
            "media_play",
            &Map::new(),
            std::slice::from_ref(&original_id),
        )
        .await
        .unwrap();
    assert!(stale_target.dispatched_entity_ids.is_empty());
    assert_eq!(
        harness
            .dispatcher
            .dispatch_service("media_play", &Map::new(), std::slice::from_ref(&renamed))
            .await
            .unwrap()
            .dispatched_entity_ids,
        vec![renamed.clone()]
    );
    assert_eq!(provider.calls().len(), 1);

    harness
        .registries
        .update_entity(
            &renamed,
            EntityUpdate {
                disabled_by: Some(Some("user".into())),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_until(|| registration.entity_id("one").is_none()).await;
    let disabled = harness
        .dispatcher
        .dispatch_service("media_play", &Map::new(), std::slice::from_ref(&renamed))
        .await
        .unwrap();
    assert_eq!(disabled.skipped_unavailable, 1);
    assert_eq!(provider.calls().len(), 1);

    harness
        .registries
        .update_entity(
            &renamed,
            EntityUpdate {
                disabled_by: Some(None),
                ..EntityUpdate::default()
            },
        )
        .unwrap();
    wait_until(|| registration.entity_id("one").as_deref() == Some(renamed.as_str())).await;
    assert!(
        harness
            .dispatcher
            .refresh_snapshot("owner", "one")
            .await
            .unwrap()
            .is_some()
    );
    let now = Utc::now();
    let history = harness
        .runtime
        .history(
            &renamed,
            now - chrono::Duration::minutes(1),
            now + chrono::Duration::minutes(1),
        )
        .unwrap();
    assert!(!history.is_empty());
    assert!(
        history
            .iter()
            .all(|state| !state.attributes.contains_key("media_position"))
    );
    assert!(history.iter().all(|state| {
        !state.attributes.contains_key("entity_picture")
            && !state.attributes.contains_key("entity_picture_local")
    }));
    assert!(
        history
            .iter()
            .all(|state| !state.attributes.contains_key("media_position_updated_at"))
    );
    assert!(
        harness
            .runtime
            .get_state(&renamed)
            .unwrap()
            .attributes
            .contains_key("media_position")
    );

    assert_eq!(registration.unregister().unwrap(), 1);
    assert!(registration.entity_id("one").is_none());
    let released = harness.runtime.get_state(&renamed).unwrap();
    assert_eq!(released.state, "unavailable");
    assert_eq!(released.attributes["restored"], Value::Bool(true));
    harness.shutdown().await;
}

#[tokio::test]
async fn unload_cancels_snapshot_future_before_stale_state_can_publish() {
    let harness = Harness::new();
    let (release_tx, release_rx) = oneshot::channel();
    let provider = Arc::new(BlockingSnapshotProvider {
        started: Notify::new(),
        result: StdMutex::new(Some(release_rx)),
    });
    let registration = harness
        .dispatcher
        .register_provider(
            "owner",
            provider.clone(),
            vec![entity_spec("owner", "one", "fixture")],
        )
        .unwrap();
    let entity_id = registration.entity_id("one").unwrap();
    let started = provider.started.notified();
    let dispatcher = harness.dispatcher.clone();
    let refresh = tokio::spawn(async move { dispatcher.refresh_snapshot("owner", "one").await });
    started.await;
    registration.unregister().unwrap();
    assert!(matches!(
        refresh.await.unwrap(),
        Err(MediaPlayerError::Cancelled)
    ));
    let late_snapshot = MediaPlayerSnapshot {
        state: Some(MediaPlayerState::Playing),
        media_title: Some("must not publish".into()),
        ..MediaPlayerSnapshot::default()
    };
    assert!(release_tx.send(late_snapshot).is_err());
    if let Some(state) = harness.runtime.get_state(&entity_id) {
        assert_ne!(state.state, "playing");
        assert_ne!(
            state.attributes.get("media_title"),
            Some(&json!("must not publish"))
        );
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn provider_callback_can_reregister_and_old_weak_lease_cannot_remove_new_owner() {
    let harness = Harness::new();
    let original = Arc::new(FakeProvider::default());
    let replacement = Arc::new(FakeProvider::default());
    let dispatcher_for_hook = harness.dispatcher.clone();
    let replacement_for_hook = replacement.clone();
    let replacement_registration = Arc::new(StdMutex::new(None::<MediaPlayerRegistration>));
    let replacement_registration_for_hook = replacement_registration.clone();
    original.set_hook(Arc::new(move |_| {
        let registration = dispatcher_for_hook
            .register_provider(
                "owner",
                replacement_for_hook.clone(),
                vec![entity_spec("owner", "one", "fixture")],
            )
            .unwrap();
        *replacement_registration_for_hook.lock().unwrap() = Some(registration);
    }));
    let old_registration = harness
        .dispatcher
        .register_provider(
            "owner",
            original.clone(),
            vec![entity_spec("owner", "one", "fixture")],
        )
        .unwrap();
    let old_id = old_registration.entity_id("one").unwrap();
    let dispatch = tokio::time::timeout(
        Duration::from_secs(2),
        harness.dispatcher.dispatch_service(
            "media_play",
            &Map::new(),
            std::slice::from_ref(&old_id),
        ),
    )
    .await
    .expect("provider callback deadlocked while re-entering registration");
    assert!(matches!(dispatch, Err(MediaPlayerError::Cancelled)));
    assert!(old_registration.entity_id("one").is_none());
    assert!(old_registration.is_cancelled());
    let current = replacement_registration.lock().unwrap().take().unwrap();
    assert!(current.entity_id("one").is_some());
    assert!(!current.is_cancelled());

    // Dropping the old RAII lease is generation-scoped and leaves the new
    // registration available for dispatch.
    drop(old_registration);
    let current_id = current.entity_id("one").unwrap();
    let outcome = harness
        .dispatcher
        .dispatch_service("media_play", &Map::new(), std::slice::from_ref(&current_id))
        .await
        .unwrap();
    assert_eq!(outcome.dispatched_entity_ids, vec![current_id]);
    assert_eq!(replacement.calls().len(), 1);
    current.unregister().unwrap();
    harness.shutdown().await;
}

#[tokio::test]
async fn direct_browse_and_search_use_feature_checks_and_typed_results() {
    let harness = Harness::new();
    let provider = Arc::new(FakeProvider::default());
    let mut browsed = BrowseMedia::new(
        "Fixture library",
        "directory",
        "fixture",
        "root",
        false,
        true,
    );
    browsed.children.push(BrowseMedia::new(
        "Track", "track", "music", "track:1", true, false,
    ));
    *provider.browse_result.lock().unwrap() = Some(browsed.clone());
    *provider.search_result.lock().unwrap() = Some(SearchMedia::new(vec![browsed.clone()]));
    let registration = harness
        .dispatcher
        .register_provider(
            "owner",
            provider,
            vec![entity_spec("owner", "one", "fixture")],
        )
        .unwrap();
    let entity_id = registration.entity_id("one").unwrap();

    let invalid_pair = object(json!({"media_content_type": "music"}));
    assert!(matches!(
        harness
            .dispatcher
            .browse_media(&entity_id, &invalid_pair)
            .await,
        Err(MediaPlayerError::InvalidFormat(_))
    ));
    let browse = harness
        .dispatcher
        .browse_media(
            &entity_id,
            &object(json!({"media_content_type":"music", "media_content_id":"root"})),
        )
        .await
        .unwrap();
    assert_eq!(
        browse.to_service_response()["children"][0]["title"],
        json!("Track")
    );
    let search = harness
        .dispatcher
        .search_media(
            &entity_id,
            &object(json!({"search_query":"track", "media_filter_classes":["track"]})),
        )
        .await
        .unwrap();
    assert_eq!(
        search.to_service_response()["result"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let service_browse = harness
        .dispatcher
        .dispatch_service(
            "browse_media",
            &Map::new(),
            std::slice::from_ref(&entity_id),
        )
        .await
        .unwrap();
    assert_eq!(service_browse.responses.len(), 1);
    assert_eq!(service_browse.responses[0].entity_id(), entity_id);
    registration.unregister().unwrap();
    harness.shutdown().await;
}

#[tokio::test]
async fn browse_service_preserves_runtime_gate_quirk_and_provider_noop() {
    let harness = Harness::new();
    let provider = Arc::new(FakeProvider::default());
    provider.set_command_outcome(ProviderCommandOutcome::NoOp);
    let mut spec = entity_spec("owner", "one", "fixture");
    spec.supported_features = MediaPlayerFeatures::PLAY | MediaPlayerFeatures::SELECT_SOURCE;
    let registration = harness
        .dispatcher
        .register_provider("owner", provider.clone(), vec![spec])
        .unwrap();
    let entity_id = registration.entity_id("one").unwrap();

    assert!(matches!(
        harness
            .dispatcher
            .browse_media(&entity_id, &Map::new())
            .await,
        Err(MediaPlayerError::NotSupported(message))
            if message == "Player does not support browsing media"
    ));
    let browsed = harness
        .dispatcher
        .dispatch_service(
            "browse_media",
            &object(json!({"media_content_type": "music"})),
            std::slice::from_ref(&entity_id),
        )
        .await
        .unwrap();
    assert_eq!(browsed.responses.len(), 1);
    assert!(
        harness
            .dispatcher
            .dispatch_service(
                "search_media",
                &object(json!({"search_query": "track"})),
                std::slice::from_ref(&entity_id),
            )
            .await
            .is_err()
    );

    let command = harness
        .dispatcher
        .dispatch_service(
            "select_source",
            &object(json!({"source": "no-longer-cached"})),
            std::slice::from_ref(&entity_id),
        )
        .await
        .unwrap();
    assert_eq!(command.no_op_entity_ids, vec![entity_id]);
    assert!(command.dispatched_entity_ids.is_empty());
    assert_eq!(provider.calls().len(), 1);
    registration.unregister().unwrap();
    harness.shutdown().await;
}
