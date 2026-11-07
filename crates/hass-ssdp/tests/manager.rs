use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use chrono::{DateTime, TimeZone, Utc};
use hass_ssdp::{
    DiscoveryAction, DiscoveryFuture, DiscoverySink, IntegrationMatchersConfig, MatchDict,
    SsdpAsyncCallback, SsdpCallback, SsdpChange, SsdpIntegration, SsdpManager, SsdpServiceInfo,
    decode_ssdp_packet, owned_discovery_task,
};
use tokio::sync::{Notify, mpsc};

const DEVICE_XML: &str = "<root><device><friendlyName>Fixture</friendlyName><manufacturer>Acme</manufacturer><modelName>Model 1</modelName></device></root>";

struct HttpFixture {
    address: SocketAddr,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl HttpFixture {
    fn start(response_delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
        listener
            .set_nonblocking(true)
            .expect("make loopback fixture nonblocking");
        let address = listener.local_addr().expect("fixture address");
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_requests = requests.clone();
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // macOS accepted sockets inherit the listener's
                        // nonblocking flag. Read the full HTTP request before
                        // selecting its status fixture, rather than racing the
                        // client's first write and returning the wrong body.
                        stream
                            .set_nonblocking(false)
                            .expect("blocking fixture request");
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let request = read_request(&mut stream);
                        worker_requests.fetch_add(1, Ordering::AcqRel);
                        if !response_delay.is_zero() {
                            thread::sleep(response_delay);
                        }

                        let missing = String::from_utf8_lossy(&request)
                            .lines()
                            .next()
                            .is_some_and(|line| line.contains(" /missing.xml "));
                        let (status, body) = if missing {
                            ("404 Not Found", "")
                        } else {
                            ("200 OK", DEVICE_XML)
                        };
                        let response = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            address,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn location(&self, path: &str) -> String {
        format!("http://device.invalid:{}{path}", self.address.port())
    }

    fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .resolve("device.invalid", self.address)
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("build loopback-only HTTP client")
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::Acquire)
    }

    async fn wait_for_requests(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.request_count() < count && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            self.request_count() >= count,
            "expected at least {count} loopback HTTP requests, got {}",
            self.request_count()
        );
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join loopback HTTP fixture");
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    request
}

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0)
        .single()
        .expect("valid fixture timestamp")
}

fn decode(packet: String, seconds: i64) -> hass_ssdp::SsdpMessage {
    let remote: SocketAddr = "192.0.2.18:1900".parse().expect("valid remote address");
    decode_ssdp_packet(packet.as_bytes(), None, remote, at(seconds)).expect("decode fixture packet")
}

fn search_response(
    udn: &str,
    st: &str,
    location: &str,
    seconds: i64,
    extra: &[(&str, &str)],
) -> hass_ssdp::SsdpMessage {
    let mut packet = format!(
        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=3600\r\nLOCATION: {location}\r\nST: {st}\r\nUSN: {udn}::{st}\r\nSERVER: Media/1\r\nX-Capability: present\r\n"
    );
    for (name, value) in extra {
        packet.push_str(name);
        packet.push_str(": ");
        packet.push_str(value);
        packet.push_str("\r\n");
    }
    packet.push_str("\r\n");
    decode(packet, seconds)
}

fn byebye(udn: &str, nt: &str, location: &str, seconds: i64) -> hass_ssdp::SsdpMessage {
    decode(
        format!(
            "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nNT: {nt}\r\nNTS: ssdp:byebye\r\nUSN: {udn}::{nt}\r\nLOCATION: {location}\r\nSERVER: Media/1\r\nX-Capability: present\r\n\r\n"
        ),
        seconds,
    )
}

