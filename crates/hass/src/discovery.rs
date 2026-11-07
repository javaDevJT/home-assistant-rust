//! Native discovery startup queue and configuration-flow admission.

use hass_config::{ConfigEntries, DiscoveryKey, DiscoveryKeyValue};
use hass_core::{Error, Result};
use hass_ssdp::{DiscoveryAction, DiscoverySink};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, Weak},
};
use tokio::{
    sync::Notify,
    task::{JoinHandle, JoinSet},
};

const FLOW_INIT_LIMIT: usize = 20;

#[derive(Clone)]
struct Candidate {
    domain: String,
    context: Map<String, Value>,
    data: Map<String, Value>,
}

enum Work {
    Create(Candidate),
    Dismiss {
        st: String,
        location: Option<String>,
    },
}

#[derive(Default)]
struct State {
    started: bool,
    stopping: bool,
    pending: IndexMap<String, Vec<Candidate>>,
    queue: VecDeque<Work>,
    in_flight: Vec<Candidate>,
    active: usize,
}

struct Inner {
    state: Mutex<State>,
    changed: Notify,
    idle: Notify,
}

pub struct DiscoveryDispatcher {
    inner: Arc<Inner>,
    task: Option<JoinHandle<()>>,
}

impl DiscoveryDispatcher {
    pub fn new(entries: ConfigEntries) -> Self {
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            changed: Notify::new(),
            idle: Notify::new(),
        });
        let worker = inner.clone();
        let task = tokio::spawn(async move {
            run(worker, entries).await;
        });
        Self {
            inner,
            task: Some(task),
        }
    }

    pub fn sink(&self) -> DiscoverySink {
        let inner = Arc::downgrade(&self.inner);
        Arc::new(move |action| {
            let result = submit(&inner, action);
            Box::pin(async move { result })
        })
    }

    /// Called after the actual homeassistant_started event. Startup candidates
    /// are deduplicated by domain/source and initial data, then admitted at 20.
    pub async fn mark_started(&self) {
        {
            let mut state = lock(&self.inner.state);
            state.started = true;
            for (_, candidates) in std::mem::take(&mut state.pending) {
                state.queue.extend(candidates.into_iter().map(Work::Create));
            }
        }
        self.inner.changed.notify_one();
        self.wait_idle().await;
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.inner.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = lock(&self.inner.state);
                if state.queue.is_empty() && state.active == 0 {
                    return;
                }
            }
            notified.await;
        }
    }

    pub async fn shutdown(mut self) {
        self.stop();
        // Native drivers are synchronous callbacks. Once admitted to a blocking
        // worker they must finish; aborting their JoinHandle would detach them.
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    fn stop(&self) {
        let mut state = lock(&self.inner.state);
        state.stopping = true;
        state.pending.clear();
        state.queue.clear();
        drop(state);
        self.inner.changed.notify_one();
    }
}

impl Drop for DiscoveryDispatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn submit(inner: &Weak<Inner>, action: DiscoveryAction) -> Result<()> {
    let inner = inner
        .upgrade()
        .ok_or_else(|| Error::Service("discovery dispatcher has stopped".into()))?;
    let mut state = lock(&inner.state);
    if state.stopping {
        return Ok(());
    }
    match action {
        DiscoveryAction::Create { domain, info, udn } => {
            let data = serde_json::to_value(info)
                .map_err(|error| Error::Service(error.to_string()))?
                .as_object()
                .cloned()
                .ok_or_else(|| Error::Service("invalid discovery data".into()))?;
            let key = DiscoveryKey {
                domain: "ssdp".into(),
                key: DiscoveryKeyValue::String(udn),
                version: 1,
            };
            let context = Map::from_iter([
                ("source".into(), json!("ssdp")),
                ("discovery_key".into(), json!(key)),
            ]);
            let candidate = Candidate {
                domain: domain.clone(),
                context,
                data,
            };
            if !state.started {
                let pending = state.pending.entry(domain).or_default();
                if !pending
                    .iter()
                    .any(|existing| existing.data == candidate.data)
                {
                    pending.push(candidate);
                }
            } else {
                state.queue.push_back(Work::Create(candidate));
            }
        }
        DiscoveryAction::Dismiss {
            ssdp_st,
            ssdp_location,
        } => {
            state.queue.push_back(Work::Dismiss {
                st: ssdp_st,
                location: ssdp_location,
            });
        }
    }
    drop(state);
    inner.changed.notify_one();
    Ok(())
}

