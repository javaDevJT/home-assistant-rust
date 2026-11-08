use std::sync::{Arc, Mutex};

use hass_core::{
    Context, MemoryStore, Runtime, ServiceCall, ServiceFuture, ServiceResult, UnitSystemKind,
};
use serde_json::{Map, Value, json};

fn new_runtime() -> Runtime {
    Runtime::new(Arc::new(MemoryStore::default())).expect("runtime should initialize")
}

fn service_call(domain: &str, service: &str, entity_ids: &[&str], context_id: &str) -> ServiceCall {
    let target = if entity_ids.is_empty() {
        Map::new()
    } else {
        json!({"entity_id": entity_ids})
            .as_object()
            .expect("target is an object")
            .clone()
    };
    ServiceCall {
        domain: domain.to_owned(),
        service: service.to_owned(),
        data: Map::new(),
        target,
        context: Context {
            id: context_id.to_owned(),
            parent_id: None,
            user_id: None,
        },
    }
}

fn result(context: Context) -> ServiceResult {
    ServiceResult {
        changed_states: Vec::new(),
        response: None,
        context,
    }
}

#[test]
fn unit_system_is_shared_by_clones_and_independent_between_runtimes() {
    let runtime = new_runtime();
    let clone = runtime.clone();
    let fresh_runtime = new_runtime();

    assert_eq!(runtime.unit_system(), UnitSystemKind::Metric);
    runtime.set_unit_system(UnitSystemKind::UsCustomary);
    assert_eq!(clone.unit_system(), UnitSystemKind::UsCustomary);
    assert_eq!(fresh_runtime.unit_system(), UnitSystemKind::Metric);
    assert_eq!(UnitSystemKind::Metric.unit_system().name(), "metric");
    assert_eq!(
        UnitSystemKind::UsCustomary.unit_system().name(),
        "us_customary"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn canceled_pending_call_does_not_leak_claims_to_the_thread_or_next_call() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let runtime = new_runtime();
    let constructions = Arc::new(Mutex::new(Vec::new()));
    let observations = Arc::clone(&constructions);
    runtime
        .register_async_service(
            "test",
            "pending",
            Value::Null,
            Arc::new(move |runtime, _| {
                observations
                    .lock()
                    .unwrap()
                    .push(runtime.claim_service_diagnostic("color", "invalid"));
                let runtime = runtime.clone();
                let future: ServiceFuture = Box::pin(poll_fn(move |_| {
                    assert!(!runtime.claim_service_diagnostic("color", "invalid"));
                    Poll::Pending
                }));
                future
            }),
        )
        .unwrap();

    for _ in 0..2 {
        let mut pending = Box::pin(runtime.call_service_async(service_call(
            "test",
            "pending",
            &[],
            "same-context",
        )));
        poll_fn(|cx| {
            assert!(pending.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(pending);
        assert!(runtime.claim_service_diagnostic("color", "invalid"));
        assert!(runtime.claim_service_diagnostic("color", "invalid"));
    }
    assert_eq!(*constructions.lock().unwrap(), vec![true, true]);
}

#[tokio::test]
async fn async_provider_groups_share_claims_across_construction_and_polls() {
    let runtime = new_runtime();
    let observations = Arc::new(Mutex::new(Vec::new()));

    for (owner, entity_id) in [("provider-a", "light.one"), ("provider-b", "light.two")] {
        let observations = Arc::clone(&observations);
        let owner = owner.to_owned();
        let registration_owner = owner.clone();
        let callback_owner = owner.clone();
        let entity_id = entity_id.to_owned();
        runtime
            .register_entity_async_service(
                &registration_owner,
                "light",
                "warn",
                Value::Null,
                Arc::new(move |candidate| candidate == entity_id),
                Arc::new(move |runtime, call| {
                    let construction_claim =
                        runtime.claim_service_diagnostic("invalid_color", "Warm White");
                    let runtime_after_await = runtime.clone();
                    let observations = Arc::clone(&observations);
                    let owner = callback_owner.clone();
                    let context = call.context.clone();
                    let future: ServiceFuture = Box::pin(async move {
                        tokio::task::yield_now().await;
                        let after_await_claim = runtime_after_await
                            .claim_service_diagnostic("invalid_color", "Warm White");
                        observations.lock().unwrap().push((
                            owner,
                            construction_claim,
                            after_await_claim,
                        ));
                        Ok(result(context))
                    });
                    future
                }),
            )
            .unwrap();
    }

    for _ in 0..2 {
        runtime
            .call_service_async(service_call(
                "light",
                "warn",
                &["light.one", "light.two"],
                "reused-context-id",
            ))
            .await
            .unwrap();
    }

    assert_eq!(
        *observations.lock().unwrap(),
        vec![
            ("provider-a".to_owned(), true, false),
            ("provider-b".to_owned(), false, false),
            ("provider-a".to_owned(), true, false),
            ("provider-b".to_owned(), false, false),
        ]
    );
    assert!(runtime.claim_service_diagnostic("invalid_color", "Warm White"));
    assert!(runtime.claim_service_diagnostic("invalid_color", "Warm White"));
}

#[tokio::test]
async fn nested_owned_blocking_call_uses_fresh_claims_and_restores_parent() {
    let runtime = new_runtime();
    let observations = Arc::new(Mutex::new(Vec::new()));

    let child_observations = Arc::clone(&observations);
    runtime
        .register_service(
            "test",
            "child",
            Value::Null,
            Arc::new(move |runtime, call| {
                let first = runtime.claim_service_diagnostic("invalid_color", "Warm White");
                let second = runtime.claim_service_diagnostic("invalid_color", "Warm White");
                child_observations
                    .lock()
                    .unwrap()
                    .push(("owned-child".to_owned(), first, second));
                Ok(result(call.context.clone()))
            }),
        )
        .unwrap();

    let parent_observations = Arc::clone(&observations);
    runtime
        .register_async_service(
            "test",
            "parent",
            Value::Null,
            Arc::new(move |runtime, call| {
                let construction_claim =
                    runtime.claim_service_diagnostic("invalid_color", "Warm White");
                parent_observations.lock().unwrap().push((
                    "parent-construction".to_owned(),
                    construction_claim,
                    false,
                ));
                let nested_runtime = runtime.clone();
                let observations = Arc::clone(&parent_observations);
                let context = call.context.clone();
                let child_context_id = context.id.clone();
                let future: ServiceFuture = Box::pin(async move {
                    tokio::task::yield_now().await;
                    let before_child =
                        nested_runtime.claim_service_diagnostic("invalid_color", "Warm White");
                    observations.lock().unwrap().push((
                        "parent-before-child".to_owned(),
                        before_child,
                        false,
                    ));

                    let mut child = nested_runtime
                        .spawn_service_call_async(service_call(
                            "test",
                            "child",
                            &[],
                            &child_context_id,
                        ))
                        .unwrap();
                    child.result().await?;

                    let after_child =
                        nested_runtime.claim_service_diagnostic("invalid_color", "Warm White");
                    observations.lock().unwrap().push((
                        "parent-after-child".to_owned(),
                        after_child,
                        false,
                    ));
                    Ok(result(context))
                });
                future
            }),
        )
        .unwrap();

    runtime
        .call_service_async(service_call("test", "parent", &[], "reused-context-id"))
        .await
        .unwrap();
    runtime.shutdown_service_tasks_async().await.unwrap();

    assert_eq!(
        *observations.lock().unwrap(),
        vec![
            ("parent-construction".to_owned(), true, false),
            ("parent-before-child".to_owned(), false, false),
            ("owned-child".to_owned(), true, false),
            ("parent-after-child".to_owned(), false, false),
        ]
    );
}
