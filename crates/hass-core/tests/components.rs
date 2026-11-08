use std::sync::Arc;

use hass_core::{LoadedComponents, MemoryStore, Runtime};

fn runtime() -> Runtime {
    Runtime::new(Arc::new(MemoryStore::default())).expect("runtime should initialize")
}

#[test]
fn records_bare_names_and_sorted_dotted_platform_names() {
    let loaded = LoadedComponents::default();
    loaded.record_successful_component("sensor");
    loaded.record_successful_component("spotify.media_player");

    assert_eq!(loaded.components(), vec!["sensor", "spotify.media_player"]);
    assert_eq!(loaded.all_components(), vec!["sensor", "spotify"]);
}

#[test]
fn arbitrary_dotted_name_is_only_in_components() {
    let loaded = LoadedComponents::default();
    loaded.record_successful_component("custom.integration");

    assert_eq!(loaded.components(), vec!["custom.integration"]);
    assert!(loaded.all_components().is_empty());
}

#[test]
fn recording_is_idempotent_and_receipts_are_retained() {
    let loaded = LoadedComponents::default();
    loaded.record_successful_component("spotify.media_player");
    loaded.record_successful_component("spotify.media_player");
    loaded.record_successful_component("hue.light");

    assert_eq!(
        loaded.components(),
        vec!["hue.light", "spotify.media_player"]
    );
    assert_eq!(loaded.all_components(), vec!["hue", "spotify"]);
}

#[test]
fn runtime_clones_share_receipts_and_fresh_runtimes_start_empty() {
    let first = runtime();
    let clone = first.clone();

    clone
        .loaded_components()
        .record_successful_component("light");
    assert_eq!(first.loaded_components().components(), vec!["light"]);

    let second = runtime();
    assert!(second.loaded_components().components().is_empty());
    assert!(second.loaded_components().all_components().is_empty());
}

#[test]
fn concurrent_receipts_are_preserved_and_sorted() {
    let loaded = LoadedComponents::default();
    let threads: Vec<_> = (0..4)
        .map(|worker| {
            let loaded = loaded.clone();
            std::thread::spawn(move || {
                for item in 0..25 {
                    loaded.record_successful_component(format!("component_{worker}_{item}"));
                }
            })
        })
        .collect();

    for thread in threads {
        thread.join().expect("receipt writer should complete");
    }

    let components = loaded.components();
    assert_eq!(components.len(), 100);
    assert!(components.windows(2).all(|pair| pair[0] < pair[1]));
}
