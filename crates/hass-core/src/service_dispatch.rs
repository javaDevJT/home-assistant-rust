//! Shared sync and async service preparation and dispatch.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use crate::service_tasks;
use crate::{
    Context, Error, Result, Runtime, ServiceCall, ServiceCallRequest, ServiceExecutionScope,
    ServiceHandlerKind, ServiceRegistrations, ServiceResponseSupport, ServiceResult,
    ServiceTargetAuthorizer, entity_domain, parse_service_target, validate_component,
    validate_entity_id,
};

impl Runtime {
    /// Return the typed response capability for a registered exact service key.
    pub fn service_response_support(
        &self,
        domain: &str,
        service: &str,
    ) -> Option<ServiceResponseSupport> {
        let _registration = self
            .inner
            .service_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let legacy = self
            .inner
            .services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(domain)
            .and_then(|entries| entries.get(service))
            .map(|registered| registered.response_support);
        legacy.or_else(|| {
            self.inner
                .entity_services
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&(domain.to_owned(), service.to_owned()))
                .and_then(|providers| providers.first_key_value())
                .map(|(_, provider)| provider.response_support)
        })
    }

    /// Invoke a registered service without holding the registry lock.
    pub fn call_service(&self, call: ServiceCall) -> Result<ServiceResult> {
        let registrations = self.snapshot_service_registrations(&call.domain, &call.service)?;
        let prepared = self.prepare_service_call(call, registrations, None)?;
        self.invoke_prepared_service_sync(prepared)
    }

    /// Invoke a service asynchronously. Async handlers are awaited directly;
    /// synchronous handlers use the runtime's shared blocking pool.
    pub async fn call_service_async(&self, call: ServiceCall) -> Result<ServiceResult> {
        self.call_service_async_with_scope(call, None).await
    }

    /// Invoke a trusted call with an execution-local scope installed for each
    /// sync invocation and each poll of an async handler.
    pub async fn call_service_async_scoped(
        &self,
        call: ServiceCall,
        scope: ServiceExecutionScope,
    ) -> Result<ServiceResult> {
        self.call_service_async_with_scope(call, Some(scope)).await
    }

    /// Invoke a transport request and check its response choice before target
    /// resolution or handler dispatch.
    pub async fn call_service_request_async(
        &self,
        request: ServiceCallRequest,
    ) -> Result<ServiceResult> {
        self.call_service_request_async_with_scope(request, None)
            .await
    }

    /// Apply response and target preflight to the same registration snapshot
    /// while preserving a script's execution-local handler scope.
    pub async fn call_service_request_async_scoped(
        &self,
        request: ServiceCallRequest,
        scope: ServiceExecutionScope,
    ) -> Result<ServiceResult> {
        self.call_service_request_async_with_scope(request, Some(scope))
            .await
    }

    async fn call_service_request_async_with_scope(
        &self,
        request: ServiceCallRequest,
        scope: Option<ServiceExecutionScope>,
    ) -> Result<ServiceResult> {
        let registrations =
            self.snapshot_service_registrations(&request.call.domain, &request.call.service)?;
        match (registrations.response_support(), request.return_response) {
            (ServiceResponseSupport::None, true) => {
                return Err(Error::ServiceValidation(
                    "Service does not support responses. Remove return_response from request."
                        .into(),
                ));
            }
            (ServiceResponseSupport::Only, false) => {
                return Err(Error::ServiceValidation(
                    "Service call requires responses but caller did not ask for responses. Add ?return_response to query parameters."
                        .into(),
                ));
            }
            _ => {}
        }
        let prepared =
            self.prepare_service_call(request.call, registrations, request.authorize_targets)?;
        let mut result = self.invoke_prepared_service_async(prepared, scope).await?;
        if !request.return_response {
            result.response = None;
        }
        Ok(result)
    }

    /// Close synchronous service-task admission and join every admitted
    /// blocking handler. Concurrent and canceled waiters share one drain.
    pub async fn shutdown_service_tasks_async(&self) -> Result<()> {
        self.inner.service_tasks.shutdown().await
    }

    async fn call_service_async_with_scope(
        &self,
        call: ServiceCall,
        scope: Option<ServiceExecutionScope>,
    ) -> Result<ServiceResult> {
        let registrations = self.snapshot_service_registrations(&call.domain, &call.service)?;
        let prepared = self.prepare_service_call(call, registrations, None)?;
        self.invoke_prepared_service_async(prepared, scope).await
    }

    fn snapshot_service_registrations(
        &self,
        domain: &str,
        service: &str,
    ) -> Result<ServiceRegistrations> {
        validate_component(domain, "service domain")?;
        validate_component(service, "service name")?;
        // Capability validation and selected handlers must describe the same
        // owner generation, including transitions between the two registries.
        let _registration = self
            .inner
            .service_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let legacy = self
            .inner
            .services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(domain)
            .and_then(|entries| entries.get(service))
            .cloned();
        let providers = self
            .inner
            .entity_services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(domain.to_owned(), service.to_owned()))
            .cloned()
            .unwrap_or_default();
        if legacy.is_none() && providers.is_empty() {
            return Err(Error::Service(format!(
                "unknown service {domain}.{service}"
            )));
        }
        Ok(ServiceRegistrations { legacy, providers })
    }
}

