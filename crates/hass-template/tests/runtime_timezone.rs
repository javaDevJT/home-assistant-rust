use hass_core::{MemoryStore, Runtime};
use hass_template::TemplateEngine;
use serde_json::Map;
use std::sync::Arc;

#[test]
fn local_filters_follow_home_assistant_timezone_and_live_updates() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    runtime.set_time_zone("Asia/Tokyo").unwrap();
    let renderer = TemplateEngine::new(runtime.clone());
    let variables = Map::new();
    assert_eq!(
        renderer
            .render("{{ 0 | timestamp_local }}", &variables)
            .unwrap(),
        "1970-01-01T09:00:00+09:00"
    );
    assert_eq!(
        renderer
            .render("{{ 0 | timestamp_custom('%H:%M', true) }}", &variables)
            .unwrap(),
        "09:00"
    );
    assert_eq!(
        renderer
            .render("{{ as_local('1970-01-01T00:00:00+00:00') }}", &variables)
            .unwrap(),
        "1970-01-01T09:00:00+09:00"
    );
    runtime.set_time_zone("America/New_York").unwrap();
    assert_eq!(
        renderer
            .render("{{ 0 | timestamp_local }}", &variables)
            .unwrap(),
        "1969-12-31T19:00:00-05:00"
    );
    assert!(runtime.set_time_zone("not/a/timezone").is_err());
    assert_eq!(runtime.time_zone(), "America/New_York");
}
