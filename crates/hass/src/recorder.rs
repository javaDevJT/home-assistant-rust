//! Native recorder retention and admin purge service. Persistence itself is
//! owned by Runtime/Store; this module owns the nightly maintenance lifecycle.
use crate::config::RecorderConfig;
use anyhow::{Context as _, Result};
use chrono::{DateTime, Datelike, Days, LocalResult, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use hass_auth::AuthManager;
use hass_core::{Error, Runtime, ServiceResult};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, task::JoinHandle};

pub struct RecorderMaintenance {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
    _auth: Arc<AuthManager>,
}

impl RecorderMaintenance {
    pub fn start(runtime: Runtime, auth: AuthManager, config: RecorderConfig) -> Result<Self> {
        let keep_days = config.purge_keep_days;
        let auth = Arc::new(auth);
        let authentication = Arc::downgrade(&auth);
        runtime.register_service("recorder","purge",json!({"description":"Purge old recorded history and events; optionally repack storage","fields":{"keep_days":{},"repack":{},"apply_filter":{}}}),Arc::new(move |runtime,call|{
            if let Some(user_id)=call.context.user_id.as_deref() {
                let user=authentication.upgrade().ok_or_else(||Error::Service("Recorder maintenance is stopped".into()))?.get_user(user_id)?;
                if !user.is_active || (!user.is_owner && !user.is_admin) {
                    return Err(Error::InvalidInput("Administrator privileges are required".into()));
                }
            }
            if !call.target.is_empty() || call.data.keys().any(|key|!matches!(key.as_str(),"keep_days"|"repack"|"apply_filter")) {
                return Err(Error::InvalidInput("Invalid recorder purge fields".into()));
            }
            let days=match call.data.get("keep_days") {Some(value)=>value.as_u64().filter(|days|*days<=365_000).ok_or_else(||Error::InvalidInput("Invalid keep_days".into()))?,None=>keep_days};
            let repack=boolean(&call.data,"repack")?;
            let _apply_filter=boolean(&call.data,"apply_filter")?;
            // This runtime rejects recorder include/exclude options, so there
            // are no additional entity filters for apply_filter to apply.
            let counts=purge(runtime,days,repack)?;
            Ok(ServiceResult{changed_states:Vec::new(),response:Some(json!({"states_deleted":counts.states_deleted,"events_deleted":counts.events_deleted})),context:call.context.clone()})
        }))?;
        let (stop, receiver) = watch::channel(false);
        let task = tokio::spawn(maintenance(runtime, config, receiver));
        Ok(Self {
            stop,
            task,
            _auth: auth,
        })
    }

    pub async fn shutdown(self) -> Result<()> {
        let _ = self.stop.send(true);
        self.task.await.context("recorder maintenance task failed")
    }
}