impl Runtime {
    fn prepare_service_call(
        &self,
        mut call: ServiceCall,
        registrations: ServiceRegistrations,
        authorize_targets: Option<ServiceTargetAuthorizer>,
    ) -> Result<PreparedServiceCall> {
        let original_domain = call.domain.clone();
        let original_service = call.service.clone();
        let original_context = call.context.clone();
        let resolver = self
            .inner
            .service_target_resolver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|installed| Arc::clone(&installed.resolver));
        if let Some(resolver) = resolver
            && !(resolver)(self, &mut call)?
        {
            return Ok(PreparedServiceCall::Immediate(ServiceResult {
                changed_states: Vec::new(),
                response: None,
                context: original_context,
            }));
        }
        if call.domain != original_domain || call.service != original_service {
            return Err(Error::InvalidInput(
                "service target resolver cannot change the service name".into(),
            ));
        }
        // Resolver callbacks normalize targets only. Preserve the caller's
        // original causal context for every handler and aggregate result.
        call.context = original_context.clone();

        if registrations.providers.is_empty() {
            let handler = registrations.legacy.ok_or_else(|| {
                Error::Service(format!("unknown service {}.{}", call.domain, call.service))
            })?;
            authorize_legacy_targets(self, &call, authorize_targets.as_ref())?;
            return Ok(PreparedServiceCall::Legacy {
                handler: handler.handler,
                call,
            });
        }
        if !call.target.contains_key("entity_id")
            && !call.data.contains_key("entity_id")
            && let Some(handler) = registrations.legacy
        {
            authorize_legacy_targets(self, &call, authorize_targets.as_ref())?;
            return Ok(PreparedServiceCall::Legacy {
                handler: handler.handler,
                call,
            });
        }
        let Some(requested_ids) = parse_service_target(&call)? else {
            if let Some(handler) = registrations.legacy {
                authorize_legacy_targets(self, &call, authorize_targets.as_ref())?;
                return Ok(PreparedServiceCall::Legacy {
                    handler: handler.handler,
                    call,
                });
            }
            return Err(Error::ServiceValidation(format!(
                "service {}.{} requires an entity target",
                call.domain, call.service
            )));
        };

