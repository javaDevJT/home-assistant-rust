// Public API regression tests for owned config-flow callback lifecycle.

use hass_config::{
    ConfigEntries, ConfigEntry, ConfigEntryDriver, ConfigFlowContinuation, ConfigFlowRequest,
    ConfigFlowResult, ConfigFlowType, ConfigSubentryFlowRequest, ConfigSubentryFlowResult,
    ConfigSubentryTypeInfo, NewConfigEntry, SetupOutcome,
};
use hass_core::{Error, MemoryStore, Result, Runtime};
use serde_json::{Map, Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

const DOMAIN: &str = "owned_lifecycle_fixture";
const CHILD_TYPE: &str = "owned_lifecycle_child";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(2);
const CALLBACK_FAILSAFE: Duration = Duration::from_secs(10);
const WAIT_TIMEOUT: Duration = Duration::from_secs(2);
const PENDING_CHECK: Duration = Duration::from_millis(75);

struct GatedCallback {
    started: mpsc::Sender<String>,
    released: (Mutex<bool>, Condvar),
}

impl GatedCallback {
    fn new() -> (Arc<Self>, mpsc::Receiver<String>) {
        let (started, receiver) = mpsc::channel();
        (
            Arc::new(Self {
                started,
                released: (Mutex::new(false), Condvar::new()),
            }),
            receiver,
        )
    }

    fn wait(&self, flow_id: &str) {
        let _ = self.started.send(flow_id.to_owned());
        let (released, condition) = &self.released;
        let deadline = Instant::now() + CALLBACK_FAILSAFE;
        let mut released = released.lock().unwrap_or_else(PoisonError::into_inner);
        while !*released {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (next, timeout) = condition
                .wait_timeout(released, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            released = next;
            if timeout.timed_out() && !*released {
                break;
            }
        }
    }

    fn release(&self) {
        let (released, condition) = &self.released;
        *released.lock().unwrap_or_else(PoisonError::into_inner) = true;
        condition.notify_all();
    }
}

struct ReleaseOnDrop(Arc<GatedCallback>);

impl ReleaseOnDrop {
    fn release(&self) {
        self.0.release();
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

async fn receive_started(receiver: mpsc::Receiver<String>) -> String {
    tokio::time::timeout(
        CALLBACK_TIMEOUT,
        tokio::task::spawn_blocking(move || receiver.recv_timeout(CALLBACK_TIMEOUT)),
    )
    .await
    .expect("timed out waiting for the blocking callback")
    .expect("callback start receiver panicked")
    .expect("blocking callback did not start")
}

async fn wait_until_inactive(manager: &ConfigEntries, flow_type: ConfigFlowType, flow_id: &str) {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            if manager.active_flow(flow_type, flow_id).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("flow did not leave the active public API");
}

async fn wait_until_shutdown_closed(manager: &ConfigEntries) {
    tokio::time::timeout(WAIT_TIMEOUT, async {
        loop {
            let error = manager
                .init_config_flow_async("shutdown_lifecycle_probe", "user", Map::new())
                .await
                .expect_err("the unregistered shutdown probe unexpectedly started");
            let message = error.to_string();
            if message.contains("shutting down") {
                return;
            }
            assert!(
                message.contains("no native integration driver registered"),
                "unexpected shutdown-probe error: {message}"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("shutdown did not close async-flow admission");
}

async fn assert_waiting<T>(task: &mut JoinHandle<T>, operation: &str) {
    assert!(
        tokio::time::timeout(PENDING_CHECK, task).await.is_err(),
        "{operation} returned while its blocking callback was still gated"
    );
}

fn runtime() -> Runtime {
    Runtime::new(Arc::new(MemoryStore::default())).unwrap()
}

fn object(key: &str, value: Value) -> Map<String, Value> {
    Map::from_iter([(key.to_owned(), value)])
}

struct LifecycleDriver {
    config_gate: Option<Arc<GatedCallback>>,
    options_gate: Option<Arc<GatedCallback>>,
    subentry_gate: Option<Arc<GatedCallback>>,
    on_create_gate: Option<Arc<GatedCallback>>,
    subentry_returns_form: bool,
    has_async_config_flow: bool,
    config_callback_calls: AtomicUsize,
    config_callback_exits: AtomicUsize,
    async_config_callback_calls: AtomicUsize,
    options_callback_calls: AtomicUsize,
    options_callback_exits: AtomicUsize,
    subentry_callback_calls: AtomicUsize,
    subentry_callback_exits: AtomicUsize,
    on_create_calls: AtomicUsize,
    sync_child_flow_id: Mutex<Option<String>>,
    sync_child_init_error: Mutex<Option<String>>,
}

impl LifecycleDriver {
    fn new(
        config_gate: Option<Arc<GatedCallback>>,
        options_gate: Option<Arc<GatedCallback>>,
        subentry_gate: Option<Arc<GatedCallback>>,
        has_async_config_flow: bool,
    ) -> Self {
        Self {
            config_gate,
            options_gate,
            subentry_gate,
            on_create_gate: None,
            subentry_returns_form: false,
            has_async_config_flow,
            config_callback_calls: AtomicUsize::new(0),
            config_callback_exits: AtomicUsize::new(0),
            async_config_callback_calls: AtomicUsize::new(0),
            options_callback_calls: AtomicUsize::new(0),
            options_callback_exits: AtomicUsize::new(0),
            subentry_callback_calls: AtomicUsize::new(0),
            subentry_callback_exits: AtomicUsize::new(0),
            on_create_calls: AtomicUsize::new(0),
            sync_child_flow_id: Mutex::new(None),
            sync_child_init_error: Mutex::new(None),
        }
    }
}

impl ConfigEntryDriver for LifecycleDriver {
    fn domain(&self) -> &str {
        DOMAIN
    }

    fn supports_options(&self) -> bool {
        true
    }

    fn setup(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<SetupOutcome> {
        Ok(SetupOutcome::Ready)
    }

    fn unload(&self, _runtime: &Runtime, _entry: &ConfigEntry) -> Result<()> {
        Ok(())
    }

    fn config_flow(&self, request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
        self.config_callback_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.config_gate {
            gate.wait(&request.flow_id);
        }
        self.config_callback_exits.fetch_add(1, Ordering::SeqCst);

        Ok(ConfigFlowResult::create_entry(
            "Must not be created after abort",
            object("marker", json!("config-abort")),
            Some("config-abort-unique-id".into()),
        ))
    }

    fn config_flow_async(
        &self,
        _request: ConfigFlowRequest,
    ) -> Option<Pin<Box<dyn Future<Output = Result<ConfigFlowResult>> + Send + 'static>>> {
        if !self.has_async_config_flow {
            return None;
        }
        self.async_config_callback_calls
            .fetch_add(1, Ordering::SeqCst);
        Some(Box::pin(async {
            Ok(ConfigFlowResult::form("wrong_config_callback", None))
        }))
    }

    fn on_create_entry(
        &self,
        manager: &ConfigEntries,
        request: &ConfigFlowRequest,
        entry: &ConfigEntry,
        _result: &ConfigFlowResult,
    ) -> Result<Option<ConfigFlowContinuation>> {
        self.on_create_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.on_create_gate {
            gate.wait(&request.flow_id);
        }
        let child_flow = match manager.init_subentry_flow(
            &entry.entry_id,
            CHILD_TYPE,
            "user",
            None,
            Map::new(),
            None,
        ) {
            Ok(child_flow) => child_flow,
            Err(error) => {
                *self
                    .sync_child_init_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(error.to_string());
                return Ok(None);
            }
        };
        let child_flow_id = child_flow
            .get("flow_id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::InvalidInput("child flow has no flow_id".into()))?;
        *self
            .sync_child_flow_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(child_flow_id.to_owned());
        Ok(Some(ConfigFlowContinuation::config_subentries_flow(
            child_flow_id,
        )))
    }

    fn supported_subentry_types(
        &self,
        _entry: &ConfigEntry,
    ) -> std::collections::BTreeMap<String, ConfigSubentryTypeInfo> {
        std::collections::BTreeMap::from([(
            CHILD_TYPE.to_owned(),
            ConfigSubentryTypeInfo::default(),
        )])
    }

    fn subentry_flow(
        &self,
        request: &ConfigSubentryFlowRequest,
    ) -> Result<ConfigSubentryFlowResult> {
        self.subentry_callback_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.subentry_gate {
            gate.wait(&request.flow_id);
        }
        self.subentry_callback_exits.fetch_add(1, Ordering::SeqCst);
        if self.subentry_returns_form {
            return Ok(ConfigSubentryFlowResult::Form {
                step_id: "user".into(),
                data_schema: None,
                errors: None,
                description_placeholders: None,
            });
        }
        Ok(ConfigSubentryFlowResult::CreateEntry {
            title: "Must not be created after shutdown".into(),
            data: object("marker", json!("subentry-shutdown")),
            unique_id: Some("subentry-shutdown-unique-id".into()),
        })
    }

    fn options_flow(&self, request: &ConfigFlowRequest) -> Result<ConfigFlowResult> {
        self.options_callback_calls.fetch_add(1, Ordering::SeqCst);
        let Some(options) = request.user_input.clone() else {
            return Ok(ConfigFlowResult::form("options", None));
        };
        if let Some(gate) = &self.options_gate {
            gate.wait(&request.flow_id);
        }
        self.options_callback_exits.fetch_add(1, Ordering::SeqCst);
        Ok(ConfigFlowResult::CreateEntry {
            title: "Options update".into(),
            data: Map::new(),
            options,
            unique_id: request.unique_id.clone(),
            version: 1,
            minor_version: 1,
        })
    }
}

fn register(manager: &ConfigEntries, driver: Arc<LifecycleDriver>) {
    let registered: Arc<dyn ConfigEntryDriver> = driver;
    manager.register_driver(registered).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canceled_config_init_abort_joins_callback_and_suppresses_entry_and_child_creation() {
    let manager = ConfigEntries::new(runtime()).unwrap();
    let (gate, started) = GatedCallback::new();
    let release = ReleaseOnDrop(gate.clone());
    let driver = Arc::new(LifecycleDriver::new(Some(gate), None, None, false));
    register(&manager, driver.clone());

    let caller_manager = manager.clone();
    let caller = tokio::spawn(async move {
        caller_manager
            .init_config_flow_async(DOMAIN, "user", Map::new())
            .await
    });
    let flow_id = receive_started(started).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());

    let abort_manager = manager.clone();
    let abort_flow_id = flow_id.clone();
    let mut abort =
        tokio::spawn(async move { abort_manager.abort_flow_async(&abort_flow_id).await });
    wait_until_inactive(&manager, ConfigFlowType::Config, &flow_id).await;
    assert_eq!(driver.config_callback_exits.load(Ordering::SeqCst), 0);
    assert_waiting(&mut abort, "abort_flow_async").await;

    release.release();
    tokio::time::timeout(WAIT_TIMEOUT, abort)
        .await
        .expect("abort did not finish after the callback exited")
        .expect("abort task panicked")
        .expect("abort failed for the active flow");

    assert_eq!(driver.config_callback_exits.load(Ordering::SeqCst), 1);
    assert!(manager.list_entries(Some(DOMAIN)).is_empty());
    assert_eq!(driver.on_create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(driver.subentry_callback_calls.load(Ordering::SeqCst), 0);
    manager.shutdown_flow_tasks_async().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canceled_options_submit_abort_joins_callback_without_updating_entry() {
    let manager = ConfigEntries::new(runtime()).unwrap();
    let (gate, started) = GatedCallback::new();
    let release = ReleaseOnDrop(gate.clone());
    let driver = Arc::new(LifecycleDriver::new(None, Some(gate), None, true));
    register(&manager, driver.clone());

    let mut seed = NewConfigEntry::new(DOMAIN, "Existing options entry", Map::new());
    seed.options = object("polling", json!(30));
    let entry = manager.create_entry(seed).unwrap();
    let initial = manager
        .init_options_flow_async(&entry.entry_id)
        .await
        .unwrap();
    let flow_id = initial["flow_id"].as_str().unwrap().to_owned();

    tokio::task::LocalSet::new()
        .run_until(async {
            let submit_manager = manager.clone();
            let submit_flow_id = flow_id.clone();
            let submit = tokio::task::spawn_local(async move {
                submit_manager
                    .submit_flow_async(&submit_flow_id, object("polling", json!(90)))
                    .await
            });
            let started_flow_id = receive_started(started).await;
            assert_eq!(started_flow_id, flow_id);
            submit.abort();
            assert!(submit.await.unwrap_err().is_cancelled());

            let abort_manager = manager.clone();
            let abort_flow_id = flow_id.clone();
            let mut abort =
                tokio::spawn(async move { abort_manager.abort_flow_async(&abort_flow_id).await });
            wait_until_inactive(&manager, ConfigFlowType::Options, &flow_id).await;
            assert_eq!(driver.options_callback_exits.load(Ordering::SeqCst), 0);
            assert_waiting(&mut abort, "abort_flow_async for options submit").await;

            release.release();
            tokio::time::timeout(WAIT_TIMEOUT, abort)
                .await
                .expect("options abort did not finish after the callback exited")
                .expect("options abort task panicked")
                .expect("options abort failed for the active flow");
        })
        .await;

    assert_eq!(driver.options_callback_exits.load(Ordering::SeqCst), 1);
    assert_eq!(
        manager.get_entry(&entry.entry_id).unwrap().options,
        entry.options
    );
    assert_eq!(driver.options_callback_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        driver.async_config_callback_calls.load(Ordering::SeqCst),
        0,
        "options transitions must call options_flow even when config_flow_async exists"
    );
    manager.shutdown_flow_tasks_async().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canceled_shutdown_waiter_shares_drain_and_rejects_new_async_flows() {
    let manager = ConfigEntries::new(runtime()).unwrap();
    let (gate, started) = GatedCallback::new();
    let release = ReleaseOnDrop(gate.clone());
    let driver = Arc::new(LifecycleDriver::new(None, None, Some(gate), false));
    register(&manager, driver.clone());
    let entry = manager
        .create_entry(NewConfigEntry::new(DOMAIN, "Existing entry", Map::new()))
        .unwrap();

    let init_manager = manager.clone();
    let entry_id = entry.entry_id.clone();
    let init = tokio::spawn(async move {
        init_manager
            .init_subentry_flow_async(&entry_id, CHILD_TYPE, "user", None, Map::new(), None)
            .await
    });
    let flow_id = receive_started(started).await;
    init.abort();
    assert!(init.await.unwrap_err().is_cancelled());

    let first_manager = manager.clone();
    let mut first_waiter =
        tokio::spawn(async move { first_manager.shutdown_flow_tasks_async().await });
    wait_until_inactive(&manager, ConfigFlowType::ConfigSubentries, &flow_id).await;
    assert_eq!(driver.subentry_callback_exits.load(Ordering::SeqCst), 0);
    assert_waiting(&mut first_waiter, "first shutdown waiter").await;

    let second_manager = manager.clone();
    let mut second_waiter =
        tokio::spawn(async move { second_manager.shutdown_flow_tasks_async().await });
    assert_waiting(&mut second_waiter, "concurrent shutdown waiter").await;
    first_waiter.abort();
    assert!(first_waiter.await.unwrap_err().is_cancelled());

    assert!(
        manager
            .init_config_flow_async(DOMAIN, "user", Map::new())
            .await
            .is_err()
    );
    assert!(
        manager
            .init_options_flow_async(&entry.entry_id)
            .await
            .is_err()
    );
    assert!(
        manager
            .init_subentry_flow_async(&entry.entry_id, CHILD_TYPE, "user", None, Map::new(), None,)
            .await
            .is_err()
    );
    assert_eq!(driver.subentry_callback_exits.load(Ordering::SeqCst), 0);
    assert_waiting(&mut second_waiter, "surviving shutdown waiter").await;

    release.release();
    let drained = tokio::time::timeout(WAIT_TIMEOUT, second_waiter)
        .await
        .expect("shared shutdown drain did not finish after the callback exited")
        .expect("surviving shutdown waiter panicked")
        .expect("shared shutdown drain failed");

    assert_eq!(drained, 1);
    assert_eq!(driver.subentry_callback_exits.load(Ordering::SeqCst), 1);
    assert!(manager.list_subentries(&entry.entry_id).unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_joins_blocked_create_hook_and_rejects_late_sync_child_flow() {
    let manager = ConfigEntries::new(runtime()).unwrap();
    let (gate, started) = GatedCallback::new();
    let release = ReleaseOnDrop(gate.clone());
    let mut fixture = LifecycleDriver::new(None, None, None, false);
    fixture.on_create_gate = Some(gate);
    fixture.subentry_returns_form = true;
    let driver = Arc::new(fixture);
    register(&manager, driver.clone());

    let init_manager = manager.clone();
    let mut init = tokio::spawn(async move {
        init_manager
            .init_config_flow_async(DOMAIN, "user", Map::new())
            .await
    });
    let flow_id = receive_started(started).await;
    assert_waiting(&mut init, "config init while on_create_entry is blocked").await;

    let entries = manager.list_entries(Some(DOMAIN));
    assert_eq!(
        entries.len(),
        1,
        "the create result should already be persisted"
    );
    let entry_id = entries[0].entry_id.clone();
    assert!(manager.list_subentries(&entry_id).unwrap().is_empty());

    let shutdown_manager = manager.clone();
    let mut shutdown =
        tokio::spawn(async move { shutdown_manager.shutdown_flow_tasks_async().await });
    wait_until_shutdown_closed(&manager).await;
    assert_waiting(&mut shutdown, "shutdown while on_create_entry is blocked").await;
    assert!(
        manager
            .active_flow(ConfigFlowType::Config, &flow_id)
            .is_err()
    );

    release.release();
    tokio::time::timeout(WAIT_TIMEOUT, shutdown)
        .await
        .expect("shutdown returned before the create hook exited")
        .expect("shutdown task panicked")
        .expect("shutdown drain failed");
    tokio::time::timeout(WAIT_TIMEOUT, init)
        .await
        .expect("config init did not finish after its create hook exited")
        .expect("config init task panicked")
        .expect("already-persisted config creation failed after its hook exited");

    let child_init_error = driver
        .sync_child_init_error
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .expect("on_create_entry did not attempt its synchronous child flow");
    assert!(
        child_init_error.contains("shutting down"),
        "late synchronous child init failed for an unexpected reason: {child_init_error}"
    );
    assert!(
        driver
            .sync_child_flow_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none(),
        "a child flow was admitted after shutdown began"
    );
    assert_eq!(driver.on_create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(driver.subentry_callback_calls.load(Ordering::SeqCst), 0);
    assert_eq!(manager.list_entries(Some(DOMAIN)).len(), 1);
    assert!(manager.list_subentries(&entry_id).unwrap().is_empty());
    assert!(
        manager
            .active_flow(ConfigFlowType::Config, &flow_id)
            .is_err()
    );
}