fn dict(entries: &[(&str, &str)]) -> MatchDict {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn callback_log(log: Arc<Mutex<Vec<(SsdpChange, SsdpServiceInfo)>>>) -> SsdpCallback {
    Arc::new(move |info, change| {
        log.lock()
            .expect("callback log lock")
            .push((change, info.clone()));
        Ok(())
    })
}

fn action_channel() -> (DiscoverySink, mpsc::UnboundedReceiver<DiscoveryAction>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let sink: DiscoverySink = Arc::new(move |action| -> DiscoveryFuture {
        let _ = sender.send(action);
        Box::pin(async { Ok(()) })
    });
    (sink, receiver)
}

async fn next_action(receiver: &mut mpsc::UnboundedReceiver<DiscoveryAction>) -> DiscoveryAction {
    tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("wait for discovery action")
        .expect("action sender remains alive")
}

#[tokio::test]
async fn decoded_observations_share_one_tracker_and_negative_description_cache() {
    let http = HttpFixture::start(Duration::from_millis(120));
    let integration =
        SsdpIntegration::with_client(http.client(), IntegrationMatchersConfig::new(), None);
    let manager = integration.manager();
    let location = http.location("/missing.xml");
    let first = search_response("uuid:shared", "urn:fixture:first", &location, 1, &[]);
    let second = search_response("uuid:shared", "urn:fixture:second", &location, 2, &[]);

    let (first_result, second_result) = tokio::join!(
        manager.observe(&first, at(1)),
        manager.observe(&second, at(2))
    );
    assert!(first_result.expect("first observation"));
    assert!(second_result.expect("second observation"));

    // The two independently decoded ST values share the device cache and one
    // concurrent description fetch. A cached 404 is not retried on later reads.
    assert_eq!(http.request_count(), 1);
    let repeated = manager
        .observe(&first, at(3))
        .await
        .expect("repeated observation");
    assert!(repeated);
    let services = manager
        .get_by_udn("uuid:shared")
        .await
        .expect("services by UDN");
    assert_eq!(services.len(), 2);
    assert_eq!(services[0].ssdp_st, "urn:fixture:first");
    assert_eq!(services[1].ssdp_st, "urn:fixture:second");
    assert!(services.iter().all(|service| {
        service.upnp.get("UDN").and_then(|value| value.as_str()) == Some("uuid:shared")
            && !service.upnp.contains_key("manufacturer")
    }));
    assert_eq!(http.request_count(), 1);
    assert_eq!(
        manager
            .get_by_st("urn:fixture:second")
            .await
            .expect("services by ST")
            .len(),
        1
    );
}

#[tokio::test]
async fn callback_replay_keeps_first_seen_device_and_type_order() {
    let http = HttpFixture::start(Duration::ZERO);
    let integration =
        SsdpIntegration::with_client(http.client(), IntegrationMatchersConfig::new(), None);
    let manager = integration.manager();
    let location = http.location("/device.xml");
    for (udn, st, seconds) in [
        ("uuid:first", "urn:fixture:z-first", 1),
        ("uuid:first", "urn:fixture:a-second", 2),
        ("uuid:second", "urn:fixture:b-third", 3),
    ] {
        manager
            .observe(
                &search_response(udn, st, &location, seconds, &[]),
                at(seconds),
            )
            .await
            .expect("seed tracker");
    }

    let observed = Arc::new(Mutex::new(Vec::new()));
    let _guard = manager
        .register_callback(
            dict(&[("sErVeR", "Media/1")]),
            callback_log(observed.clone()),
        )
        .await
        .expect("register replay callback");

    let observed = observed.lock().expect("callback log lock");
    assert_eq!(
        observed
            .iter()
            .map(|(change, info)| (*change, info.ssdp_usn.as_str(), info.ssdp_st.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (
                SsdpChange::Alive,
                "uuid:first::urn:fixture:z-first",
                "urn:fixture:z-first"
            ),
            (
                SsdpChange::Alive,
                "uuid:first::urn:fixture:a-second",
                "urn:fixture:a-second"
            ),
            (
                SsdpChange::Alive,
                "uuid:second::urn:fixture:b-third",
                "urn:fixture:b-third"
            ),
        ]
    );
    assert!(
        observed
            .iter()
            .all(|(_, info)| { info.x_homeassistant_matching_domains.is_empty() })
    );
}

#[tokio::test]
async fn description_lookup_combines_the_latest_headers_after_a_concurrent_update() {
    let http = HttpFixture::start(Duration::from_millis(150));
    let integration = SsdpIntegration::with_client(http.client(), Default::default(), None);
    let manager = integration.manager();
    let log = Arc::new(Mutex::new(Vec::new()));
    let _guard = manager
        .register_callback(MatchDict::new(), callback_log(log.clone()))
        .await
        .unwrap();
    let old_location = http.location("/zzz.xml");
    let new_location = http.location("/aaa.xml");
    let first = search_response(
        "uuid:updating",
        "type",
        &old_location,
        1,
        &[("X-Version", "old")],
    );
    let observing = manager.clone();
    let first_job = tokio::spawn(async move { observing.observe(&first, at(1)).await });
    http.wait_for_requests(1).await;
    let second = search_response(
        "uuid:updating",
        "type",
        &new_location,
        2,
        &[("X-Version", "new")],
    );
    manager.observe(&second, at(2)).await.unwrap();
    first_job.await.unwrap().unwrap();
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    for (_, info) in seen {
        assert_eq!(info.ssdp_location.as_deref(), Some(new_location.as_str()));
        assert_eq!(info.ssdp_headers["X-Version"], "new");
        assert_eq!(info.ssdp_all_locations.len(), 2);
    }
}

#[tokio::test]
async fn live_domains_alive_filtering_byebye_and_getters_keep_their_contracts() {
    let http = HttpFixture::start(Duration::ZERO);
    let (sink, mut actions) = action_channel();
    let matchers = BTreeMap::from([(
        "acme_device".to_owned(),
        vec![
            dict(&[("st", "urn:fixture:device"), ("manufacturer", "Acme")]),
            dict(&[("nt", "urn:fixture:device"), ("manufacturer", "Acme")]),
        ],
    )]);
    let integration = SsdpIntegration::with_client(http.client(), matchers, Some(sink));
    let manager = integration.manager();

    let accepted_count = Arc::new(AtomicUsize::new(0));
    let wrong_value_count = Arc::new(AtomicUsize::new(0));
    let missing_presence_count = Arc::new(AtomicUsize::new(0));
    let callback_info = Arc::new(Mutex::new(Vec::new()));
    let accepted_count_for_callback = accepted_count.clone();
    let callback_info_for_callback = callback_info.clone();
    let _accepted = manager
        .register_callback(
            dict(&[("sErVeR", "Media/1"), ("x-Capability", "*")]),
            Arc::new(move |info, change| {
                accepted_count_for_callback.fetch_add(1, Ordering::AcqRel);
                callback_info_for_callback
                    .lock()
                    .expect("callback detail lock")
                    .push((change, info.clone()));
                Ok(())
            }),
        )
        .await
        .expect("register accepted callback");
    let wrong_value = wrong_value_count.clone();
    let _wrong_value = manager
        .register_callback(
            dict(&[("server", "media/1")]),
            Arc::new(move |_, _| {
                wrong_value.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }),
        )
        .await
        .expect("register exact-value falsifier");
    let missing_presence = missing_presence_count.clone();
    let _missing_presence = manager
        .register_callback(
            dict(&[("x-not-present", "*")]),
            Arc::new(move |_, _| {
                missing_presence.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }),
        )
        .await
        .expect("register presence falsifier");

    let first_location = http.location("/device.xml");
    let first = search_response("uuid:live", "urn:fixture:device", &first_location, 1, &[]);
    manager.observe(&first, at(1)).await.expect("first search");
    match next_action(&mut actions).await {
        DiscoveryAction::Create { domain, info, udn } => {
            assert_eq!(domain, "acme_device");
            assert_eq!(udn, "uuid:live");
            assert_eq!(
                info.x_homeassistant_matching_domains,
                BTreeSet::from(["acme_device".into()])
            );
        }
        action => panic!("expected matched create, received {action:?}"),
    }

    // Identical data is a SearchAlive callback. It retains the callback but
    // does not recalculate domains or enqueue another config flow.
    manager
        .observe(
            &search_response("uuid:live", "urn:fixture:device", &first_location, 2, &[]),
            at(2),
        )
        .await
        .expect("repeated search");
    tokio::time::timeout(Duration::from_millis(75), actions.recv())
        .await
        .expect_err("SearchAlive must not create another flow");

    let second_location = http.location("/alternate.xml");
    manager
        .observe(
            &search_response("uuid:live", "urn:fixture:device", &second_location, 3, &[]),
            at(3),
        )
        .await
        .expect("same service at another location");
    tokio::time::timeout(Duration::from_millis(75), actions.recv())
        .await
        .expect_err("another SearchAlive must not create a flow");

    let by_udn = manager.get_by_udn("uuid:live").await.expect("get by UDN");
    let by_st = manager
        .get_by_st("urn:fixture:device")
        .await
        .expect("get by ST");
    let exact = manager
        .get_by_udn_and_st("uuid:live", "urn:fixture:device")
        .await
        .expect("get exact service")
        .expect("service exists");
    assert_eq!(by_udn.len(), 1);
    assert_eq!(by_st.len(), 1);
    let expected_locations = BTreeSet::from([first_location.clone(), second_location.clone()]);
    assert_eq!(by_udn[0].ssdp_all_locations, expected_locations);
    assert_eq!(by_st[0].ssdp_all_locations, expected_locations);
    assert_eq!(exact.ssdp_all_locations, expected_locations);
    assert!(
        by_udn
            .iter()
            .chain(&by_st)
            .all(|info| { info.x_homeassistant_matching_domains.is_empty() })
    );
    assert!(exact.x_homeassistant_matching_domains.is_empty());

    let byebye = byebye("uuid:live", "urn:fixture:device", &second_location, 4);
    manager
        .observe(&byebye, at(4))
        .await
        .expect("byebye observation");
    assert_eq!(
        next_action(&mut actions).await,
        DiscoveryAction::Dismiss {
            ssdp_st: "urn:fixture:device".into(),
            ssdp_location: Some(second_location.clone()),
        }
    );
    tokio::time::timeout(Duration::from_millis(75), actions.recv())
        .await
        .expect_err("BYEBYE must dismiss without creating another flow");

    assert_eq!(accepted_count.load(Ordering::Acquire), 4);
    assert_eq!(wrong_value_count.load(Ordering::Acquire), 0);
    assert_eq!(missing_presence_count.load(Ordering::Acquire), 0);
    let callback_info = callback_info.lock().expect("callback detail lock").clone();
    assert_eq!(callback_info[0].0, SsdpChange::Alive);
    assert_eq!(
        callback_info[0].1.x_homeassistant_matching_domains,
        BTreeSet::from(["acme_device".into()])
    );
    assert_eq!(callback_info[1].0, SsdpChange::Alive);
    assert!(
        callback_info[1]
            .1
            .x_homeassistant_matching_domains
            .is_empty()
    );
    assert_eq!(callback_info[2].0, SsdpChange::Alive);
    assert!(
        callback_info[2]
            .1
            .x_homeassistant_matching_domains
            .is_empty()
    );
    assert_eq!(callback_info[3].0, SsdpChange::Byebye);

    let by_udn = manager.get_by_udn("uuid:live").await.expect("get by UDN");
    assert!(by_udn.is_empty(), "BYEBYE removes the tracked service");

    assert!(
        manager
            .get_by_udn("uuid:live")
            .await
            .expect("get after BYEBYE")
            .is_empty()
    );
}

#[tokio::test]
async fn dropping_one_callback_guard_keeps_its_peer_registered() {
    let http = HttpFixture::start(Duration::ZERO);
    let integration =
        SsdpIntegration::with_client(http.client(), IntegrationMatchersConfig::new(), None);
    let manager = integration.manager();
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let first_counter = first_calls.clone();
    let first_guard = manager
        .register_callback(
            dict(&[("server", "Media/1")]),
            Arc::new(move |_, _| {
                first_counter.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }),
        )
        .await
        .expect("register first callback");
    let second_counter = second_calls.clone();
    let _second_guard = manager
        .register_callback(
            dict(&[("SERVER", "Media/1")]),
            Arc::new(move |_, _| {
                second_counter.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }),
        )
        .await
        .expect("register second callback");

    drop(first_guard);
    let location = http.location("/device.xml");
    manager
        .observe(
            &search_response("uuid:guards", "urn:fixture:device", &location, 1, &[]),
            at(1),
        )
        .await
        .expect("observe after dropping first guard");
    assert_eq!(first_calls.load(Ordering::Acquire), 0);
    assert_eq!(second_calls.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn rediscovery_only_uses_ready_nonempty_descriptions_without_callbacks() {
    let http = HttpFixture::start(Duration::from_millis(180));
    let (sink, mut actions) = action_channel();
    let matchers = BTreeMap::from([(
        "acme_device".to_owned(),
        vec![dict(&[
            ("st", "urn:fixture:device"),
            ("manufacturer", "Acme"),
        ])],
    )]);
    let integration = SsdpIntegration::with_client(http.client(), matchers, Some(sink));
    let manager = integration.manager();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let callback_count = callbacks.clone();
    let _callback = manager
        .register_callback(
            dict(&[("server", "Media/1")]),
            Arc::new(move |_, _| {
                callback_count.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }),
        )
        .await
        .expect("register ordinary callback");

    let location = http.location("/device.xml");
    let message = search_response("uuid:ready", "urn:fixture:device", &location, 1, &[]);
    let observing_manager: SsdpManager = manager.clone();
    let observation = tokio::spawn(async move { observing_manager.observe(&message, at(1)).await });
    http.wait_for_requests(1).await;

    // The tracker already has the device, but the description is still in
    // flight. Rediscovery must neither fetch again nor deliver ordinary events.
    manager
        .rediscover_cached_udn("uuid:ready")
        .expect("rediscovery during fetch");
    assert_eq!(callbacks.load(Ordering::Acquire), 0);
    assert!(actions.try_recv().is_err());

    observation
        .await
        .expect("observation task")
        .expect("completed observation");
    match next_action(&mut actions).await {
        DiscoveryAction::Create { domain, udn, info } => {
            assert_eq!(domain, "acme_device");
            assert_eq!(udn, "uuid:ready");
            assert_eq!(
                info.upnp
                    .get("manufacturer")
                    .and_then(|value| value.as_str()),
                Some("Acme")
            );
        }
        action => panic!("expected initial create, received {action:?}"),
    }
    assert_eq!(callbacks.load(Ordering::Acquire), 1);

    manager
        .rediscover_cached_udn("uuid:ready")
        .expect("rediscover ready description");
    match next_action(&mut actions).await {
        DiscoveryAction::Create { domain, udn, info } => {
            assert_eq!(domain, "acme_device");
            assert_eq!(udn, "uuid:ready");
            assert_eq!(
                info.x_homeassistant_matching_domains,
                BTreeSet::from(["acme_device".into()])
            );
        }
        action => panic!("expected rediscovery create, received {action:?}"),
    }
    assert_eq!(callbacks.load(Ordering::Acquire), 1);
    assert_eq!(http.request_count(), 1);

    // A ready negative cache entry cannot create a match and rediscovery does
    // not retry the device's missing document.
    let missing = http.location("/missing.xml");
    manager
        .observe(
            &search_response("uuid:missing", "urn:fixture:device", &missing, 2, &[]),
            at(2),
        )
        .await
        .expect("observe device with absent description");
    assert_eq!(callbacks.load(Ordering::Acquire), 2);
    assert!(actions.try_recv().is_err());
    manager
        .rediscover_cached_udn("uuid:missing")
        .expect("rediscover negative cached description");
    assert_eq!(callbacks.load(Ordering::Acquire), 2);
    assert!(actions.try_recv().is_err());
    assert_eq!(http.request_count(), 2);
}

struct DropNotice(Arc<AtomicBool>);

impl Drop for DropNotice {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[tokio::test]
async fn async_callbacks_cancel_owned_futures_and_transferred_spawned_tasks() {
    for spawned in [false, true] {
        let http = HttpFixture::start(Duration::ZERO);
        let integration = SsdpIntegration::with_client(http.client(), Default::default(), None);
        let manager = integration.manager();
        let started = Arc::new(Notify::new());
        let stopped = Arc::new(AtomicBool::new(false));
        let start_notice = started.clone();
        let stop_notice = stopped.clone();
        let callback: SsdpAsyncCallback = Arc::new(move |info, change| {
            assert_eq!(info.ssdp_udn.as_deref(), Some("uuid:async"));
            assert_eq!(change, SsdpChange::Alive);
            let started = start_notice.clone();
            let stopped = stop_notice.clone();
            let future = async move {
                let _notice = DropNotice(stopped);
                started.notify_one();
                std::future::pending::<()>().await;
                Ok(())
            };
            if spawned {
                owned_discovery_task(tokio::spawn(future))
            } else {
                Box::pin(future)
            }
        });
        let _guard = manager
            .register_async_callback(MatchDict::new(), callback)
            .await
            .unwrap();
        manager
            .observe(
                &search_response("uuid:async", "type", &http.location("/device.xml"), 1, &[]),
                at(1),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .unwrap();
        integration.shutdown().await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !stopped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(manager.get_by_udn("uuid:async").await.is_err());
    }
}

#[tokio::test]
async fn explicit_shutdown_cancels_pending_flow_work_and_stops_weak_managers() {
    let http = HttpFixture::start(Duration::ZERO);
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let sink_started = started.clone();
    let sink_dropped = dropped.clone();
    let sink: DiscoverySink = Arc::new(move |_| -> DiscoveryFuture {
        let started = sink_started.clone();
        let dropped = sink_dropped.clone();
        Box::pin(async move {
            let _notice = DropNotice(dropped);
            started.notify_one();
            std::future::pending::<hass_core::Result<()>>().await
        })
    });
    let matchers = BTreeMap::from([(
        "st_only".to_owned(),
        vec![dict(&[("st", "urn:fixture:device")])],
    )]);
    let integration = SsdpIntegration::with_client(http.client(), matchers, Some(sink));
    let manager = integration.manager();
    let location = http.location("/device.xml");
    manager
        .observe(
            &search_response("uuid:shutdown", "urn:fixture:device", &location, 1, &[]),
            at(1),
        )
        .await
        .expect("observation dispatches pending flow work");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("flow job started");

    integration.shutdown().await;
    assert!(
        dropped.load(Ordering::Acquire),
        "shutdown joined the cancelled sink future"
    );
    assert!(manager.get_by_udn("uuid:shutdown").await.is_err());
    assert!(
        manager
            .observe(
                &search_response("uuid:shutdown", "urn:fixture:device", &location, 2, &[]),
                at(2),
            )
            .await
            .is_err()
    );
}