        let all_requested = requested_ids.len() == 1 && requested_ids[0] == "all";
        let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if all_requested {
            let mut states = self.states();
            states.retain(|state| entity_domain(&state.entity_id) == call.domain);
            for state in states {
                let owners: Vec<_> = registrations
                    .providers
                    .iter()
                    .filter(|(_, provider)| (provider.matcher)(&state.entity_id))
                    .map(|(owner, _)| owner.clone())
                    .collect();
                match owners.as_slice() {
                    [owner] => {
                        validate_entity_id(&state.entity_id)?;
                        grouped
                            .entry(owner.clone())
                            .or_default()
                            .push(state.entity_id);
                    }
                    [] => {}
                    _ => {
                        return Err(Error::Service(format!(
                            "entity {} matches multiple providers for {}.{}",
                            state.entity_id, call.domain, call.service
                        )));
                    }
                }
            }
            if grouped.is_empty() {
                return Err(Error::ServiceValidation(format!(
                    "target all matched no current entities for {}.{}",
                    call.domain, call.service
                )));
            }
        } else {
            for entity_id in requested_ids {
                validate_entity_id(&entity_id)?;
                let owners: Vec<_> = registrations
                    .providers
                    .iter()
                    .filter(|(_, provider)| (provider.matcher)(&entity_id))
                    .map(|(owner, _)| owner.clone())
                    .collect();
                match owners.as_slice() {
                    [owner] => grouped.entry(owner.clone()).or_default().push(entity_id),
                    [] => {
                        return Err(Error::ServiceValidation(format!(
                            "no provider for target {entity_id} on {}.{}",
                            call.domain, call.service
                        )));
                    }
                    _ => {
                        return Err(Error::ServiceValidation(format!(
                            "entity {entity_id} matches multiple providers for {}.{}",
                            call.domain, call.service
                        )));
                    }
                }
            }
        }
        let normalized_ids: Vec<_> = grouped.values().flatten().cloned().collect();
        if let Some(authorize_targets) = authorize_targets {
            authorize_targets(&normalized_ids)?;
        }

        // Every requested entity was validated, assigned, and authorized before
        // any provider callback can run.
        let mut handlers = Vec::with_capacity(grouped.len());
        for (owner, entity_ids) in grouped {
            let provider = registrations
                .providers
                .get(&owner)
                .expect("grouped owners came from registered providers");
            let mut data = call.data.clone();
            data.remove("entity_id");
            let subcall = ServiceCall {
                domain: call.domain.clone(),
                service: call.service.clone(),
                data,
                target: serde_json::json!({"entity_id": entity_ids})
                    .as_object()
                    .expect("service target is an object")
                    .clone(),
                context: call.context.clone(),
            };
            handlers.push((provider.handler.clone(), subcall));
        }
        Ok(PreparedServiceCall::Entity {
            handlers,
            context: call.context,
        })
    }
}

enum PreparedServiceCall {
    Immediate(ServiceResult),
    Legacy {
        handler: ServiceHandlerKind,
        call: ServiceCall,
    },
    Entity {
        handlers: Vec<(ServiceHandlerKind, ServiceCall)>,
        context: Context,
    },
}

impl Runtime {
    fn invoke_prepared_service_sync(&self, prepared: PreparedServiceCall) -> Result<ServiceResult> {
        match prepared {
            PreparedServiceCall::Immediate(result) => Ok(result),
            PreparedServiceCall::Legacy { handler, call } => match handler {
                ServiceHandlerKind::Sync(handler) => handler(self, &call),
                ServiceHandlerKind::Async(_) => Err(async_service_required(&call)),
            },
            PreparedServiceCall::Entity { handlers, context } => {
                if let Some((_, call)) = handlers
                    .iter()
                    .find(|(handler, _)| matches!(handler, ServiceHandlerKind::Async(_)))
                {
                    return Err(async_service_required(call));
                }
                let mut changed_states = Vec::new();
                let mut responses = Vec::new();
                for (handler, call) in handlers {
                    let ServiceHandlerKind::Sync(handler) = handler else {
                        unreachable!("async handlers were rejected before dispatch")
                    };
                    let result = handler(self, &call)?;
                    changed_states.extend(result.changed_states);
                    if let Some(response) = result.response {
                        responses.push(response);
                    }
                }
                Ok(aggregate_service_results(
                    changed_states,
                    responses,
                    context,
                ))
            }
        }
    }