fn boolean(
    data: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> hass_core::Result<bool> {
    data.get(key).map_or(Ok(false), |value| {
        value
            .as_bool()
            .ok_or_else(|| Error::InvalidInput(format!("{key} must be a boolean")))
    })
}

fn purge(runtime: &Runtime, days: u64, repack: bool) -> hass_core::Result<hass_core::PurgeResult> {
    let duration = chrono::Duration::try_days(
        i64::try_from(days).map_err(|_| Error::InvalidInput("Invalid retention days".into()))?,
    )
    .ok_or_else(|| Error::InvalidInput("Invalid retention days".into()))?;
    let cutoff = Utc::now()
        .checked_sub_signed(duration)
        .ok_or_else(|| Error::InvalidInput("Retention cutoff is out of range".into()))?;
    let counts = runtime.purge_before(cutoff)?;
    if repack {
        runtime.repack_storage()?;
    }
    Ok(counts)
}

fn next_nightly(now: DateTime<Utc>, timezone: Tz) -> Option<DateTime<Utc>> {
    let mut day = now.with_timezone(&timezone).date_naive();
    for _ in 0..3 {
        let local = day.and_time(NaiveTime::from_hms_opt(4, 12, 0)?);
        let candidates = match timezone.from_local_datetime(&local) {
            LocalResult::Single(time) => vec![time],
            LocalResult::Ambiguous(first, second) => vec![first, second],
            LocalResult::None => Vec::new(),
        };
        if let Some(next) = candidates
            .into_iter()
            .map(|time| time.with_timezone(&Utc))
            .filter(|time| *time > now)
            .min()
        {
            return Some(next);
        }
        day = day.checked_add_days(Days::new(1))?;
    }
    None
}

async fn maintenance(runtime: Runtime, config: RecorderConfig, mut stop: watch::Receiver<bool>) {
    if !config.auto_purge {
        loop {
            if *stop.borrow_and_update() {
                break;
            }
            if stop.changed().await.is_err() {
                break;
            }
        }
        return;
    }
    let mut events = runtime.subscribe();
    let mut last_run = None;
    let mut scheduled = None;
    loop {
        if *stop.borrow_and_update() {
            break;
        }
        let Ok(timezone) = runtime.time_zone().parse::<Tz>() else {
            break;
        };
        if scheduled.is_none_or(|(zone, _)| zone != timezone) {
            scheduled = next_nightly(Utc::now(), timezone).map(|next| (timezone, next));
        }
        let Some((_, next)) = scheduled else {
            break;
        };
        let wait = (next - Utc::now())
            .to_std()
            .unwrap_or_default()
            .min(Duration::from_secs(3600));
        tokio::select! {
            result=stop.changed()=>{if result.is_err() || *stop.borrow(){break;}},
            event=events.recv()=>{
                if matches!(event,Err(tokio::sync::broadcast::error::RecvError::Closed)){break;}
            },
            _=tokio::time::sleep(wait)=>{
                if Utc::now()<next {continue;}
                scheduled=None;
                let date=next.with_timezone(&timezone).date_naive();
                if last_run==Some(date){continue;}
                last_run=Some(date);
                let repack=config.auto_repack && date.weekday()==Weekday::Sun && (8..=14).contains(&date.day());
                let target=runtime.clone();
                let days=config.purge_keep_days;
                match tokio::task::spawn_blocking(move||purge(&target,days,repack)).await {
                    Ok(Ok(counts))=>tracing::info!(states_deleted=counts.states_deleted,events_deleted=counts.events_deleted,"recorder retention completed"),
                    Ok(Err(error))=>tracing::error!(%error,"recorder retention failed"),
                    Err(error)=>tracing::error!(%error,"recorder retention task failed"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nightly_schedule_tracks_local_timezone_and_next_day() {
        let now = Utc.with_ymd_and_hms(2026, 11, 5, 8, 0, 0).unwrap();
        let next = next_nightly(now, chrono_tz::America::New_York).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 11, 5, 9, 12, 0).unwrap());
        let after = next_nightly(next, chrono_tz::America::New_York).unwrap();
        assert_eq!(after, Utc.with_ymd_and_hms(2026, 11, 6, 9, 12, 0).unwrap());
    }

    #[tokio::test]
    async fn purge_requires_admin_and_preserves_current_state_and_auth_metadata() {
        use hass_core::{Context, Event, MemoryStore, ServiceCall, State, Store};
        use serde_json::Map;
        let store = Arc::new(MemoryStore::default());
        let context = Context::default();
        let state = State {
            entity_id: "sensor.retention".into(),
            state: "42".into(),
            attributes: Map::new(),
            last_changed: Utc.timestamp_opt(100, 0).unwrap(),
            last_updated: Utc.timestamp_opt(100, 0).unwrap(),
            last_reported: Utc.timestamp_opt(100, 0).unwrap(),
            context: context.clone(),
        };
        let event = Event {
            event_type: "state_changed".into(),
            data: Map::new(),
            origin: "LOCAL".into(),
            time_fired: state.last_updated,
            context,
        };
        store.put_state(&state, Some(&event)).unwrap();
        let runtime = Runtime::new(store).unwrap();
        let auth = AuthManager::new(runtime.clone()).unwrap();
        let owner = auth
            .bootstrap_owner_with_username("owner", "Owner", "strong-fixture-password")
            .unwrap();
        let reader = auth
            .create_user_with_username(
                &owner.id,
                "reader",
                "Reader",
                "strong-fixture-password",
                false,
            )
            .unwrap();
        let tokens = auth
            .issue_tokens(&owner.id, "http://localhost/", None)
            .unwrap();
        let maintenance = RecorderMaintenance::start(
            runtime.clone(),
            auth.clone(),
            RecorderConfig {
                auto_purge: false,
                ..RecorderConfig::default()
            },
        )
        .unwrap();
        let call = |user_id: String| ServiceCall {
            domain: "recorder".into(),
            service: "purge".into(),
            data: Map::from_iter([
                ("keep_days".into(), json!(0)),
                ("repack".into(), json!(true)),
            ]),
            target: Map::new(),
            context: Context {
                user_id: Some(user_id),
                ..Context::default()
            },
        };
        assert!(runtime.call_service(call(reader.id)).is_err());
        assert_eq!(
            runtime
                .history(
                    "sensor.retention",
                    Utc.timestamp_opt(0, 0).unwrap(),
                    Utc::now()
                )
                .unwrap()
                .len(),
            1
        );
        let result = runtime.call_service(call(owner.id)).unwrap();
        assert_eq!(result.response.unwrap()["states_deleted"], 1);
        assert!(
            runtime
                .history(
                    "sensor.retention",
                    Utc.timestamp_opt(0, 0).unwrap(),
                    Utc::now()
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(runtime.get_state("sensor.retention").unwrap().state, "42");
        assert!(auth.validate_access_token(&tokens.access_token).is_ok());
        maintenance.shutdown().await.unwrap();
    }
}