async fn run(inner: Arc<Inner>, entries: ConfigEntries) {
    let mut jobs = JoinSet::new();
    let mut dismiss_in_flight = false;
    loop {
        while jobs.len() < FLOW_INIT_LIMIT && !dismiss_in_flight {
            let work = {
                let mut state = lock(&inner.state);
                // Preserve causal dismissal: let admitted callbacks publish
                // their forms before dismissing those discovery flows.
                if matches!(state.queue.front(), Some(Work::Dismiss { .. })) && !jobs.is_empty() {
                    None
                } else {
                    let work = state.queue.pop_front();
                    if work.is_some() {
                        state.active += 1;
                    }
                    work
                }
            };
            let Some(work) = work else {
                break;
            };
            let is_dismiss = matches!(&work, Work::Dismiss { .. });
            let candidate = match &work {
                Work::Create(candidate) => Some(candidate.clone()),
                _ => None,
            };
            if let Some(candidate) = &candidate {
                let mut state = lock(&inner.state);
                if state.stopping
                    || state.in_flight.iter().any(|existing| {
                        existing.domain == candidate.domain
                            && existing.data == candidate.data
                            && candidate
                                .context
                                .iter()
                                .all(|(key, value)| existing.context.get(key) == Some(value))
                    })
                {
                    state.active -= 1;
                    drop(state);
                    inner.idle.notify_waiters();
                    continue;
                }
                state.in_flight.push(candidate.clone());
            }
            let entries = entries.clone();
            let complete = inner.clone();
            jobs.spawn_blocking(move || {
                let _completion = Completion {
                    inner: complete.clone(),
                    candidate,
                };
                match work {
                    Work::Create(candidate) => {
                        if lock(&complete.state).stopping
                            || entries.has_matching_discovery_flow(
                                &candidate.domain,
                                "ssdp",
                                &candidate.context,
                                &candidate.data,
                            )
                        {
                            Ok(())
                        } else {
                            entries
                                .init_config_flow_with_input(
                                    &candidate.domain,
                                    "ssdp",
                                    candidate.context,
                                    Some(candidate.data),
                                )
                                .map(|_| ())
                        }
                    }
                    Work::Dismiss { st, location } => {
                        let mut result = Ok(());
                        for domain in entries.flow_handlers() {
                            if let Err(error) =
                                entries.dismiss_discovery_flows(&domain, "ssdp", |data| {
                                    data.get("ssdp_st").and_then(Value::as_str) == Some(st.as_str())
                                        && data.get("ssdp_location").and_then(Value::as_str)
                                            == location.as_deref()
                                })
                            {
                                result = Err(error);
                            }
                        }
                        result
                    }
                }
            });
            // A dismissal also forms a barrier against following creates.
            // Otherwise a new alive flow could race the preceding byebye and
            // be skipped as a duplicate or removed by that older dismissal.
            dismiss_in_flight = is_dismiss;
        }
        {
            let state = lock(&inner.state);
            if state.queue.is_empty() && state.active == 0 {
                inner.idle.notify_waiters();
            }
            if state.stopping && jobs.is_empty() {
                break;
            }
        }
        tokio::select! {
            _ = inner.changed.notified() => {},
            result = jobs.join_next(), if !jobs.is_empty() => {
                if jobs.is_empty() { dismiss_in_flight = false; }
                match result {
                    Some(Ok(Ok(()))) | None => {},
                    Some(Ok(Err(error))) => tracing::error!(%error, "Discovery flow initialization failed"),
                    Some(Err(error)) => tracing::error!(%error, "Discovery callback worker failed"),
                }
            }
        }
    }
}