    async fn invoke_prepared_service_async(
        &self,
        prepared: PreparedServiceCall,
        scope: Option<ServiceExecutionScope>,
    ) -> Result<ServiceResult> {
        match prepared {
            PreparedServiceCall::Immediate(result) => Ok(result),
            PreparedServiceCall::Legacy { handler, call } => {
                self.invoke_service_handler_async(handler, call, scope)
                    .await
            }
            PreparedServiceCall::Entity { handlers, context } => {
                let mut changed_states = Vec::new();
                let mut responses = Vec::new();
                for (handler, call) in handlers {
                    let result = self
                        .invoke_service_handler_async(handler, call, scope.clone())
                        .await?;
                    changed_states.extend(result.changed_states);
                    if let Some(response) = result.response {
                        responses.push(response);
                    }
                }
                Ok(aggregate_service_results(
                    changed_states,
                    responses,
                    context,
                ))
            }
        }
    }

    async fn invoke_service_handler_async(
        &self,
        handler: ServiceHandlerKind,
        call: ServiceCall,
        scope: Option<ServiceExecutionScope>,
    ) -> Result<ServiceResult> {
        match handler {
            ServiceHandlerKind::Sync(handler) => {
                let runtime = self.clone();
                let blocking_scope = scope;
                let handle = tokio::runtime::Handle::try_current().map_err(|_| {
                    Error::Service(
                        "synchronous service dispatch requires an active Tokio runtime".to_owned(),
                    )
                })?;
                let mut completion =
                    self.inner.service_tasks.spawn_blocking(&handle, move || {
                        let _scope = blocking_scope.map(|scope| scope());
                        handler(&runtime, &call)
                    })?;
                service_tasks::wait_for_result(&mut completion).await
            }
            ServiceHandlerKind::Async(handler) => {
                let runtime = self.clone();
                let future = if let Some(scope) = &scope {
                    let _scope = scope();
                    handler(runtime, call)
                } else {
                    handler(runtime, call)
                };
                if let Some(scope) = scope {
                    let mut future = future;
                    std::future::poll_fn(move |cx| {
                        let _scope = scope();
                        future.as_mut().poll(cx)
                    })
                    .await
                } else {
                    future.await
                }
            }
        }
    }
}

fn authorize_legacy_targets(
    runtime: &Runtime,
    call: &ServiceCall,
    authorize_targets: Option<&ServiceTargetAuthorizer>,
) -> Result<()> {
    let Some(authorize_targets) = authorize_targets else {
        return Ok(());
    };
    if !call.target.contains_key("entity_id") && !call.data.contains_key("entity_id") {
        return Ok(());
    }
    let Some(mut entity_ids) = parse_service_target(call)? else {
        return Ok(());
    };
    if entity_ids.len() == 1 && entity_ids[0] == "all" {
        entity_ids = runtime
            .states()
            .into_iter()
            .filter(|state| entity_domain(&state.entity_id) == call.domain)
            .map(|state| state.entity_id)
            .collect();
    }
    authorize_targets(&entity_ids)
}

fn async_service_required(call: &ServiceCall) -> Error {
    Error::Service(format!(
        "service {}.{} requires asynchronous dispatch; use Runtime::call_service_async",
        call.domain, call.service
    ))
}

fn aggregate_service_results(
    changed_states: Vec<crate::State>,
    mut responses: Vec<Value>,
    context: Context,
) -> ServiceResult {
    let response = match responses.len() {
        0 => None,
        1 => responses.pop(),
        _ => Some(Value::Array(responses)),
    };
    ServiceResult {
        changed_states,
        response,
        context,
    }
}
