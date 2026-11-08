use super::*;

struct ReleaseAnotherOwner {
    runtime: Weak<Runtime>,
    dropped: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for ReleaseAnotherOwner {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.upgrade() {
            runtime.unregister_owned_services("nested").unwrap();
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn empty_handler() -> ServiceHandler {
    Arc::new(|_, call| {
        Ok(ServiceResult {
            changed_states: Vec::new(),
            response: None,
            context: call.context.clone(),
        })
    })
}

#[test]
fn service_callback_destructors_can_reenter_registration_during_remove_and_replace() {
    for operation in 0..4 {
        let runtime = Arc::new(Runtime::new(Arc::new(MemoryStore::default())).unwrap());
        runtime
            .register_owned_service("nested", "demo", "nested", Value::Null, empty_handler())
            .unwrap();
        let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release = ReleaseAnotherOwner {
            runtime: Arc::downgrade(&runtime),
            dropped: dropped.clone(),
        };
        let handler: ServiceHandler = Arc::new(move |_, call| {
            let _keep_alive = &release;
            Ok(ServiceResult {
                changed_states: Vec::new(),
                response: None,
                context: call.context.clone(),
            })
        });
        if operation < 2 {
            runtime
                .register_owned_service("outer", "demo", "outer", Value::Null, handler)
                .unwrap();
        } else {
            runtime
                .register_entity_service(
                    "outer",
                    "demo",
                    "outer",
                    Value::Null,
                    Arc::new(|_| true),
                    handler,
                )
                .unwrap();
        }
        let worker_runtime = runtime.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            match operation {
                0 => {
                    worker_runtime.unregister_owned_services("outer").unwrap();
                }
                1 => {
                    worker_runtime
                        .register_owned_service(
                            "outer",
                            "demo",
                            "outer",
                            Value::Null,
                            empty_handler(),
                        )
                        .unwrap();
                }
                2 => {
                    worker_runtime.unregister_entity_services("outer").unwrap();
                }
                _ => {
                    worker_runtime
                        .register_entity_service(
                            "outer",
                            "demo",
                            "outer",
                            Value::Null,
                            Arc::new(|_| true),
                            empty_handler(),
                        )
                        .unwrap();
                }
            }
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("callback destructor deadlocked inside a service registry lock");
        worker.join().unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.service_response_support("demo", "nested"), None);
    }
}

#[test]
fn service_request_snapshots_capability_and_handlers_in_one_registration_generation() {
    let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let old_calls = calls.clone();
    runtime
        .register_owned_service_with_response_support(
            "old",
            "light",
            "handoff",
            Value::Null,
            ServiceResponseSupport::Optional,
            Arc::new(move |_, call| {
                old_calls.fetch_add(1, Ordering::SeqCst);
                Ok(ServiceResult {
                    changed_states: Vec::new(),
                    response: None,
                    context: call.context.clone(),
                })
            }),
        )
        .unwrap();
    let registration = runtime.inner.service_registration.lock().unwrap();
    let reader_runtime = runtime.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        started_tx.send(()).unwrap();
        let result = executor.block_on(reader_runtime.call_service_request_async(
            ServiceCallRequest {
                call: ServiceCall {
                    domain: "light".into(),
                    service: "handoff".into(),
                    data: Map::new(),
                    target: Map::from_iter([(
                        "entity_id".into(),
                        Value::String("light.one".into()),
                    )]),
                    context: Context::default(),
                },
                return_response: false,
                authorize_targets: None,
            },
        ));
        result_tx.send(result).unwrap();
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
    let early = result_rx
        .recv_timeout(std::time::Duration::from_millis(25))
        .ok();
    // Model an owner handoff while the registry's common writer lock is held.
    // The request must see this complete generation after the lock releases.
    runtime.inner.services.write().unwrap().remove("light");
    let new_calls = calls.clone();
    runtime.inner.entity_services.write().unwrap().insert(
        ("light".into(), "handoff".into()),
        BTreeMap::from_iter([(
            "new".into(),
            RegisteredEntityService {
                description: Value::Null,
                response_support: ServiceResponseSupport::Only,
                matcher: Arc::new(|id| id == "light.one"),
                handler: ServiceHandlerKind::Sync(Arc::new(move |_, call| {
                    new_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(ServiceResult {
                        changed_states: Vec::new(),
                        response: Some(Value::Null),
                        context: call.context.clone(),
                    })
                })),
            },
        )]),
    );
    drop(registration);
    let result = match early {
        Some(result) => result,
        None => result_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap(),
    };
    reader.join().unwrap();
    assert!(
        matches!(result, Err(Error::ServiceValidation(message)) if message.contains("requires responses"))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
