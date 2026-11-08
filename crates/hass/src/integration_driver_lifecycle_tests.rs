use super::*;
use hass_core::{MemoryStore, Runtime};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};

struct LoopbackBroker {
    address: String,
    first_connect: Option<oneshot::Receiver<()>>,
    release_first_connect: Option<oneshot::Sender<()>>,
    closed_connections: mpsc::UnboundedReceiver<()>,
    task: JoinHandle<()>,
}

impl LoopbackBroker {
    async fn start(gate_first_connect: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (connected_tx, connected_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let first_gate = if gate_first_connect {
            Some((connected_tx, release_rx))
        } else {
            None
        };
        let first_connect = gate_first_connect.then_some(connected_rx);
        let release_first_connect = gate_first_connect.then_some(release_tx);
        let (closed_tx, closed_connections) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut first_gate = first_gate;
            while let Ok((stream, _)) = listener.accept().await {
                let gate = first_gate.take();
                let closed_tx = closed_tx.clone();
                tokio::spawn(async move {
                    let _ = serve_client(stream, gate, closed_tx).await;
                });
            }
        });
        Self {
            address,
            first_connect,
            release_first_connect,
            closed_connections,
            task,
        }
    }

    async fn wait_for_first_connect(&mut self) {
        let connected = self
            .first_connect
            .as_mut()
            .expect("broker is not waiting on its first connection");
        time::timeout(Duration::from_secs(5), connected)
            .await
            .expect("MQTT client did not reach the gated broker")
            .expect("broker task ended before the MQTT client connected");
    }

    fn release_first_connect(&mut self) {
        self.release_first_connect
            .take()
            .expect("broker has no gated first connection")
            .send(())
            .expect("gated MQTT connection ended before release");
    }

    async fn wait_for_closed_connection(&mut self) {
        time::timeout(Duration::from_secs(5), self.closed_connections.recv())
            .await
            .expect("MQTT client did not close its loopback connection")
            .expect("broker task ended before observing the MQTT client close");
    }
}

impl Drop for LoopbackBroker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_client(
    mut stream: TcpStream,
    gate: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    closed_connections: mpsc::UnboundedSender<()>,
) -> io::Result<()> {
    let (header, _) = read_packet(&mut stream).await?;
    if header >> 4 != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected MQTT CONNECT",
        ));
    }
    if let Some((connected, release)) = gate {
        let _ = connected.send(());
        let _ = release.await;
    }
    write_packet(&mut stream, 0x20, &[0, 0]).await?;

    loop {
        let (header, body) = match read_packet(&mut stream).await {
            Ok(packet) => packet,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                let _ = closed_connections.send(());
                return Ok(());
            }
            Err(error) => {
                let _ = closed_connections.send(());
                return Err(error);
            }
        };
        match header >> 4 {
            3 => {
                let qos = (header >> 1) & 0x03;
                if qos > 0 {
                    let mut offset = 0;
                    let topic_len = read_u16(&body, &mut offset)? as usize;
                    offset = offset.checked_add(topic_len).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "topic length overflow")
                    })?;
                    let packet_id = body.get(offset..offset + 2).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "short PUBLISH packet ID")
                    })?;
                    write_packet(&mut stream, 0x40, packet_id).await?;
                }
            }
            8 => {
                let packet_id = body.get(..2).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "short SUBSCRIBE packet ID")
                })?;
                let mut offset = 2;
                let mut grants = Vec::new();
                while offset < body.len() {
                    let topic_len = read_u16(&body, &mut offset)? as usize;
                    offset = offset.checked_add(topic_len).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "topic length overflow")
                    })?;
                    let _requested_qos = body.get(offset).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "missing subscription QoS")
                    })?;
                    offset += 1;
                    grants.push(0);
                }
                let mut acknowledgement = packet_id.to_vec();
                acknowledgement.extend(grants);
                write_packet(&mut stream, 0x90, &acknowledgement).await?;
            }
            10 => {
                let packet_id = body.get(..2).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "short UNSUBSCRIBE packet ID")
                })?;
                write_packet(&mut stream, 0xB0, packet_id).await?;
            }
            12 => write_packet(&mut stream, 0xD0, &[]).await?,
            14 => {
                let _ = closed_connections.send(());
                return Ok(());
            }
            _ => {}
        }
    }
}