struct Completion {
    inner: Arc<Inner>,
    candidate: Option<Candidate>,
}
impl Drop for Completion {
    fn drop(&mut self) {
        let mut state = lock(&self.inner.state);
        state.active -= 1;
        if let Some(candidate) = &self.candidate {
            state.in_flight.retain(|existing| {
                existing.domain != candidate.domain
                    || existing.context != candidate.context
                    || existing.data != candidate.data
            });
        }
        drop(state);
        self.inner.idle.notify_waiters();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_config::{
        ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult, SetupOutcome,
    };
    use hass_core::{MemoryStore, Runtime};
    use hass_ssdp::SsdpServiceInfo;
    use std::{
        collections::BTreeSet,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    struct Driver {
        requests: Mutex<Vec<ConfigFlowRequest>>,
        create: bool,
        active: AtomicUsize,
        peak: AtomicUsize,
        delay: bool,
    }
    impl ConfigEntryDriver for Driver {
        fn domain(&self) -> &str {
            "discovery_fixture"
        }
        fn setup(&self, _: &Runtime, _: &ConfigEntry) -> Result<SetupOutcome> {
            Ok(SetupOutcome::Ready)
        }
        fn unload(&self, _: &Runtime, _: &ConfigEntry) -> Result<()> {
            Ok(())
        }
        fn config_flow(&self, request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            if self.delay {
                std::thread::sleep(Duration::from_millis(30));
            }
            lock(&self.requests).push(request.clone());
            self.active.fetch_sub(1, Ordering::SeqCst);
            if self.create {
                Ok(ConfigFlowResult::create_entry(
                    "Discovered fixture",
                    Map::new(),
                    request
                        .user_input
                        .as_ref()
                        .and_then(|data| data.get("ssdp_udn"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                ))
            } else {
                Ok(ConfigFlowResult::form("confirm", None))
            }
        }
    }

    fn fixture(create: bool, delay: bool) -> (ConfigEntries, Arc<Driver>, DiscoveryDispatcher) {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let entries = ConfigEntries::new(runtime).unwrap();
        let driver = Arc::new(Driver {
            requests: Mutex::new(Vec::new()),
            create,
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            delay,
        });
        entries.register_driver(driver.clone()).unwrap();
        let dispatcher = DiscoveryDispatcher::new(entries.clone());
        (entries, driver, dispatcher)
    }

    fn action(id: &str) -> DiscoveryAction {
        DiscoveryAction::Create {
            domain: "discovery_fixture".into(),
            udn: id.into(),
            info: Box::new(SsdpServiceInfo {
                ssdp_usn: format!("{id}::type"),
                ssdp_st: "type".into(),
                upnp: Map::new(),
                ssdp_location: Some(format!("http://device.invalid/{id}.xml")),
                ssdp_udn: Some(id.into()),
                ssdp_nt: None,
                ssdp_ext: None,
                ssdp_server: None,
                ssdp_headers: Map::new(),
                ssdp_all_locations: BTreeSet::new(),
                x_homeassistant_matching_domains: BTreeSet::new(),
            }),
        }
    }

    #[tokio::test]
    async fn startup_dedup_active_flow_matching_and_byebye_use_real_callbacks() {
        let (entries, driver, dispatcher) = fixture(false, false);
        let sink = dispatcher.sink();
        sink(action("uuid:one")).await.unwrap();
        sink(action("uuid:one")).await.unwrap();
        assert!(lock(&driver.requests).is_empty());
        dispatcher.mark_started().await;
        assert_eq!(lock(&driver.requests).len(), 1);
        let request = lock(&driver.requests)[0].clone();
        assert_eq!(request.source, "ssdp");
        assert_eq!(
            request.context["discovery_key"],
            json!({"domain":"ssdp","key":"uuid:one","version":1})
        );
        sink(action("uuid:one")).await.unwrap();
        dispatcher.wait_idle().await;
        assert_eq!(lock(&driver.requests).len(), 1);
        sink(DiscoveryAction::Dismiss {
            ssdp_st: "type".into(),
            ssdp_location: Some("http://device.invalid/uuid:one.xml".into()),
        })
        .await
        .unwrap();
        dispatcher.wait_idle().await;
        assert!(entries.flow_result(&request.flow_id).is_err());
        sink(action("uuid:one")).await.unwrap();
        dispatcher.wait_idle().await;
        assert_eq!(lock(&driver.requests).len(), 2);
        dispatcher.shutdown().await;
        assert!(sink(action("uuid:late")).await.is_err());
    }

    #[tokio::test]
    async fn native_creation_persists_the_ssdp_key_and_keeps_it_out_of_public_entries() {
        let (entries, driver, dispatcher) = fixture(true, false);
        dispatcher.sink()(action("uuid:stored")).await.unwrap();
        dispatcher.mark_started().await;
        assert_eq!(lock(&driver.requests).len(), 1);
        let key = DiscoveryKey {
            domain: "ssdp".into(),
            key: DiscoveryKeyValue::String("uuid:stored".into()),
            version: 1,
        };
        let found = entries.entries_for_discovery_key(&key);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].unique_id.as_deref(), Some("uuid:stored"));
        assert!(found[0].public_json().get("discovery_keys").is_none());
        dispatcher.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn byebye_then_alive_preserves_the_new_flow_in_queue_order() {
        let (entries, driver, dispatcher) = fixture(false, false);
        let sink = dispatcher.sink();
        sink(action("uuid:returning")).await.unwrap();
        dispatcher.mark_started().await;
        for generation in 1..=64 {
            sink(DiscoveryAction::Dismiss {
                ssdp_st: "type".into(),
                ssdp_location: Some("http://device.invalid/uuid:returning.xml".into()),
            })
            .await
            .unwrap();
            sink(action("uuid:returning")).await.unwrap();
            dispatcher.wait_idle().await;
            let requests = lock(&driver.requests).clone();
            assert_eq!(requests.len(), generation + 1);
            assert!(
                entries
                    .flow_result(&requests.last().unwrap().flow_id)
                    .is_ok()
            );
            assert!(
                entries
                    .flow_result(&requests[requests.len() - 2].flow_id)
                    .is_err()
            );
        }
        dispatcher.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_admits_at_most_twenty_and_shutdown_joins_blocking_callbacks() {
        let (_, driver, dispatcher) = fixture(false, true);
        let sink = dispatcher.sink();
        for index in 0..45 {
            sink(action(&format!("uuid:{index}"))).await.unwrap();
        }
        dispatcher.mark_started().await;
        assert_eq!(lock(&driver.requests).len(), 45);
        assert!((2..=20).contains(&driver.peak.load(Ordering::SeqCst)));
        for index in 45..60 {
            sink(action(&format!("uuid:{index}"))).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while driver.active.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        dispatcher.shutdown().await;
        assert_eq!(driver.active.load(Ordering::SeqCst), 0);
        let count = lock(&driver.requests).len();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(lock(&driver.requests).len(), count);
    }
}
