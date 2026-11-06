use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Extension,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use hass_core::{Error as CoreError, Event, ServiceCall, ServiceResult, State as HassState};
use serde_json::{Map, Value, json};
use tokio::{
    sync::{OwnedSemaphorePermit, broadcast},
    time::timeout,
};

use crate::AppState;

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_WEBSOCKET_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_WEBSOCKET_FRAME_BYTES: usize = 256 * 1024;
const MAX_SUBSCRIPTIONS_PER_SOCKET: usize = 32;
const MAX_PRE_SUBSCRIPTION_DRAIN: usize = 4096;

pub(crate) async fn upgrade(
    State(state): State<Arc<AppState>>,
    Extension(origin): Extension<crate::network::ClientOrigin>,
    websocket: WebSocketUpgrade,
) -> Response {
    if state.api.access_token.is_empty() && state.features.auth.is_none() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "message": "API access token is not configured" })),
        )
            .into_response();
    }

    let permit = match Arc::clone(&state.websocket_slots).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({ "message": "WebSocket connection limit reached" })),
            )
                .into_response();
        }
    };

    websocket
        .max_message_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .max_frame_size(MAX_WEBSOCKET_FRAME_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, permit, origin))
        .into_response()
}

async fn handle_socket(
    mut socket: WebSocket,
    state: Arc<AppState>,
    _permit: OwnedSemaphorePermit,
    origin: crate::network::ClientOrigin,
) {
    let shutdown_wait: Pin<Box<dyn Future<Output = ()> + Send>> = match state.shutdown.clone() {
        Some(mut receiver) => Box::pin(async move {
            loop {
                if *receiver.borrow() {
                    return;
                }
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        }),
        None => Box::pin(std::future::pending()),
    };
    tokio::pin!(shutdown_wait);

    if send_json(
        &mut socket,
        &json!({ "type": "auth_required", "ha_version": hass_core::VERSION }),
    )
    .await
    .is_err()
    {
        return;
    }

    let auth_result = tokio::select! {
        _ = &mut shutdown_wait => {
            close_socket(&mut socket).await;
            return;
        }
        result = timeout(AUTH_TIMEOUT, receive_auth(&mut socket)) => result,
    };
    let token = match auth_result {
        Ok(Some(token)) => token,
        _ => {
            close_socket(&mut socket).await;
            return;
        }
    };

    let mut identity = crate::extensions::authenticate_token(&state, &token)
        .await
        .ok();
    if identity
        .as_ref()
        .is_none_or(|identity| identity.local_only && !origin.is_local)
    {
        let _ = send_json(
            &mut socket,
            &json!({
                "type": "auth_invalid",
                "message": "Invalid access token"
            }),
        )
        .await;
        close_socket(&mut socket).await;
        return;
    }

    let mut identity = identity.take().expect("authentication was checked");
    // The receiver exists before auth_ok, so any event emitted before the first
    // subscribe command can be discarded deliberately before acknowledging that
    // subscription. Events emitted after that point are retained for delivery.
    let mut event_receiver = state.api.runtime.subscribe();
    if send_json(
        &mut socket,
        &json!({ "type": "auth_ok", "ha_version": hass_core::VERSION }),
    )
    .await
    .is_err()
    {
        return;
    }

    let mut subscriptions = HashMap::<u64, Option<String>>::new();
    let mut last_command_id: Option<u64> = None;
    let mut native = crate::subscriptions::NativeSubscriptions::new(
        state.api.runtime.clone(),
        state.features.config_entries.clone(),
        state.features.templates.clone(),
    );
    native.set_state_read_access(identity.can_read_states);
    native.set_notifications(state.features.notifications.clone());
    native.set_zones(state.features.zones.clone());
    native.set_persons(state.features.persons.clone());
    let native = Arc::new(Mutex::new(native));
    let mut stream_tick = tokio::time::interval(Duration::from_millis(100));
    stream_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    'connection: loop {
        tokio::select! {
            _ = stream_tick.tick() => {
                let native = native.clone();
                let frames = run_blocking(move || {
                    let mut native=native.lock().map_err(|_|CoreError::Service("Subscription lock failed".into()))?;
                    if native.is_empty() { Ok(Vec::new()) } else { native.drain_events() }
                }).await;
                let frames=match frames {Ok(frames)=>frames,Err(_)=>{close_socket(&mut socket).await;break;}};
                if !frames.is_empty() && identity.user_id.is_some() {
                    match crate::extensions::authenticate_token(&state,&token).await {
                        Ok(current) if crate::extensions::same_authority(&identity,&current) && (!current.local_only || origin.is_local) => {},
                        _ => {close_socket(&mut socket).await;break;}
                    }
                }
                for frame in frames {
                    if send_json(&mut socket,&frame).await.is_err() {break 'connection;}
                }
            }
            _ = &mut shutdown_wait => {
                close_socket(&mut socket).await;
                break;
            }
            message = socket.recv() => {
                let Some(Ok(message)) = message else {
                    break;
                };
                match message {
                    Message::Text(text) => {
                        let value = match serde_json::from_str::<Value>(&text) {
                            Ok(value) => value,
                            Err(_) => {
                                close_socket(&mut socket).await;
                                break;
                            }
                        };
                        let Some(object) = value.as_object() else {
                            close_socket(&mut socket).await;
                            break;
                        };
                        let Some(id) = object.get("id").and_then(Value::as_u64).filter(|id| *id > 0) else {
                            close_socket(&mut socket).await;
                            break;
                        };
                        if last_command_id.is_some_and(|last| id <= last) {
                            let error = WsError::new(
                                "invalid_format",
                                "Command ids must be strictly increasing positive integers",
                            );
                            if send_error(&mut socket, id, error).await.is_err() {
                                break;
                            }
                            continue;
                        }
                        last_command_id = Some(id);
                        let Some(command_type) = object.get("type").and_then(Value::as_str) else {
                            if send_error(
                                &mut socket,
                                id,
                                WsError::new("invalid_format", "Command type must be a string"),
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                            continue;
                        };
                        if identity.user_id.is_some() {
                            match crate::extensions::authenticate_token(&state, &token).await {
                                Ok(current) if (!current.local_only || origin.is_local) && crate::extensions::same_authority(&identity,&current) => identity = current,
                                Ok(_) => {
                                    let _ = send_json(&mut socket, &json!({"type":"auth_invalid","message":"Invalid access token"})).await;
                                    close_socket(&mut socket).await;
                                    break;
                                }
                                Err(_) => {
                                    let _ = send_json(&mut socket, &json!({"type":"auth_invalid","message":"Access token revoked or expired"})).await;
                                    close_socket(&mut socket).await;
                                    break;
                                }
                            }
                        }
                        if !crate::extensions::websocket_allowed(&identity,command_type,object) {
                            if send_error(&mut socket,id,WsError::new("unauthorized","Unauthorized")).await.is_err() {break;}
                            continue;
                        }
                        if crate::subscriptions::is_subscription_command(command_type) {
                            let manager=native.clone();let command=value.clone();
                            match run_blocking(move || manager.lock().map_err(|_|CoreError::Service("Subscription lock failed".into()))?.handle_command(&command)).await {
                                Ok(Some(response)) => {
                                    if send_success(&mut socket,id,response.result).await.is_err() {break;}
                                    for frame in response.events {
                                        if send_json(&mut socket,&frame).await.is_err() {break 'connection;}
                                    }
                                    continue;
                                },
                                Ok(None)=>{},
                                Err(error)=>{
                                    if send_error(&mut socket,id,error).await.is_err() {break;}
                                    continue;
                                }
                            }
                        }
                        if !handle_command(
                            &mut socket,
                            &state,
                            &mut event_receiver,
                            &mut subscriptions,
                            (id, command_type),
                            object,
                            &identity,
                        )
                        .await
                        {
                            break;
                        }
                    }
                    Message::Ping(payload) => {
                        if send_message(&mut socket, Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => break,
                    Message::Binary(_) => {
                        close_socket(&mut socket).await;
                        break;
                    }
                }
            }
            event = event_receiver.recv(), if !subscriptions.is_empty() => {
                match event {
                    Ok(event) => {
                        let recipients = subscriptions
                            .iter()
                            .filter_map(|(id, event_type)| {
                                event_type
                                    .as_ref()
                                    .is_none_or(|event_type| event_type == &event.event_type)
                                    .then_some(*id)
                            })
                            .collect::<Vec<_>>();
                        for id in recipients {
                            if send_event(&mut socket, id, &event).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let subscription_ids = subscriptions.keys().copied().collect::<Vec<_>>();
                        for id in subscription_ids {
                            let error = WsError::new(
                                "event_stream_lagged",
                                "Event subscription lagged; subscribe again to resume",
                            );
                            if send_error(&mut socket, id, error).await.is_err() {
                                return;
                            }
                        }
                        subscriptions.clear();
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let subscription_ids = subscriptions.keys().copied().collect::<Vec<_>>();
                        for id in subscription_ids {
                            let error = WsError::new(
                                "event_stream_closed",
                                "Event stream is closed",
                            );
                            if send_error(&mut socket, id, error).await.is_err() {
                                return;
                            }
                        }
                        subscriptions.clear();
                    }
                }
            }
        }
    }
}

async fn receive_auth(socket: &mut WebSocket) -> Option<String> {
    match socket.recv().await? {
        Ok(Message::Text(text)) => {
            let value = serde_json::from_str::<Value>(&text).ok()?;
            let object = value.as_object()?;
            if object.get("type")?.as_str()? != "auth" {
                return None;
            }
            object.get("access_token")?.as_str().map(str::to_owned)
        }
        Ok(Message::Ping(payload)) => {
            let _ = send_message(socket, Message::Pong(payload)).await;
            None
        }
        _ => None,
    }
}

async fn handle_command(
    socket: &mut WebSocket,
    state: &AppState,
    event_receiver: &mut broadcast::Receiver<Event>,
    subscriptions: &mut HashMap<u64, Option<String>>,
    command: (u64, &str),
    message: &serde_json::Map<String, Value>,
    identity: &crate::extensions::Identity,
) -> bool {
    let (id, command_type) = command;
    if (crate::extensions::command_requires_admin(command_type) && !identity.is_admin)
        || (identity.read_only && matches!(command_type, "call_service" | "fire_event"))
    {
        return send_error(socket, id, WsError::new("unauthorized", "Unauthorized"))
            .await
            .is_ok();
    }
    if let Some(result) =
        crate::extensions::handle_ws(state, identity, &Value::Object(message.clone())).await
    {
        return send_core_result(socket, id, result.map_err(map_core_error))
            .await
            .is_ok();
    }
    match command_type {
        "ping" => send_json(socket, &json!({ "id": id, "type": "pong" }))
            .await
            .is_ok(),
        "get_states" => {
            if !identity.can_read_states {
                return send_success(socket, id, json!([])).await.is_ok();
            }
            let runtime = state.api.runtime.clone();
            let result = run_blocking(move || Ok(runtime.states())).await;
            send_core_result(
                socket,
                id,
                result.map(|states: Vec<HassState>| json!(states)),
            )
            .await
            .is_ok()
        }
        "get_config" => send_success(socket, id, state.api.config.clone())
            .await
            .is_ok(),
        "get_services" => {
            let runtime = state.api.runtime.clone();
            let result = run_blocking(move || Ok(runtime.services())).await;
            send_core_result(socket, id, result).await.is_ok()
        }
        "subscribe_events" => {
            if subscriptions.len() >= MAX_SUBSCRIPTIONS_PER_SOCKET {
                return send_error(
                    socket,
                    id,
                    WsError::new("subscription_limit", "Subscription limit reached"),
                )
                .await
                .is_ok();
            }
            let event_type = match message.get("event_type") {
                None => None,
                Some(Value::String(event_type))
                    if !event_type.is_empty()
                        && event_type.len() <= 255
                        && !event_type.chars().any(char::is_control) =>
                {
                    (event_type != "*").then(|| event_type.clone())
                }
                _ => {
                    return send_error(
                        socket,
                        id,
                        WsError::new("invalid_format", "Invalid event_type"),
                    )
                    .await
                    .is_ok();
                }
            };

            if subscriptions.is_empty() {
                for _ in 0..MAX_PRE_SUBSCRIPTION_DRAIN {
                    match event_receiver.try_recv() {
                        Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                        Err(broadcast::error::TryRecvError::Empty)
                        | Err(broadcast::error::TryRecvError::Closed) => break,
                    }
                }
            }
            // The event receiver was created when the authenticated session began.
            // Register before acknowledging so no new event can fall between the
            // client receiving its subscription id and the subscription becoming live.
            subscriptions.insert(id, event_type);
            send_success(socket, id, Value::Null).await.is_ok()
        }
        "unsubscribe_events" => {
            let Some(subscription_id) = message
                .get("subscription")
                .and_then(Value::as_u64)
                .filter(|id| *id > 0)
            else {
                return send_error(
                    socket,
                    id,
                    WsError::new("invalid_format", "subscription must be a positive integer"),
                )
                .await
                .is_ok();
            };
            if subscriptions.remove(&subscription_id).is_none() {
                return send_error(
                    socket,
                    id,
                    WsError::new("not_found", "Subscription not found"),
                )
                .await
                .is_ok();
            }
            send_success(socket, id, Value::Null).await.is_ok()
        }
        "fire_event" => {
            let Some(event_type) = message.get("event_type").and_then(Value::as_str) else {
                return send_error(
                    socket,
                    id,
                    WsError::new("invalid_format", "event_type must be a string"),
                )
                .await
                .is_ok();
            };
            if event_type.is_empty()
                || event_type.len() > 255
                || event_type.chars().any(char::is_control)
            {
                return send_error(
                    socket,
                    id,
                    WsError::new("invalid_format", "Invalid event_type"),
                )
                .await
                .is_ok();
            }
            let data = match message.get("event_data") {
                None => Map::new(),
                Some(Value::Object(data)) => data.clone(),
                Some(_) => {
                    return send_error(
                        socket,
                        id,
                        WsError::new("invalid_format", "event_data must be an object"),
                    )
                    .await
                    .is_ok();
                }
            };
            let runtime = state.api.runtime.clone();
            let event_type = event_type.to_owned();
            let identity_context = identity.context();
            let result =
                run_blocking(move || runtime.fire_event(&event_type, data, identity_context))
                    .await
                    .map(|event| json!({ "context": event.context }));
            send_core_result(socket, id, result).await.is_ok()
        }
        "call_service" => {
            let Some(domain) = message.get("domain").and_then(Value::as_str) else {
                return send_error(
                    socket,
                    id,
                    WsError::new("invalid_format", "domain must be a string"),
                )
                .await
                .is_ok();
            };
            let Some(service) = message.get("service").and_then(Value::as_str) else {
                return send_error(
                    socket,
                    id,
                    WsError::new("invalid_format", "service must be a string"),
                )
                .await
                .is_ok();
            };
            let data = match message.get("service_data") {
                None => Map::new(),
                Some(Value::Object(data)) => data.clone(),
                Some(_) => {
                    return send_error(
                        socket,
                        id,
                        WsError::new("invalid_format", "service_data must be an object"),
                    )
                    .await
                    .is_ok();
                }
            };
            let target = match message.get("target") {
                None => Map::new(),
                Some(Value::Object(target)) => target.clone(),
                Some(_) => {
                    return send_error(
                        socket,
                        id,
                        WsError::new("invalid_format", "target must be an object"),
                    )
                    .await
                    .is_ok();
                }
            };
            let call = ServiceCall {
                domain: domain.to_owned(),
                service: service.to_owned(),
                data,
                target,
                context: identity.context(),
            };
            let runtime = state.api.runtime.clone();
            let check_runtime = runtime.clone();
            let check_domain = domain.to_owned();
            let check_service = service.to_owned();
            let registered = run_blocking(move || {
                Ok(check_runtime
                    .services()
                    .get(&check_domain)
                    .and_then(Value::as_object)
                    .is_some_and(|services| services.contains_key(&check_service)))
            })
            .await;
            match registered {
                Ok(true) => {}
                Ok(false) => {
                    return send_error(
                        socket,
                        id,
                        WsError::new("service_not_found", "Service not found"),
                    )
                    .await
                    .is_ok();
                }
                Err(error) => return send_error(socket, id, error).await.is_ok(),
            }
            let result = run_blocking(move || runtime.call_service(call)).await.map(
                |result: ServiceResult| {
                    json!({
                        "changed_states": result.changed_states,
                        "response": result.response,
                        "context": result.context,
                    })
                },
            );
            send_core_result(socket, id, result).await.is_ok()
        }
        _ => send_error(
            socket,
            id,
            WsError::new("unknown_command", "Unknown command"),
        )
        .await
        .is_ok(),
    }
}

#[derive(Clone)]
struct WsError {
    code: &'static str,
    message: String,
}

impl WsError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn map_core_error(error: CoreError) -> WsError {
    match error {
        CoreError::InvalidInput(message) => WsError::new("invalid_format", message),
        CoreError::NotFound(message) => WsError::new("not_found", message),
        CoreError::Storage(_) => WsError::new("unknown_error", "Internal server error"),
        CoreError::Service(_) => WsError::new("service_error", "Service call failed"),
    }
}

async fn send_core_result<T: serde::Serialize>(
    socket: &mut WebSocket,
    id: u64,
    result: Result<T, WsError>,
) -> Result<(), ()> {
    match result {
        Ok(value) => {
            send_success(
                socket,
                id,
                serde_json::to_value(value).unwrap_or(Value::Null),
            )
            .await
        }
        Err(error) => send_error(socket, id, error).await,
    }
}

async fn send_success(socket: &mut WebSocket, id: u64, result: Value) -> Result<(), ()> {
    send_json(
        socket,
        &json!({
            "id": id,
            "type": "result",
            "success": true,
            "result": result,
        }),
    )
    .await
}

async fn send_error(socket: &mut WebSocket, id: u64, error: WsError) -> Result<(), ()> {
    send_json(
        socket,
        &json!({
            "id": id,
            "type": "result",
            "success": false,
            "error": {
                "code": error.code,
                "message": error.message,
            },
        }),
    )
    .await
}

async fn send_event(socket: &mut WebSocket, id: u64, event: &Event) -> Result<(), ()> {
    send_json(
        socket,
        &json!({
            "id": id,
            "type": "event",
            "event": event,
        }),
    )
    .await
}

async fn send_json(socket: &mut WebSocket, value: &Value) -> Result<(), ()> {
    let text = serde_json::to_string(value).map_err(|_| ())?;
    send_message(socket, Message::Text(text.into())).await
}

async fn send_message(socket: &mut WebSocket, message: Message) -> Result<(), ()> {
    match timeout(SEND_TIMEOUT, socket.send(message)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

async fn close_socket(socket: &mut WebSocket) {
    let _ = send_message(socket, Message::Close(None)).await;
}

async fn run_blocking<T, F>(operation: F) -> Result<T, WsError>
where
    T: Send + 'static,
    F: FnOnce() -> hass_core::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| WsError::new("unknown_error", "Internal server error"))?
        .map_err(map_core_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiState;
    use axum::Router;
    use futures_util::{SinkExt, StreamExt};
    use hass_core::{MemoryStore, ServiceHandler};
    use std::net::SocketAddr;
    use tokio::{net::TcpListener, task::JoinHandle};
    use tokio_tungstenite::{connect_async, tungstenite::Message as ClientMessage};

    fn test_state() -> ApiState {
        use hass_core::Runtime;

        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let handler: ServiceHandler = Arc::new(|runtime, call| {
            let entity_id = call
                .data
                .get("entity_id")
                .and_then(Value::as_str)
                .ok_or_else(|| CoreError::InvalidInput("entity_id is required".to_owned()))?;
            let new_state = call
                .data
                .get("state")
                .and_then(Value::as_str)
                .ok_or_else(|| CoreError::InvalidInput("state is required".to_owned()))?;
            let changed = runtime.set_state(
                entity_id,
                new_state,
                Map::new(),
                call.context.clone(),
                false,
            )?;
            Ok(ServiceResult {
                changed_states: vec![changed.state],
                response: None,
                context: call.context.clone(),
            })
        });
        runtime
            .register_service("test", "set_state", json!({}), handler)
            .unwrap();
        ApiState {
            runtime,
            access_token: "test-secret-token".to_owned(),
            config: json!({ "location_name": "test" }),
        }
    }

    async fn start_server(router: Router) -> (SocketAddr, JoinHandle<Result<(), std::io::Error>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await });
        (address, task)
    }

    async fn connect(
        address: SocketAddr,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let (socket, _) = connect_async(format!("ws://{address}/api/websocket"))
            .await
            .unwrap();
        socket
    }

    async fn receive_json<S>(socket: &mut S) -> Value
    where
        S: futures_util::Stream<
                Item = Result<ClientMessage, tokio_tungstenite::tungstenite::Error>,
            > + Unpin,
    {
        loop {
            match socket.next().await.unwrap().unwrap() {
                ClientMessage::Text(text) => return serde_json::from_str(&text).unwrap(),
                ClientMessage::Ping(payload) => {
                    let _ = socket;
                    let _ = payload;
                }
                other => panic!("unexpected websocket message: {other:?}"),
            }
        }
    }

    async fn send_command<S>(socket: &mut S, value: Value)
    where
        S: futures_util::Sink<ClientMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        socket
            .send(ClientMessage::Text(value.to_string().into()))
            .await
            .unwrap();
    }

    async fn authenticate<S>(socket: &mut S, token: &str)
    where
        S: futures_util::Sink<ClientMessage, Error = tokio_tungstenite::tungstenite::Error>
            + futures_util::Stream<
                Item = Result<ClientMessage, tokio_tungstenite::tungstenite::Error>,
            > + Unpin,
    {
        assert_eq!(receive_json(socket).await["type"], "auth_required");
        send_command(socket, json!({ "type": "auth", "access_token": token })).await;
        let response = receive_json(socket).await;
        assert_eq!(response["type"], "auth_ok");
    }

    #[tokio::test]
    async fn websocket_auth_and_subscription_service_round_trip() {
        let app = crate::router(test_state());
        let (address, server) = start_server(app).await;
        let mut socket = connect(address).await;
        authenticate(&mut socket, "test-secret-token").await;

        send_command(
            &mut socket,
            json!({ "id": 1, "type": "subscribe_events", "event_type": "state_changed" }),
        )
        .await;
        assert_eq!(receive_json(&mut socket).await["success"], true);

        send_command(
            &mut socket,
            json!({
                "id": 2,
                "type": "call_service",
                "domain": "test",
                "service": "set_state",
                "service_data": {
                    "entity_id": "light.test",
                    "state": "on"
                }
            }),
        )
        .await;

        let mut saw_result = false;
        let mut saw_state_event = false;
        for _ in 0..3 {
            let message = timeout(Duration::from_secs(2), receive_json(&mut socket))
                .await
                .unwrap();
            if message["type"] == "event" {
                assert_eq!(message["id"], 1);
                assert_eq!(message["event"]["event_type"], "state_changed");
                saw_state_event = true;
            } else if message["id"] == 2 {
                assert_eq!(message["success"], true);
                assert_eq!(message["result"]["changed_states"][0]["state"], "on");
                saw_result = true;
            }
            if saw_result && saw_state_event {
                break;
            }
        }
        assert!(saw_result && saw_state_event);

        send_command(
            &mut socket,
            json!({ "id": 3, "type": "unsubscribe_events", "subscription": 1 }),
        )
        .await;
        assert_eq!(receive_json(&mut socket).await["success"], true);

        send_command(
            &mut socket,
            json!({
                "id": 4,
                "type": "call_service",
                "domain": "test",
                "service": "set_state",
                "service_data": {
                    "entity_id": "light.test",
                    "state": "off"
                }
            }),
        )
        .await;
        let response = timeout(Duration::from_secs(2), receive_json(&mut socket))
            .await
            .unwrap();
        assert_eq!(response["id"], 4);
        assert_eq!(response["success"], true);

        send_command(&mut socket, json!({ "id": 5, "type": 7 })).await;
        let malformed = receive_json(&mut socket).await;
        assert_eq!(malformed["id"], 5);
        assert_eq!(malformed["success"], false);
        assert_eq!(malformed["error"]["code"], "invalid_format");

        send_command(&mut socket, json!({ "id": 5, "type": "ping" })).await;
        let duplicate = receive_json(&mut socket).await;
        assert_eq!(duplicate["id"], 5);
        assert_eq!(duplicate["success"], false);
        assert_eq!(duplicate["error"]["code"], "invalid_format");

        send_command(&mut socket, json!({ "id": 6, "type": "not_a_command" })).await;
        let unknown = receive_json(&mut socket).await;
        assert_eq!(unknown["id"], 6);
        assert_eq!(unknown["error"]["code"], "unknown_command");

        drop(socket);
        server.abort();
    }

    #[tokio::test]
    async fn invalid_websocket_token_is_rejected_and_closed() {
        let app = crate::router(test_state());
        let (address, server) = start_server(app).await;
        let mut socket = connect(address).await;
        assert_eq!(receive_json(&mut socket).await["type"], "auth_required");
        send_command(
            &mut socket,
            json!({ "type": "auth", "access_token": "wrong-token" }),
        )
        .await;
        let response = receive_json(&mut socket).await;
        assert_eq!(response["type"], "auth_invalid");
        let close = timeout(Duration::from_secs(1), socket.next())
            .await
            .unwrap();
        assert!(matches!(close, Some(Ok(ClientMessage::Close(_))) | None));
        server.abort();
    }

    #[tokio::test]
    async fn shutdown_signal_closes_idle_websocket() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let app = crate::router_with_shutdown(test_state(), shutdown_rx);
        let (address, server) = start_server(app).await;
        let mut socket = connect(address).await;
        authenticate(&mut socket, "test-secret-token").await;

        shutdown_tx.send(true).unwrap();
        let close = timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap();
        assert!(matches!(close, Some(Ok(ClientMessage::Close(_))) | None));
        server.abort();
    }

    #[tokio::test]
    async fn shutdown_signal_closes_socket_waiting_for_auth() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let app = crate::router_with_shutdown(test_state(), shutdown_rx);
        let (address, server) = start_server(app).await;
        let mut socket = connect(address).await;
        assert_eq!(receive_json(&mut socket).await["type"], "auth_required");

        shutdown_tx.send(true).unwrap();
        let close = timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap();
        assert!(matches!(close, Some(Ok(ClientMessage::Close(_))) | None));
        server.abort();
    }
}