async fn read_packet(reader: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0; 1];
    reader.read_exact(&mut header).await?;
    let mut multiplier = 1usize;
    let mut remaining = 0usize;
    loop {
        let mut digit = [0; 1];
        reader.read_exact(&mut digit).await?;
        remaining = remaining.saturating_add(((digit[0] & 0x7f) as usize) * multiplier);
        if digit[0] & 0x80 == 0 {
            break;
        }
        multiplier = multiplier.saturating_mul(128);
        if multiplier > 128 * 128 * 128 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid MQTT remaining length",
            ));
        }
    }
    let mut body = vec![0; remaining];
    reader.read_exact(&mut body).await?;
    Ok((header[0], body))
}

async fn write_packet(writer: &mut TcpStream, header: u8, body: &[u8]) -> io::Result<()> {
    writer.write_all(&[header]).await?;
    let mut remaining = body.len();
    loop {
        let mut digit = (remaining % 128) as u8;
        remaining /= 128;
        if remaining > 0 {
            digit |= 0x80;
        }
        writer.write_all(&[digit]).await?;
        if remaining == 0 {
            break;
        }
    }
    writer.write_all(body).await?;
    writer.flush().await
}

fn read_u16(body: &[u8], offset: &mut usize) -> io::Result<u16> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MQTT field length overflow"))?;
    let bytes = body
        .get(*offset..end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short MQTT string length"))?;
    *offset = end;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn runtime() -> Runtime {
    Runtime::new(Arc::new(MemoryStore::default())).unwrap()
}

fn mqtt_config(address: &str, client_id: &str) -> MqttConfig {
    let (host, port) = address.rsplit_once(':').unwrap();
    serde_json::from_value(json!({
        "host": host,
        "port": port.parse::<u16>().unwrap(),
        "client_id": client_id,
        "discovery_enabled": true
    }))
    .unwrap()
}

async fn wait_for_entry(
    entries: &ConfigEntries,
    entry_id: &str,
    predicate: impl Fn(&ConfigEntry) -> bool,
) -> ConfigEntry {
    time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(entry) = entries.get_entry(entry_id)
                && predicate(&entry)
            {
                return entry;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("config entry did not reach the expected lifecycle state")
}

async fn start_again_when_unregistered(
    runtime: &Runtime,
    entries: &ConfigEntries,
    registries: &Registries,
) -> NativeDrivers {
    let mut last_error = None;
    time::timeout(Duration::from_secs(5), async {
        loop {
            match NativeDrivers::start(
                runtime.clone(),
                entries.clone(),
                registries.clone(),
                Vec::new(),
                None,
            )
            .await
            {
                Ok(host) => return host,
                Err(error) => {
                    last_error = Some(error.to_string());
                    tokio::task::yield_now().await;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("same-manager NativeDrivers restart failed: {last_error:?}"))
}

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.poll(&mut context)
}

#[tokio::test(flavor = "current_thread")]
async fn normal_shutdown_unloads_and_unregisters_for_same_manager_restart() {
    let mut broker = LoopbackBroker::start(false).await;
    let runtime = runtime();
    let entries = ConfigEntries::new(runtime.clone()).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();

    let host = NativeDrivers::start(
        runtime.clone(),
        entries.clone(),
        registries.clone(),
        Vec::new(),
        Some(mqtt_config(&broker.address, "lifecycle-normal-shutdown")),
    )
    .await
    .unwrap();
    let entry = entries
        .list_entries(Some(MQTT_DOMAIN))
        .pop()
        .expect("YAML MQTT startup should create an entry");
    assert_eq!(
        wait_for_entry(&entries, &entry.entry_id, |entry| {
            entry.state == ConfigEntryState::Loaded
        })
        .await
        .state,
        ConfigEntryState::Loaded
    );

    host.shutdown().await.unwrap();
    assert_eq!(
        entries.get_entry(&entry.entry_id).unwrap().state,
        ConfigEntryState::NotLoaded
    );
    broker.wait_for_closed_connection().await;

    let restarted = NativeDrivers::start(
        runtime.clone(),
        entries.clone(),
        registries.clone(),
        Vec::new(),
        None,
    )
    .await
    .expect("normal shutdown must unregister its driver from the same ConfigEntries");
    assert_eq!(
        wait_for_entry(&entries, &entry.entry_id, |entry| {
            entry.state == ConfigEntryState::Loaded
        })
        .await
        .state,
        ConfigEntryState::Loaded
    );
    restarted.shutdown().await.unwrap();
    assert_eq!(
        entries.get_entry(&entry.entry_id).unwrap().state,
        ConfigEntryState::NotLoaded
    );
    broker.wait_for_closed_connection().await;
}

#[tokio::test(flavor = "current_thread")]
async fn canceled_startup_joins_setup_and_unregisters_its_driver() {
    let mut broker = LoopbackBroker::start(true).await;
    let runtime = runtime();
    let entries = ConfigEntries::new(runtime.clone()).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let mut startup = Box::pin(NativeDrivers::start(
        runtime.clone(),
        entries.clone(),
        registries.clone(),
        Vec::new(),
        Some(mqtt_config(&broker.address, "lifecycle-canceled-startup")),
    ));

    assert!(poll_once(startup.as_mut()).is_pending());
    broker.wait_for_first_connect().await;
    let entry = entries
        .list_entries(Some(MQTT_DOMAIN))
        .pop()
        .expect("the blocked startup already imported its MQTT entry");
    drop(startup);
    broker.release_first_connect();

    broker.wait_for_closed_connection().await;
    let settled = wait_for_entry(&entries, &entry.entry_id, |entry| {
        entry.state != ConfigEntryState::SetupInProgress
    })
    .await;
    assert_ne!(settled.state, ConfigEntryState::Loaded);
    assert!(runtime.services().as_object().unwrap().is_empty());

    let restarted = start_again_when_unregistered(&runtime, &entries, &registries).await;
    restarted.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_pending_shutdown_still_unloads_disconnects_and_allows_restart() {
    let mut broker = LoopbackBroker::start(false).await;
    let runtime = runtime();
    let entries = ConfigEntries::new(runtime.clone()).unwrap();
    let registries = Registries::new(runtime.clone()).unwrap();
    let host = NativeDrivers::start(
        runtime.clone(),
        entries.clone(),
        registries.clone(),
        Vec::new(),
        Some(mqtt_config(&broker.address, "lifecycle-canceled-shutdown")),
    )
    .await
    .unwrap();
    let entry = entries
        .list_entries(Some(MQTT_DOMAIN))
        .pop()
        .expect("YAML MQTT startup should create an entry");
    wait_for_entry(&entries, &entry.entry_id, |entry| {
        entry.state == ConfigEntryState::Loaded
    })
    .await;

    let mut shutdown = Box::pin(host.shutdown());
    assert!(poll_once(shutdown.as_mut()).is_pending());
    drop(shutdown);

    broker.wait_for_closed_connection().await;
    let unloaded = wait_for_entry(&entries, &entry.entry_id, |entry| {
        entry.state == ConfigEntryState::NotLoaded
    })
    .await;
    assert_eq!(unloaded.state, ConfigEntryState::NotLoaded);

    let restarted = start_again_when_unregistered(&runtime, &entries, &registries).await;
    wait_for_entry(&entries, &entry.entry_id, |entry| {
        entry.state == ConfigEntryState::Loaded
    })
    .await;
    restarted.shutdown().await.unwrap();
    assert_eq!(
        entries.get_entry(&entry.entry_id).unwrap().state,
        ConfigEntryState::NotLoaded
    );
    broker.wait_for_closed_connection().await;
}
