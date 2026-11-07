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
    sync::{OwnedSemaphorePermit, broadcast, mpsc},
    time::timeout,
};
use tracing::{Instrument, debug_span};

use crate::AppState;
use crate::ssdp::SsdpStream;
use crate::system_health::HealthStream;
use crate::zeroconf::ZeroconfStream;

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
    let mut raw_log_streams = RawLogStreams::new();
    let mut health_streams = HashMap::<u64, HealthStream>::new();
    let mut ssdp_streams = HashMap::<u64, SsdpStream>::new();
    let mut zeroconf_streams = HashMap::<u64, ZeroconfStream>::new();
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
    native.set_images(state.features.images.clone());
    native.set_tags(state.features.tags.clone());
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
                let mut health_frames = Vec::new();
                let mut finished = Vec::new();
                for (id, stream) in &mut health_streams {
                    for _ in 0..stream.capacity() {
                        match stream.try_recv() {
                            Ok(frame) => {
                                let terminal = frame["event"]["type"] == "finish"
                                    || frame["success"] == false;
                                health_frames.push(frame);
                                if terminal {
                                    finished.push(*id);
                                    break;
                                }
                            }
                            Err(mpsc::error::TryRecvError::Empty) => break,
                            Err(mpsc::error::TryRecvError::Disconnected) => {
                                finished.push(*id);
                                break;
                            }
                        }
                    }
                    if stream.is_finished() && !finished.contains(id) {
                        finished.push(*id);
                    }
                }
                // Remove completed subscriptions before exposing their finish
                // frame. A following unsubscribe must see an inactive ID.
                for id in finished {
                    if let Some(stream) = health_streams.remove(&id) {
                        if stream.is_finished() {
                            stream.join().await;
                        } else {
                            stream.cancel().await;
                        }
                    }
                }
                if !health_frames.is_empty() && identity.user_id.is_some() {
                    match crate::extensions::authenticate_token(&state,&token).await {
                        Ok(current) if crate::extensions::same_authority(&identity,&current) && (!current.local_only || origin.is_local) => {},
                        _ => {close_socket(&mut socket).await;break;}
                    }
                }
                for frame in health_frames {
                    if send_json(&mut socket,&frame).await.is_err() {break 'connection;}
                }

                let mut ssdp_frames = Vec::new();
                let mut ssdp_errors = Vec::new();
                let mut finished_ssdp = Vec::new();
                for (id, stream) in &mut ssdp_streams {
                    for _ in 0..stream.capacity() {
                        match stream.try_recv() {
                            Ok(frame) => ssdp_frames.push(frame),
                            Err(mpsc::error::TryRecvError::Empty) => break,
                            Err(mpsc::error::TryRecvError::Disconnected) => {
                                finished_ssdp.push(*id);
                                break;
                            }
                        }
                    }
                    if let Some(error) = stream.take_error() {
                        ssdp_errors.push((*id, error));
                        if !finished_ssdp.contains(id) {
                            finished_ssdp.push(*id);
                        }
                    } else if stream.is_finished() && !finished_ssdp.contains(id) {
                        finished_ssdp.push(*id);
                    }
                }
                for id in finished_ssdp {
                    if let Some(stream) = ssdp_streams.remove(&id) {
                        if stream.is_finished() {
                            stream.join().await;
                        } else {
                            stream.cancel().await;
                        }
                    }
                }
                if (!ssdp_frames.is_empty() || !ssdp_errors.is_empty())
                    && identity.user_id.is_some()
                {
                    match crate::extensions::authenticate_token(&state, &token).await {
                        Ok(current)
                            if crate::extensions::same_authority(&identity, &current)
                                && (!current.local_only || origin.is_local) =>
                        {
                            identity = current;
                        }
                        _ => {
                            close_socket(&mut socket).await;
                            break 'connection;
                        }
                    }
                }
                for frame in ssdp_frames {
                    if send_json(&mut socket, &frame).await.is_err() {
                        break 'connection;
                    }
                }
                for (id, error) in ssdp_errors {
                    if send_error(
                        &mut socket,
                        id,
                        WsError::new("ssdp_stream_error", error),
                    )
                    .await
                    .is_err()
                    {
                        break 'connection;
                    }
                }

                let mut zeroconf_frames = Vec::new();
                let mut zeroconf_errors = Vec::new();
                let mut finished_zeroconf = Vec::new();
                for (id, stream) in &mut zeroconf_streams {
                    for _ in 0..stream.capacity() {
                        match stream.try_recv() {
                            Ok(frame) => zeroconf_frames.push(frame),
                            Err(mpsc::error::TryRecvError::Empty) => break,
                            Err(mpsc::error::TryRecvError::Disconnected) => {
                                finished_zeroconf.push(*id);
                                break;
                            }
                        }
                    }
                    if let Some(error) = stream.take_error() {
                        zeroconf_errors.push((*id, error));
                        if !finished_zeroconf.contains(id) {
                            finished_zeroconf.push(*id);
                        }
                    } else if stream.is_finished() && !finished_zeroconf.contains(id) {
                        finished_zeroconf.push(*id);
                    }
                }
                for id in finished_zeroconf {
                    if let Some(stream) = zeroconf_streams.remove(&id) {
                        if stream.is_finished() {
                            stream.join().await;
                        } else {
                            stream.cancel().await;
                        }
                    }
                }
                if (!zeroconf_frames.is_empty() || !zeroconf_errors.is_empty())
                    && identity.user_id.is_some()
                {
                    match crate::extensions::authenticate_token(&state, &token).await {
                        Ok(current)
                            if crate::extensions::same_authority(&identity, &current)
                                && current.is_admin
                                && (!current.local_only || origin.is_local) =>
                        {
                            identity = current;
                        }
                        _ => {
                            close_socket(&mut socket).await;
                            break 'connection;
                        }
                    }
                }
                for frame in zeroconf_frames {
                    if send_json(&mut socket, &frame).await.is_err() {
                        break 'connection;
                    }
                }
                for (id, error) in zeroconf_errors {
                    if send_error(
                        &mut socket,
                        id,
                        WsError::new("zeroconf_stream_error", error),
                    )
                    .await
                    .is_err()
                    {
                        break 'connection;
                    }
                }
            }
            _ = &mut shutdown_wait => {
                close_socket(&mut socket).await;
                break;
            }
            message = socket.recv().instrument(debug_span!("hass_raw_stream_delivery")) => {
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
                        if matches!(command_type, "ssdp/subscribe_discovery" | "zeroconf/subscribe_discovery") && !identity.is_admin {
                            if send_error(&mut socket,id,WsError::new("unauthorized","Unauthorized")).await.is_err() {break;}
                            continue;
                        }
                        if !crate::extensions::websocket_allowed(&identity,command_type,object) {
                            if send_error(&mut socket,id,WsError::new("unauthorized","Unauthorized")).await.is_err() {break;}
                            continue;
                        }
                        if crate::subscriptions::is_subscription_command(command_type)
                            && !matches!(command_type, "unsubscribe_events" | "config_entries/flow/progress")
                            && active_subscription_count(&native, &subscriptions, &raw_log_streams, &health_streams, &ssdp_streams, &zeroconf_streams)
                                >= MAX_SUBSCRIPTIONS_PER_SOCKET
                        {
                            if send_error(
                                &mut socket,
                                id,
                                WsError::new("subscription_limit", "Subscription limit reached"),
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                        if crate::subscriptions::is_subscription_command(command_type)
                            && command_type != "unsubscribe_events"
                        {
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
                            &mut raw_log_streams,
                            &mut health_streams,
                            &mut ssdp_streams,
                            &mut zeroconf_streams,
                            &native,
                            (id, command_type),
                            object,
                            (&identity, origin.clone()),
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
            raw_log_frame = raw_log_streams.recv(), if !raw_log_streams.is_empty() => {
                match raw_log_frame {
                    Some(RawLogFrame::Event { id, lines }) => {
                        if !raw_log_streams.contains(id) {
                            continue;
                        }
                        if identity.user_id.is_some() {
                            match crate::extensions::authenticate_token(&state, &token).await {
                                Ok(current)
                                    if current.is_admin
                                        && crate::extensions::same_authority(&identity, &current)
                                        && (!current.local_only || origin.is_local) =>
                                {
                                    identity = current;
                                }
                                _ => {
                                    close_socket(&mut socket).await;
                                    break 'connection;
                                }
                            }
                        }
                        if send_json(&mut socket, &json!({"id": id, "type": "event", "event": lines})).await.is_err() {
                            break 'connection;
                        }
                    }
                    Some(RawLogFrame::Error { id, error }) => {
                        if !raw_log_streams.contains(id) {
                            continue;
                        }
                        raw_log_streams.remove(id).await;
                        if send_error(&mut socket, id, map_core_error(error)).await.is_err() {
                            break 'connection;
                        }
                    }
                    Some(RawLogFrame::Closed { id }) => {
                        if !raw_log_streams.contains(id) {
                            continue;
                        }
                        raw_log_streams.remove(id).await;
                        if send_error(
                            &mut socket,
                            id,
                            WsError::new("event_stream_closed", "Raw log stream is closed"),
                        ).await.is_err() {
                            break 'connection;
                        }
                    }
                    None => {}
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
                                break 'connection;
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
                                break 'connection;
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
                                break 'connection;
                            }
                        }
                        subscriptions.clear();
                    }
                }
            }
        }
    }
    raw_log_streams.shutdown().await;
    for (_, stream) in health_streams {
        stream.cancel().await;
    }
    for (_, stream) in ssdp_streams {
        stream.cancel().await;
    }
    for (_, stream) in zeroconf_streams {
        stream.cancel().await;
    }
}

async fn receive_auth(socket: &mut WebSocket) -> Option<String> {
    match socket
        .recv()
        .instrument(debug_span!("hass_raw_stream_delivery"))
        .await?
    {
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

fn active_subscription_count(
    native: &Arc<Mutex<crate::subscriptions::NativeSubscriptions>>,
    event_subscriptions: &HashMap<u64, Option<String>>,
    raw_log_streams: &RawLogStreams,
    health_streams: &HashMap<u64, HealthStream>,
    ssdp_streams: &HashMap<u64, SsdpStream>,
    zeroconf_streams: &HashMap<u64, ZeroconfStream>,
) -> usize {
    let native_count = native
        .lock()
        .map(|subscriptions| subscriptions.len())
        .unwrap_or(MAX_SUBSCRIPTIONS_PER_SOCKET);
    native_count
        .saturating_add(event_subscriptions.len())
        .saturating_add(raw_log_streams.len())
        .saturating_add(health_streams.len())
        .saturating_add(ssdp_streams.len())
        .saturating_add(zeroconf_streams.len())
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    socket: &mut WebSocket,
    state: &AppState,
    event_receiver: &mut broadcast::Receiver<Event>,
    subscriptions: &mut HashMap<u64, Option<String>>,
    raw_log_streams: &mut RawLogStreams,
    health_streams: &mut HashMap<u64, HealthStream>,
    ssdp_streams: &mut HashMap<u64, SsdpStream>,
    zeroconf_streams: &mut HashMap<u64, ZeroconfStream>,
    native: &Arc<Mutex<crate::subscriptions::NativeSubscriptions>>,
    command: (u64, &str),
    message: &serde_json::Map<String, Value>,
    client: (&crate::extensions::Identity, crate::network::ClientOrigin),
) -> bool {
    let (identity, origin) = client;
    let (id, command_type) = command;
    if ((crate::extensions::command_requires_admin(command_type)
        || matches!(
            command_type,
            "ssdp/subscribe_discovery" | "zeroconf/subscribe_discovery"
        ))
        && !identity.is_admin)
        || (identity.read_only && matches!(command_type, "call_service" | "fire_event"))
    {
        return send_error(socket, id, WsError::new("unauthorized", "Unauthorized"))
            .await
            .is_ok();
    }
    if let Some(result) =
        crate::extensions::handle_ws(state, identity, origin, &Value::Object(message.clone())).await
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
            if active_subscription_count(
                native,
                subscriptions,
                raw_log_streams,
                health_streams,
                ssdp_streams,
                zeroconf_streams,
            ) >= MAX_SUBSCRIPTIONS_PER_SOCKET
            {
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
            let native_manager = native.clone();
            let native_command = Value::Object(message.clone());
            match run_blocking(move || {
                native_manager
                    .lock()
                    .map_err(|_| CoreError::Service("Subscription lock failed".into()))?
                    .handle_command(&native_command)
            })
            .await
            {
                Ok(Some(response)) => {
                    if send_success(socket, id, response.result).await.is_err() {
                        return false;
                    }
                    for frame in response.events {
                        if send_json(socket, &frame).await.is_err() {
                            return false;
                        }
                    }
                    return true;
                }
                Ok(None) => {}
                Err(error) if error.code == "not_found" => {}
                Err(error) => return send_error(socket, id, error).await.is_ok(),
            }
            let removed_event = subscriptions.remove(&subscription_id).is_some();
            let removed_raw_log = raw_log_streams.remove(subscription_id).await;
            let removed_health = match health_streams.remove(&subscription_id) {
                Some(stream) => {
                    stream.cancel().await;
                    true
                }
                None => false,
            };
            let mut ssdp_error = None;
            let removed_ssdp = match ssdp_streams.remove(&subscription_id) {
                Some(stream) => {
                    ssdp_error = stream.take_error();
                    stream.cancel().await;
                    true
                }
                None => false,
            };
            let mut zeroconf_error = None;
            let removed_zeroconf = match zeroconf_streams.remove(&subscription_id) {
                Some(stream) => {
                    zeroconf_error = stream.take_error();
                    stream.cancel().await;
                    true
                }
                None => false,
            };
            if !removed_event
                && !removed_raw_log
                && !removed_health
                && !removed_ssdp
                && !removed_zeroconf
            {
                return send_error(
                    socket,
                    id,
                    WsError::new("not_found", "Subscription not found"),
                )
                .await
                .is_ok();
            }
            if let Some(error) = ssdp_error
                && send_error(
                    socket,
                    subscription_id,
                    WsError::new("ssdp_stream_error", error),
                )
                .await
                .is_err()
            {
                return false;
            }
            if let Some(error) = zeroconf_error
                && send_error(
                    socket,
                    subscription_id,
                    WsError::new("zeroconf_stream_error", error),
                )
                .await
                .is_err()
            {
                return false;
            }
            send_success(socket, id, Value::Null).await.is_ok()
        }
        "ssdp/subscribe_discovery" => {
            if message.len() != 2 {
                return send_error(
                    socket,
                    id,
                    WsError::new("invalid_format", "Unexpected fields"),
                )
                .await
                .is_ok();
            }
            let Some(manager) = state.features.ssdp.clone() else {
                return send_error(
                    socket,
                    id,
                    WsError::new("not_found", "SSDP integration is not loaded"),
                )
                .await
                .is_ok();
            };
            if active_subscription_count(
                native,
                subscriptions,
                raw_log_streams,
                health_streams,
                ssdp_streams,
                zeroconf_streams,
            ) >= MAX_SUBSCRIPTIONS_PER_SOCKET
            {
                return send_error(
                    socket,
                    id,
                    WsError::new("subscription_limit", "Subscription limit reached"),
                )
                .await
                .is_ok();
            }
            if send_success(socket, id, Value::Null).await.is_err() {
                return false;
            }
            ssdp_streams.insert(id, SsdpStream::start(manager, id));
            true
        }
        "zeroconf/subscribe_discovery" => {
            if message.len() != 2 {
                return send_error(
                    socket,
                    id,
                    WsError::new("invalid_format", "Unexpected fields"),
                )
                .await
                .is_ok();
            }
            let Some(manager) = state.features.zeroconf.clone() else {
                return send_error(
                    socket,
                    id,
                    WsError::new("not_found", "Zeroconf integration is not loaded"),
                )
                .await
                .is_ok();
            };
            if active_subscription_count(
                native,
                subscriptions,
                raw_log_streams,
                health_streams,
                ssdp_streams,
                zeroconf_streams,
            ) >= MAX_SUBSCRIPTIONS_PER_SOCKET
            {
                return send_error(
                    socket,
                    id,
                    WsError::new("subscription_limit", "Subscription limit reached"),
                )
                .await
                .is_ok();
            }
            let mut stream = match ZeroconfStream::prepare(manager, id) {
                Ok(stream) => stream,
                Err(error) => {
                    return send_error(socket, id, map_core_error(error)).await.is_ok();
                }
            };
            if send_success(socket, id, Value::Null).await.is_err() {
                return false;
            }
            stream.start_snapshot();
            zeroconf_streams.insert(id, stream);
            true
        }
        "system_health/info" => {
            if message.len() != 2 {
                return send_error(
                    socket,
                    id,
                    WsError::new("invalid_format", "Unexpected fields"),
                )
                .await
                .is_ok();
            }
            let Some(manager) = state.features.system_health.clone() else {
                return send_error(
                    socket,
                    id,
                    WsError::new("not_found", "System Health integration is not loaded"),
                )
                .await
                .is_ok();
            };
            if active_subscription_count(
                native,
                subscriptions,
                raw_log_streams,
                health_streams,
                ssdp_streams,
                zeroconf_streams,
            ) >= MAX_SUBSCRIPTIONS_PER_SOCKET
            {
                return send_error(
                    socket,
                    id,
                    WsError::new("subscription_limit", "Subscription limit reached"),
                )
                .await
                .is_ok();
            }
            health_streams.insert(id, HealthStream::start(manager, id));
            true
        }
        "system_log/list" => {
            let Some(manager) = state.features.system_log.clone() else {
                return send_error(
                    socket,
                    id,
                    WsError::new("not_found", "System log integration is not loaded"),
                )
                .await
                .is_ok();
            };
            let result = run_blocking(move || manager.list()).await;
            send_core_result(socket, id, result).await.is_ok()
        }
        "repairs/list_issues" => {
            let Some(manager) = state.features.repairs.clone() else {
                return send_error(
                    socket,
                    id,
                    WsError::new("not_found", "Repairs integration is not loaded"),
                )
                .await
                .is_ok();
            };
            let result = run_repairs(move || manager.list_issues())
                .await
                .map(|issues| json!({"issues": issues}));
            send_core_result(socket, id, result).await.is_ok()
        }
        "repairs/get_issue_data" => {
            let (domain, issue_id) = match (
                required_field_string(message, "domain"),
                required_field_string(message, "issue_id"),
            ) {
                (Ok(domain), Ok(issue_id)) => (domain, issue_id),
                (Err(error), _) | (_, Err(error)) => {
                    return send_error(socket, id, error).await.is_ok();
                }
            };
            let Some(manager) = state.features.repairs.clone() else {
                return send_error(
                    socket,
                    id,
                    WsError::new("not_found", "Repairs integration is not loaded"),
                )
                .await
                .is_ok();
            };
            let result = run_repairs(move || {
                manager
                    .get_issue_data(&domain, &issue_id)
                    .map(|issue_data| json!({"issue_data": issue_data}))
            })
            .await;
            send_core_result(socket, id, result).await.is_ok()
        }
        "repairs/ignore_issue" => {
            let (domain, issue_id, ignore) = match (
                required_field_string(message, "domain"),
                required_field_string(message, "issue_id"),
                required_field_bool(message, "ignore"),
            ) {
                (Ok(domain), Ok(issue_id), Ok(ignore)) => (domain, issue_id, ignore),
                (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                    return send_error(socket, id, error).await.is_ok();
                }
            };
            let Some(manager) = state.features.repairs.clone() else {
                return send_error(
                    socket,
                    id,
                    WsError::new("not_found", "Repairs integration is not loaded"),
                )
                .await
                .is_ok();
            };
            let result = run_repairs(move || {
                manager
                    .ignore_issue(&domain, &issue_id, ignore)
                    .map(|()| Value::Null)
            })
            .await;
            send_core_result(socket, id, result).await.is_ok()
        }
        "system_log/subscribe_raw" => {
            if active_subscription_count(
                native,
                subscriptions,
                raw_log_streams,
                health_streams,
                ssdp_streams,
                zeroconf_streams,
            ) >= MAX_SUBSCRIPTIONS_PER_SOCKET
            {
                return send_error(
                    socket,
                    id,
                    WsError::new("subscription_limit", "Subscription limit reached"),
                )
                .await
                .is_ok();
            }
            let integration = match message.get("integration") {
                None => None,
                Some(Value::String(integration)) => Some(integration.clone()),
                Some(_) => {
                    return send_error(
                        socket,
                        id,
                        WsError::new("invalid_format", "integration must be a string"),
                    )
                    .await
                    .is_ok();
                }
            };
            let Some(manager) = state.features.system_log.clone() else {
                return send_error(
                    socket,
                    id,
                    WsError::new("not_found", "System log integration is not loaded"),
                )
                .await
                .is_ok();
            };
            let stream = match manager.subscribe_raw(integration.as_deref()) {
                Ok(stream) => stream,
                Err(CoreError::NotFound(_)) => {
                    return send_error(
                        socket,
                        id,
                        WsError::new("not_found", "Integration not found"),
                    )
                    .await
                    .is_ok();
                }
                Err(error) => {
                    return send_error(socket, id, map_core_error(error)).await.is_ok();
                }
            };
            if send_success(socket, id, Value::Null).await.is_err() {
                return false;
            }
            raw_log_streams.insert(id, stream);
            true
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

enum RawLogFrame {
    Event { id: u64, lines: Vec<String> },
    Error { id: u64, error: CoreError },
    Closed { id: u64 },
}

struct RawLogStreams {
    tasks: HashMap<u64, tokio::task::JoinHandle<()>>,
    sender: mpsc::Sender<RawLogFrame>,
    receiver: mpsc::Receiver<RawLogFrame>,
}

impl RawLogStreams {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel(MAX_SUBSCRIPTIONS_PER_SOCKET);
        Self {
            tasks: HashMap::new(),
            sender,
            receiver,
        }
    }

    fn len(&self) -> usize {
        self.tasks.len()
    }

    fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn contains(&self, id: u64) -> bool {
        self.tasks.contains_key(&id)
    }

    fn insert(&mut self, id: u64, mut subscription: hass_system_log::RawLogSubscription) {
        let sender = self.sender.clone();
        let task = tokio::spawn(async move {
            match subscription.initial().await {
                Ok(lines) => {
                    if sender.send(RawLogFrame::Event { id, lines }).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(RawLogFrame::Error { id, error }).await;
                    return;
                }
            }

            loop {
                match subscription.next_batch().await {
                    Ok(Some(lines)) => {
                        if sender.send(RawLogFrame::Event { id, lines }).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => {
                        let _ = sender.send(RawLogFrame::Closed { id }).await;
                        return;
                    }
                    Err(error) => {
                        let _ = sender.send(RawLogFrame::Error { id, error }).await;
                        return;
                    }
                }
            }
        });
        if let Some(previous) = self.tasks.insert(id, task) {
            previous.abort();
        }
    }

    async fn recv(&mut self) -> Option<RawLogFrame> {
        self.receiver.recv().await
    }

    async fn remove(&mut self, id: u64) -> bool {
        let Some(task) = self.tasks.remove(&id) else {
            return false;
        };
        task.abort();
        let _ = task.await;
        true
    }

    async fn shutdown(&mut self) {
        let tasks = std::mem::take(&mut self.tasks);
        for (_, task) in tasks {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for RawLogStreams {
    fn drop(&mut self) {
        for task in self.tasks.values() {
            task.abort();
        }
    }
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
        CoreError::InvalidData { schema_errors } => WsError::new(
            "invalid_format",
            format!("Invalid data: {}", json!(schema_errors)),
        ),
        CoreError::NotFound(message) => WsError::new("not_found", message),
        CoreError::HomeAssistant(message) => WsError::new("home_assistant_error", message),
        CoreError::NotAllowed(message) => WsError::new("not_allowed", message),
        CoreError::Storage(_) => WsError::new("unknown_error", "Internal server error"),
        CoreError::Service(_) => WsError::new("service_error", "Service call failed"),
    }
}

fn map_repairs_error(error: hass_repairs::RepairsError) -> WsError {
    use hass_repairs::RepairsError;

    match error {
        RepairsError::Core(error) => map_core_error(error),
        RepairsError::UnknownIssue { issue_id, .. } => {
            WsError::new("unknown_issue", format!("Issue '{issue_id}' not found"))
        }
        RepairsError::NotFixable { issue_id, .. } => WsError::new(
            "unknown_issue",
            format!("Issue id {issue_id} is not fixable"),
        ),
        RepairsError::UnknownFlow(flow_id) => {
            WsError::new("not_found", format!("Unknown flow: {flow_id}"))
        }
        RepairsError::UnknownStep(message) => WsError::new("invalid_format", message),
        RepairsError::UnknownEntry(entry_id) => {
            WsError::new("not_found", format!("Config entry {entry_id} not found"))
        }
        RepairsError::Unsupported(message) => WsError::new("not_supported", message),
    }
}

fn required_field_string(message: &Map<String, Value>, field: &str) -> Result<String, WsError> {
    message
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| WsError::new("invalid_format", format!("Missing or invalid '{field}'")))
}

fn required_field_bool(message: &Map<String, Value>, field: &str) -> Result<bool, WsError> {
    message
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| WsError::new("invalid_format", format!("Missing or invalid '{field}'")))
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
    match timeout(
        SEND_TIMEOUT,
        socket
            .send(message)
            .instrument(debug_span!("hass_raw_stream_delivery")),
    )
    .await
    {
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

async fn run_repairs<T, F>(operation: F) -> Result<T, WsError>
where
    T: Send + 'static,
    F: FnOnce() -> hass_repairs::RepairsResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| WsError::new("unknown_error", "Internal server error"))?
        .map_err(map_repairs_error)
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
