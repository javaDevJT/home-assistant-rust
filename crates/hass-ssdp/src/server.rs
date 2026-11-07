//! Native UPnP device description and SSDP advertisement server.
//!
//! This is deliberately a small single-device implementation: the HTTP
//! listener serves one escaped UPnP device description, while the UDP listener
//! answers M-SEARCH and owns its advertisements for its full lifetime.

use std::{
    io,
    net::{SocketAddr, SocketAddrV4, SocketAddrV6, TcpListener as StdTcpListener},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::Utc;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::watch,
    task::{JoinHandle, JoinSet},
    time::{self, Instant, MissedTickBehavior},
};

const DEVICE_TYPE: &str = "urn:home-assistant.io:device:HomeAssistant:1";
const ROOT_DEVICE: &str = "upnp:rootdevice";
const HTTP_DEVICE_PATH: &str = "/device.xml";
const HTTP_REQUEST_LIMIT: usize = 8 * 1024;
const SSDP_PACKET_LIMIT: usize = 8 * 1024;
const MAX_HTTP_CONNECTIONS: usize = 32;
const MAX_SEARCH_TASKS: usize = 32;
const MX_MAX_SECONDS: u32 = 5;

/// An icon entry in a UPnP device description.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceIcon {
    /// MIME type, such as `image/png`.
    pub mimetype: String,
    /// Icon width in pixels.
    pub width: u16,
    /// Icon height in pixels.
    pub height: u16,
    /// Icon color depth in bits per pixel.
    pub depth: u16,
    /// Device-relative or absolute icon URL.
    pub url: String,
}

/// Metadata rendered by the native UPnP description server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpnpDevice {
    /// Stable UPnP unique device name, conventionally prefixed with `uuid:`.
    pub udn: String,
    /// Human-readable device name.
    pub friendly_name: String,
    /// Manufacturer model name.
    pub model_name: String,
    /// Manufacturer model number or native application version.
    pub model_number: String,
    /// Stable serial number for this device instance.
    pub serial_number: String,
    /// URL users should open to reach the application UI.
    pub presentation_url: String,
    /// Available device icons. An empty list emits an empty `iconList`.
    pub icons: Vec<DeviceIcon>,
}

/// Port, advertisement, and UPnP identity settings for [`UpnpServer`].
#[derive(Clone, Debug)]
pub struct UpnpServerOptions {
    /// TCP port for `/device.xml`; zero selects an available ephemeral port.
    pub http_port: u16,
    /// Local SSDP UDP port; zero selects an available ephemeral port.
    pub ssdp_port: u16,
    /// Interval between periodic `ssdp:alive` notifications.
    pub announce_interval: Duration,
    /// Cache lifetime advertised in seconds.
    pub cache_max_age: u32,
    /// UPnP boot identifier.
    pub boot_id: u32,
    /// UPnP configuration identifier.
    pub config_id: u32,
}

impl Default for UpnpServerOptions {
    fn default() -> Self {
        Self {
            http_port: 0,
            ssdp_port: 1900,
            announce_interval: Duration::from_secs(30),
            cache_max_age: 1800,
            boot_id: 1,
            config_id: 1,
        }
    }
}

/// A running HTTP UPnP description server and SSDP responder/advertiser.
///
/// All listener and worker tasks are owned by this value. Call [`shutdown`]
/// for best-effort BYEBYE delivery and joined shutdown; dropping the server
/// signals its tasks and aborts them as a fallback.
pub struct UpnpServer {
    device_url: String,
    http_addr: SocketAddr,
    ssdp_addr: SocketAddr,
    target: SocketAddr,
    options: UpnpServerOptions,
    announcement_socket: Arc<UdpSocket>,
    device: Arc<UpnpDevice>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    shutdown_complete: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Record {
    Root,
    Udn,
    DeviceType,
}

impl Record {
    fn nt(self, udn: &str) -> String {
        match self {
            Self::Root => ROOT_DEVICE.to_owned(),
            Self::Udn => udn.to_owned(),
            Self::DeviceType => DEVICE_TYPE.to_owned(),
        }
    }

    fn usn(self, udn: &str) -> String {
        match self {
            Self::Root => format!("{udn}::{ROOT_DEVICE}"),
            Self::Udn => udn.to_owned(),
            Self::DeviceType => format!("{udn}::{DEVICE_TYPE}"),
        }
    }
}

impl UpnpServer {
    /// Bind the HTTP and SSDP sockets and start serving one UPnP device.
    ///
    /// `source` selects the local network interface. Its IPv6 scope identifier
    /// is retained when binding listeners and selecting the multicast interface.
    /// `target` is normally an SSDP multicast group, and can be an explicit
    /// loopback destination for deterministic tests.
    pub async fn start(
        device: UpnpDevice,
        source: SocketAddr,
        target: SocketAddr,
        options: UpnpServerOptions,
    ) -> io::Result<Self> {
        validate_start(&device, source, target, &options)?;

        let tcp_std = bind_tcp(source, options.http_port)?;
        let http_addr = tcp_std.local_addr()?;
        let listener = TcpListener::from_std(tcp_std)?;
        let device_url = device_url(http_addr);
        let xml: Arc<str> = build_device_xml(&device).into();

        let search_std = bind_udp(source, options.ssdp_port, target, true)?;
        let ssdp_addr = with_port(source, search_std.local_addr()?.port());
        let search_socket = Arc::new(UdpSocket::from_std(search_std)?);
        let announce_std = bind_udp(source, 0, target, false)?;
        let announcement_socket = Arc::new(UdpSocket::from_std(announce_std)?);
        let response_std = bind_udp(source, 0, target, false)?;
        let response_socket = Arc::new(UdpSocket::from_std(response_std)?);
        let device = Arc::new(device);

        // Announce synchronously so that successful startup means the initial
        // alive datagram has already been handed to the local UDP stack.
        let initial_alive = build_notify(
            Record::Root,
            &device.udn,
            "ssdp:alive",
            &device_url,
            target,
            &options,
        );
        announcement_socket
            .send_to(initial_alive.as_bytes(), target)
            .await?;

        let (shutdown, rx) = watch::channel(false);
        let tasks = vec![
            tokio::spawn(serve_http(listener, xml, rx.clone())),
            tokio::spawn(serve_searches(
                search_socket,
                response_socket,
                device.clone(),
                device_url.clone(),
                options.clone(),
                rx.clone(),
            )),
            tokio::spawn(announce_alive(
                announcement_socket.clone(),
                device.clone(),
                device_url.clone(),
                target,
                options.clone(),
                rx,
            )),
        ];

        Ok(Self {
            device_url,
            http_addr,
            ssdp_addr,
            target,
            options,
            announcement_socket,
            device,
            shutdown,
            tasks,
            shutdown_complete: false,
        })
    }

    /// The absolute URL advertised in SSDP responses.
    pub fn device_url(&self) -> &str {
        &self.device_url
    }

    /// Bound TCP address serving the UPnP description.
    pub fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    /// Bound UDP address receiving M-SEARCH datagrams.
    pub fn ssdp_addr(&self) -> SocketAddr {
        self.ssdp_addr
    }

    /// Send BYEBYE for all three UPnP search records, then stop and join tasks.
    pub async fn shutdown(mut self) -> io::Result<()> {
        let mut first_error = None;
        for record in [Record::Root, Record::Udn, Record::DeviceType] {
            let packet = build_notify(
                record,
                &self.device.udn,
                "ssdp:byebye",
                &self.device_url,
                self.target,
                &self.options,
            );
            if let Err(error) = self
                .announcement_socket
                .send_to(packet.as_bytes(), self.target)
                .await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }

        self.shutdown.send_replace(true);
        for task in self.tasks.drain(..) {
            if let Err(error) = task.await
                && first_error.is_none()
                && !error.is_cancelled()
            {
                first_error = Some(io::Error::other(error.to_string()));
            }
        }
        self.shutdown_complete = true;
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for UpnpServer {
    fn drop(&mut self) {
        if self.shutdown_complete {
            return;
        }
        self.shutdown.send_replace(true);
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

fn validate_start(
    device: &UpnpDevice,
    source: SocketAddr,
    target: SocketAddr,
    options: &UpnpServerOptions,
) -> io::Result<()> {
    if source.is_ipv4() != target.is_ipv4() {
        return Err(invalid_input(
            "source and SSDP target must use the same IP family",
        ));
    }
    if source.ip().is_unspecified() {
        return Err(invalid_input(
            "source must identify a concrete network interface",
        ));
    }
    if target.port() == 0 {
        return Err(invalid_input("SSDP target port must not be zero"));
    }
    if options.announce_interval.is_zero() {
        return Err(invalid_input("announce interval must be greater than zero"));
    }
    if options.boot_id == 0 || options.boot_id > 2_147_483_647 {
        return Err(invalid_input("UPnP boot id must be in 1..=2147483647"));
    }
    if options.config_id > 16_777_215 {
        return Err(invalid_input("UPnP config id must be in 0..=16777215"));
    }
    if device.udn.is_empty()
        || device.udn.bytes().any(|byte| byte.is_ascii_control())
        || !device.udn.to_ascii_lowercase().starts_with("uuid:")
    {
        return Err(invalid_input(
            "UDN must be a non-empty UUID URN without control characters",
        ));
    }
    for value in [
        device.friendly_name.as_str(),
        device.model_name.as_str(),
        device.model_number.as_str(),
        device.serial_number.as_str(),
        device.presentation_url.as_str(),
    ] {
        if !is_xml_text(value) {
            return Err(invalid_input(
                "device metadata contains invalid XML characters",
            ));
        }
    }
    for icon in &device.icons {
        if !is_xml_text(&icon.mimetype) || !is_xml_text(&icon.url) {
            return Err(invalid_input(
                "icon metadata contains invalid XML characters",
            ));
        }
    }
    Ok(())
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn bind_tcp(source: SocketAddr, port: u16) -> io::Result<StdTcpListener> {
    let address = with_port(source, port);
    let domain = domain_for(address);
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    let _ = socket.set_reuse_port(true);
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.bind(&SockAddr::from(address))?;
    socket.listen(128)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

fn bind_udp(
    source: SocketAddr,
    port: u16,
    target: SocketAddr,
    join_group: bool,
) -> io::Result<std::net::UdpSocket> {
    // Multicast destinations do not match a socket bound to a unicast address.
    // Join on the selected source interface, but listen on the wildcard address
    // as the pinned responder does. Outbound sockets remain source-bound.
    let address = if join_group {
        match source {
            SocketAddr::V4(_) => SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), port),
            SocketAddr::V6(_) => SocketAddr::new(std::net::Ipv6Addr::UNSPECIFIED.into(), port),
        }
    } else {
        with_port(source, port)
    };
    let socket = Socket::new(domain_for(address), Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    let _ = socket.set_reuse_port(true);
    let _ = socket.set_broadcast(true);
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }

    if target.ip().is_multicast() {
        match source {
            SocketAddr::V4(source) => {
                socket.set_multicast_if_v4(source.ip())?;
                socket.set_multicast_ttl_v4(2)?;
                if join_group {
                    let SocketAddr::V4(target) = target else {
                        return Err(invalid_input(
                            "multicast target family does not match source",
                        ));
                    };
                    socket.join_multicast_v4(target.ip(), source.ip())?;
                }
            }
            SocketAddr::V6(source) => {
                socket.set_multicast_if_v6(source.scope_id())?;
                socket.set_multicast_hops_v6(2)?;
                if join_group {
                    let SocketAddr::V6(target) = target else {
                        return Err(invalid_input(
                            "multicast target family does not match source",
                        ));
                    };
                    socket.join_multicast_v6(target.ip(), source.scope_id())?;
                }
            }
        }
    }

    socket.bind(&SockAddr::from(address))?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

fn domain_for(address: SocketAddr) -> Domain {
    if address.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    }
}

fn with_port(address: SocketAddr, port: u16) -> SocketAddr {
    match address {
        SocketAddr::V4(address) => SocketAddr::V4(SocketAddrV4::new(*address.ip(), port)),
        SocketAddr::V6(address) => SocketAddr::V6(SocketAddrV6::new(
            *address.ip(),
            port,
            address.flowinfo(),
            address.scope_id(),
        )),
    }
}

fn device_url(address: SocketAddr) -> String {
    let authority = match address {
        SocketAddr::V4(address) => format!("{}:{}", address.ip(), address.port()),
        SocketAddr::V6(address) => {
            let scope = if address.scope_id() == 0 {
                String::new()
            } else {
                format!("%25{}", address.scope_id())
            };
            format!("[{}{scope}]:{}", address.ip(), address.port())
        }
    };
    format!("http://{authority}{HTTP_DEVICE_PATH}")
}

fn host_port(address: SocketAddr) -> String {
    match address {
        SocketAddr::V4(address) => format!("{}:{}", address.ip(), address.port()),
        SocketAddr::V6(address) => {
            let scope = if address.scope_id() == 0 {
                String::new()
            } else {
                format!("%{}", address.scope_id())
            };
            format!("[{}{scope}]:{}", address.ip(), address.port())
        }
    }
}

fn build_device_xml(device: &UpnpDevice) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root xmlns=\"urn:schemas-upnp-org:device-1-0\">\n  <specVersion><major>1</major><minor>0</minor></specVersion>\n  <device>\n",
    );
    push_xml_element(&mut xml, 4, "deviceType", DEVICE_TYPE);
    push_xml_element(&mut xml, 4, "friendlyName", &device.friendly_name);
    push_xml_element(&mut xml, 4, "manufacturer", "Home Assistant");
    push_xml_element(
        &mut xml,
        4,
        "manufacturerURL",
        "https://www.home-assistant.io",
    );
    push_xml_element(&mut xml, 4, "modelDescription", "");
    push_xml_element(&mut xml, 4, "modelName", &device.model_name);
    push_xml_element(&mut xml, 4, "modelNumber", &device.model_number);
    push_xml_element(&mut xml, 4, "serialNumber", &device.serial_number);
    push_xml_element(&mut xml, 4, "UDN", &device.udn);
    push_xml_element(&mut xml, 4, "UPC", "");
    push_xml_element(&mut xml, 4, "presentationURL", &device.presentation_url);

    xml.push_str("    <iconList>\n");
    for icon in &device.icons {
        xml.push_str("      <icon>\n");
        push_xml_element(&mut xml, 8, "mimetype", &icon.mimetype);
        push_xml_element(&mut xml, 8, "width", &icon.width.to_string());
        push_xml_element(&mut xml, 8, "height", &icon.height.to_string());
        push_xml_element(&mut xml, 8, "depth", &icon.depth.to_string());
        push_xml_element(&mut xml, 8, "url", &icon.url);
        xml.push_str("      </icon>\n");
    }
    xml.push_str("    </iconList>\n    <serviceList>\n    </serviceList>\n    <deviceList>\n    </deviceList>\n  </device>\n</root>\n");
    xml
}

fn push_xml_element(xml: &mut String, indent: usize, name: &str, value: &str) {
    let pad = " ".repeat(indent);
    xml.push_str(&pad);
    xml.push('<');
    xml.push_str(name);
    xml.push('>');
    xml.push_str(&escape_xml(value));
    xml.push_str("</");
    xml.push_str(name);
    xml.push_str(">\n");
}

fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn is_xml_text(value: &str) -> bool {
    value.chars().all(|character| {
        matches!(character, '\u{9}' | '\u{A}' | '\u{D}')
            || ('\u{20}'..='\u{D7FF}').contains(&character)
            || ('\u{E000}'..='\u{FFFD}').contains(&character)
            || ('\u{10000}'..='\u{10FFFF}').contains(&character)
    })
}

async fn serve_http(listener: TcpListener, xml: Arc<str>, mut shutdown: watch::Receiver<bool>) {
    let mut connections = JoinSet::new();
    loop {
        if *shutdown.borrow() {
            break;
        }
        if connections.len() >= MAX_HTTP_CONNECTIONS {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = connections.join_next() => {},
            }
            continue;
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let xml = xml.clone();
                    connections.spawn(async move { serve_http_connection(stream, xml).await });
                }
                Err(error) => {
                    tracing::warn!(%error, "UPnP HTTP accept failed");
                    time::sleep(Duration::from_millis(25)).await;
                }
            },
            _ = connections.join_next(), if !connections.is_empty() => {},
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

async fn serve_http_connection(mut stream: TcpStream, xml: Arc<str>) {
    let request = match time::timeout(HTTP_READ_TIMEOUT, read_http_request(&mut stream)).await {
        Ok(Ok(request)) => request,
        _ => return,
    };
    let Some(request_line) = request.split(|byte| *byte == b'\n').next() else {
        return;
    };
    let Ok(request_line) =
        std::str::from_utf8(request_line.strip_suffix(b"\r").unwrap_or(request_line))
    else {
        return;
    };
    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if parts.next().is_some() || !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return;
    }

    let (status, content_type, body): (&str, &str, &[u8]) =
        if matches!(method, "GET" | "HEAD") && path == HTTP_DEVICE_PATH {
            ("200 OK", "text/xml;charset=utf-8", xml.as_bytes())
        } else if path == HTTP_DEVICE_PATH {
            (
                "405 Method Not Allowed",
                "text/plain;charset=utf-8",
                b"Method Not Allowed\n",
            )
        } else {
            ("404 Not Found", "text/plain;charset=utf-8", b"Not Found\n")
        };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type:{content_type}\r\nContent-Length:{}\r\nConnection:close\r\n\r\n",
        body.len()
    );
    if stream.write_all(response.as_bytes()).await.is_ok() && method != "HEAD" {
        let _ = stream.write_all(body).await;
    }
}

const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(3);

async fn read_http_request(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if request.len() >= HTTP_REQUEST_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP request too large",
            ));
        }
        let available = (HTTP_REQUEST_LIMIT - request.len()).min(chunk.len());
        let read = stream.read(&mut chunk[..available]).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete HTTP request",
            ));
        }
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(request);
        }
    }
}

async fn serve_searches(
    announcement_socket: Arc<UdpSocket>,
    response_socket: Arc<UdpSocket>,
    device: Arc<UpnpDevice>,
    device_url: String,
    options: UpnpServerOptions,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tasks = JoinSet::new();
    let mut packet = vec![0u8; SSDP_PACKET_LIMIT];
    loop {
        if *shutdown.borrow() {
            break;
        }
        if tasks.len() >= MAX_SEARCH_TASKS {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = tasks.join_next() => {},
            }
            continue;
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            received = announcement_socket.recv_from(&mut packet) => match received {
                Ok((size, remote)) if size < packet.len() => {
                    let Some(search) = parse_search(&packet[..size], &device.udn) else {
                        continue;
                    };
                    if search.records.is_empty() {
                        continue;
                    }
                    let response_socket = response_socket.clone();
                    let udn = device.udn.clone();
                    let location = device_url.clone();
                    let options = options.clone();
                    tasks.spawn(async move {
                        if !search.delay.is_zero() {
                            time::sleep(search.delay).await;
                        }
                        let date = rfc_date();
                        for (record, response_st) in search.records {
                            let packet = build_search_response(
                                record,
                                &udn,
                                &response_st,
                                &location,
                                &date,
                                &options,
                            );
                            if let Err(error) = response_socket.send_to(packet.as_bytes(), remote).await {
                                tracing::debug!(%error, %remote, "UPnP search response failed");
                            }
                        }
                    });
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "SSDP receive failed");
                    time::sleep(Duration::from_millis(25)).await;
                }
            },
            _ = tasks.join_next(), if !tasks.is_empty() => {},
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

struct SearchPlan {
    records: Vec<(Record, String)>,
    delay: Duration,
}

fn parse_search(packet: &[u8], udn: &str) -> Option<SearchPlan> {
    let text = std::str::from_utf8(packet).ok()?;
    let mut lines = text.split("\r\n");
    if lines.next()? != "M-SEARCH * HTTP/1.1" {
        return None;
    }

    let mut headers = std::collections::HashMap::<String, String>::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        if name.is_empty()
            || name
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return None;
        }
        if value
            .bytes()
            .any(|byte| byte == b'\r' || byte == b'\n' || byte == 0)
        {
            return None;
        }
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    if headers.get("man").map(String::as_str) != Some("\"ssdp:discover\"") {
        return None;
    }
    let requested_st = headers.get("st")?.trim();
    if requested_st.is_empty() || requested_st.bytes().any(|byte| byte.is_ascii_control()) {
        return None;
    }

    let mx = match headers.get("mx") {
        None => Some(None),
        Some(value) => match value.trim().parse::<i64>() {
            Ok(value) if value < 0 => return None,
            Ok(value) => Some(Some(value.min(i64::from(MX_MAX_SECONDS)) as u32)),
            Err(_) if value.trim().starts_with('-') => return None,
            Err(_) => Some(None),
        },
    }?;
    let delay = mx.map_or(Duration::ZERO, random_mx_delay);
    let records = matching_records(requested_st, udn);
    Some(SearchPlan { records, delay })
}

fn random_mx_delay(mx: u32) -> Duration {
    if mx == 0 {
        return Duration::ZERO;
    }
    let min_ms = 100u64;
    let max_ms = u64::from(mx)
        .saturating_mul(1000)
        .saturating_sub(250)
        .max(min_ms);
    let span = max_ms - min_ms + 1;
    // Mix the monotonic call sequence with the current clock for inexpensive
    // per-request jitter without introducing a global RNG dependency.
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let sequence = SEQUENCE.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    let mut entropy = nanos ^ sequence;
    entropy ^= entropy >> 30;
    entropy = entropy.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    entropy ^= entropy >> 27;
    entropy = entropy.wrapping_mul(0x94d0_49bb_1331_11eb);
    entropy ^= entropy >> 31;
    Duration::from_millis(min_ms + entropy % span)
}

fn matching_records(requested_st: &str, udn: &str) -> Vec<(Record, String)> {
    if requested_st.eq_ignore_ascii_case("ssdp:all") {
        return [Record::Root, Record::Udn, Record::DeviceType]
            .into_iter()
            .map(|record| (record, record.nt(udn)))
            .collect();
    }
    if requested_st.eq_ignore_ascii_case(ROOT_DEVICE) {
        return vec![(Record::Root, requested_st.to_owned())];
    }
    if requested_st.eq_ignore_ascii_case(udn) {
        return vec![(Record::Udn, requested_st.to_owned())];
    }
    if upnp_type_match(requested_st, DEVICE_TYPE) {
        return vec![(Record::DeviceType, requested_st.to_owned())];
    }
    Vec::new()
}

fn upnp_type_match(requested: &str, advertised: &str) -> bool {
    let Some((requested_base, requested_version)) = requested.rsplit_once(':') else {
        return false;
    };
    let Some((advertised_base, advertised_version)) = advertised.rsplit_once(':') else {
        return false;
    };
    if !requested_base.eq_ignore_ascii_case(advertised_base) {
        return false;
    }
    let (Ok(requested_version), Ok(advertised_version)) = (
        requested_version.parse::<u32>(),
        advertised_version.parse::<u32>(),
    ) else {
        return false;
    };
    requested_version <= advertised_version
}

fn build_search_response(
    record: Record,
    udn: &str,
    response_st: &str,
    location: &str,
    date: &str,
    options: &UpnpServerOptions,
) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nCACHE-CONTROL:max-age={}\r\nDATE:{}\r\nSERVER:{}\r\nST:{}\r\nUSN:{}\r\nEXT:\r\nLOCATION:{}\r\nBOOTID.UPNP.ORG:{}\r\nCONFIGID.UPNP.ORG:{}\r\n\r\n",
        options.cache_max_age,
        date,
        server_header(),
        response_st,
        record.usn(udn),
        location,
        options.boot_id,
        options.config_id,
    )
}

fn build_notify(
    record: Record,
    udn: &str,
    nts: &str,
    location: &str,
    target: SocketAddr,
    options: &UpnpServerOptions,
) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\nHOST:{}\r\nCACHE-CONTROL:max-age={}\r\nDATE:{}\r\nSERVER:{}\r\nNT:{}\r\nNTS:{}\r\nUSN:{}\r\nEXT:\r\nLOCATION:{}\r\nBOOTID.UPNP.ORG:{}\r\nCONFIGID.UPNP.ORG:{}\r\n\r\n",
        host_port(target),
        options.cache_max_age,
        rfc_date(),
        server_header(),
        record.nt(udn),
        nts,
        record.usn(udn),
        location,
        options.boot_id,
        options.config_id,
    )
}

fn server_header() -> String {
    format!(
        "hass-rust/{} UPnP/2.0 Server/1.0",
        env!("CARGO_PKG_VERSION")
    )
}

fn rfc_date() -> String {
    Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

async fn announce_alive(
    socket: Arc<UdpSocket>,
    device: Arc<UpnpDevice>,
    location: String,
    target: SocketAddr,
    options: UpnpServerOptions,
    mut shutdown: watch::Receiver<bool>,
) {
    let period = options.announce_interval.max(Duration::from_millis(1));
    let mut interval = time::interval_at(Instant::now() + period, period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = interval.tick() => {
                let packet = build_notify(
                    Record::Root,
                    &device.udn,
                    "ssdp:alive",
                    &location,
                    target,
                    &options,
                );
                if let Err(error) = socket.send_to(packet.as_bytes(), target).await {
                    tracing::debug!(%error, "periodic UPnP alive notification failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener as TokioTcpListener, UdpSocket as TokioUdpSocket},
    };

    fn sample_device() -> UpnpDevice {
        UpnpDevice {
            udn: "uuid:01234567-89ab-cdef-0123-456789abcdef".to_owned(),
            friendly_name: "Home & <Assistant>".to_owned(),
            model_name: "Home Assistant".to_owned(),
            model_number: "0.1.0".to_owned(),
            serial_number: "stable-instance-1".to_owned(),
            presentation_url: "http://127.0.0.1:8123/".to_owned(),
            icons: vec![DeviceIcon {
                mimetype: "image/png".to_owned(),
                width: 64,
                height: 64,
                depth: 24,
                url: "/static/icons/favicon-192x192.png".to_owned(),
            }],
        }
    }

    fn loopback_options() -> UpnpServerOptions {
        UpnpServerOptions {
            http_port: 0,
            ssdp_port: 0,
            announce_interval: Duration::from_millis(60),
            ..UpnpServerOptions::default()
        }
    }

    fn loopback() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
    }

    fn notify_header(packet: &[u8], name: &str) -> Option<String> {
        let text = std::str::from_utf8(packet).ok()?;
        text.split("\r\n").find_map(|line| {
            let (header, value) = line.split_once(':')?;
            header.eq_ignore_ascii_case(name).then(|| value.to_owned())
        })
    }

    async fn recv_packet(socket: &TokioUdpSocket) -> (Vec<u8>, SocketAddr) {
        let mut bytes = [0u8; SSDP_PACKET_LIMIT];
        let (size, source) = time::timeout(Duration::from_secs(1), socket.recv_from(&mut bytes))
            .await
            .expect("datagram timed out")
            .expect("UDP receive failed");
        (bytes[..size].to_vec(), source)
    }

    async fn start_loopback_server(target: SocketAddr, options: UpnpServerOptions) -> UpnpServer {
        UpnpServer::start(sample_device(), loopback(), target, options)
            .await
            .expect("server start failed")
    }

    #[test]
    fn description_is_escaped_and_has_fixed_empty_containers() {
        let device = sample_device();
        let xml = build_device_xml(&device);
        let parsed = crate::parse_description_xml(&xml).expect("description XML should parse");
        assert!(xml.contains("Home &amp; &lt;Assistant&gt;"));
        assert!(xml.contains("<manufacturer>Home Assistant</manufacturer>"));
        assert!(xml.contains("<manufacturerURL>https://www.home-assistant.io</manufacturerURL>"));
        assert!(xml.contains("<UPC></UPC>"));
        assert!(xml.contains("<serviceList>\n    </serviceList>"));
        assert!(xml.contains("<deviceList>\n    </deviceList>"));
        let field_order = [
            "<deviceType>",
            "<friendlyName>",
            "<manufacturer>",
            "<manufacturerURL>",
            "<modelDescription>",
            "<modelName>",
            "<modelNumber>",
            "<serialNumber>",
            "<UDN>",
            "<UPC>",
            "<presentationURL>",
            "<iconList>",
            "<serviceList>",
            "<deviceList>",
        ]
        .map(|field| xml.find(field).expect("required device field is present"));
        assert!(field_order.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(parsed["friendlyName"], "Home & <Assistant>");
        assert_eq!(parsed["modelNumber"], "0.1.0");
        assert_eq!(parsed["serialNumber"], "stable-instance-1");
        assert_eq!(parsed["UDN"], device.udn);
        assert_eq!(parsed["iconList"]["icon"]["width"], "64");
    }

    #[test]
    fn search_matching_keeps_type_spelling_and_checks_versions() {
        let udn = "uuid:01234567-89ab-cdef-0123-456789abcdef";
        let requested = "URN:HOME-ASSISTANT.IO:DEVICE:HOMEASSISTANT:0";
        let records = matching_records(requested, udn);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, Record::DeviceType);
        assert_eq!(records[0].1, requested);
        assert!(matching_records("urn:home-assistant.io:device:HomeAssistant:2", udn).is_empty());
        assert_eq!(matching_records("ssdp:all", udn).len(), 3);
    }

    #[tokio::test]
    async fn serves_http_description_and_releases_bound_port() {
        let advertisements = TokioUdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let server =
            start_loopback_server(advertisements.local_addr().unwrap(), loopback_options()).await;
        let http_addr = server.http_addr();
        let mut stream = TcpStream::connect(http_addr).await.unwrap();
        stream
            .write_all(b"GET /device.xml HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Type:text/xml;charset=utf-8\r\n"));
        assert!(
            response
                .contains("<deviceType>urn:home-assistant.io:device:HomeAssistant:1</deviceType>")
        );
        assert!(response.contains(&format!(
            "<presentationURL>{}</presentationURL>",
            sample_device().presentation_url
        )));
        let (_, get_body) = response.split_once("\r\n\r\n").unwrap();
        let mut head = TcpStream::connect(http_addr).await.unwrap();
        head.write_all(b"HEAD /device.xml?probe=description HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut head_response = Vec::new();
        head.read_to_end(&mut head_response).await.unwrap();
        let head_response = String::from_utf8(head_response).unwrap();
        assert!(head_response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head_response.contains(&format!("Content-Length:{}\r\n", get_body.len())));
        assert_eq!(head_response.split_once("\r\n\r\n").unwrap().1, "");
        let _ = recv_packet(&advertisements).await; // initial alive
        server.shutdown().await.unwrap();
        assert!(TokioTcpListener::bind(http_addr).await.is_ok());
    }

    #[tokio::test]
    async fn search_responds_to_all_three_records_from_ephemeral_port() {
        let advertisements = TokioUdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let server =
            start_loopback_server(advertisements.local_addr().unwrap(), loopback_options()).await;
        let query = TokioUdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let request =
            b"M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: 0\r\nST: ssdp:all\r\n\r\n";
        query.send_to(request, server.ssdp_addr()).await.unwrap();

        let mut records = Vec::new();
        let mut search_types = Vec::new();
        for _ in 0..3 {
            let (response, source) = recv_packet(&query).await;
            let text = String::from_utf8(response).unwrap();
            assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
            assert_eq!(source.ip(), Ipv4Addr::LOCALHOST);
            assert_ne!(source.port(), server.ssdp_addr().port());
            assert!(text.contains("EXT:\r\n"));
            assert!(text.contains("SERVER:hass-rust/"));
            assert!(text.contains("BOOTID.UPNP.ORG:1\r\n"));
            assert!(text.contains("CONFIGID.UPNP.ORG:1\r\n"));
            records.push(notify_header(text.as_bytes(), "USN").unwrap());
            search_types.push(notify_header(text.as_bytes(), "ST").unwrap());
            assert_eq!(
                notify_header(text.as_bytes(), "LOCATION").as_deref(),
                Some(server.device_url())
            );
            let header_names = text
                .split("\r\n")
                .skip(1)
                .take_while(|line| !line.is_empty())
                .map(|line| line.split_once(':').unwrap().0)
                .collect::<Vec<_>>();
            assert_eq!(
                header_names,
                [
                    "CACHE-CONTROL",
                    "DATE",
                    "SERVER",
                    "ST",
                    "USN",
                    "EXT",
                    "LOCATION",
                    "BOOTID.UPNP.ORG",
                    "CONFIGID.UPNP.ORG",
                ]
            );
        }
        assert!(records.iter().any(|usn| usn.ends_with("::upnp:rootdevice")));
        assert!(records.iter().any(|usn| usn == &sample_device().udn));
        assert!(
            records
                .iter()
                .any(|usn| usn.ends_with("::urn:home-assistant.io:device:HomeAssistant:1"))
        );
        assert_eq!(
            search_types,
            ["upnp:rootdevice", sample_device().udn.as_str(), DEVICE_TYPE]
        );

        let requested_type = "URN:HOME-ASSISTANT.IO:DEVICE:HOMEASSISTANT:0";
        let specific = format!(
            "M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: invalid\r\nST: {requested_type}\r\n\r\n"
        );
        query
            .send_to(specific.as_bytes(), server.ssdp_addr())
            .await
            .unwrap();
        let (response, _) = recv_packet(&query).await;
        assert_eq!(
            notify_header(&response, "ST").as_deref(),
            Some(requested_type)
        );
        assert!(
            notify_header(&response, "USN")
                .unwrap()
                .ends_with("::urn:home-assistant.io:device:HomeAssistant:1")
        );

        let _ = recv_packet(&advertisements).await; // initial alive
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wildcard_search_listener_receives_multicast_on_the_loopback_interface() {
        let source: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let group: SocketAddr = "239.255.255.250:1900".parse().unwrap();
        // Every socket explicitly selects 127.0.0.1 as its multicast interface.
        // The group packets stay on loopback; no production interface is used.
        let observer = bind_udp(source, 0, group, true).unwrap();
        assert!(observer.local_addr().unwrap().ip().is_unspecified());
        let port = observer.local_addr().unwrap().port();
        let target = with_port(group, port);
        let observer = TokioUdpSocket::from_std(observer).unwrap();
        let server = UpnpServer::start(
            sample_device(),
            source,
            target,
            UpnpServerOptions {
                ssdp_port: port,
                ..loopback_options()
            },
        )
        .await
        .unwrap();
        let requester =
            TokioUdpSocket::from_std(bind_udp(source, 0, target, false).unwrap()).unwrap();
        requester.send_to(b"M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: 0\r\nST: upnp:rootdevice\r\n\r\n", target).await.unwrap();
        let (packet, peer) = recv_packet(&requester).await;
        assert!(
            String::from_utf8(packet)
                .unwrap()
                .starts_with("HTTP/1.1 200 OK\r\n")
        );
        assert_eq!(peer.ip(), source.ip());
        assert_eq!(server.ssdp_addr(), with_port(source, port));
        server.shutdown().await.unwrap();
        drop(observer);
    }

    #[tokio::test]
    async fn rejects_wrong_man_and_unmatched_type_without_reply() {
        let advertisements = TokioUdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let server =
            start_loopback_server(advertisements.local_addr().unwrap(), loopback_options()).await;
        let query = TokioUdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        for request in [
            b"M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:alive\"\r\nST: ssdp:all\r\n\r\n".as_slice(),
            b"M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nST: urn:unknown:device:Missing:1\r\n\r\n".as_slice(),
            b"m-search * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nST: ssdp:all\r\n\r\n".as_slice(),
        ] {
            query.send_to(request, server.ssdp_addr()).await.unwrap();
            assert!(time::timeout(Duration::from_millis(80), query.recv_from(&mut [0u8; 512]))
                .await
                .is_err());
        }
        let _ = recv_packet(&advertisements).await;
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn negative_mx_is_rejected_and_unparseable_mx_is_immediate() {
        let udn = sample_device().udn;
        let negative = parse_search(
            b"M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: -1\r\nST: ssdp:all\r\n\r\n",
            &udn,
        );
        assert!(negative.is_none());
        let invalid = parse_search(
            b"M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: nope\r\nST: ssdp:all\r\n\r\n",
            &udn,
        )
        .unwrap();
        assert_eq!(invalid.delay, Duration::ZERO);
    }

    #[tokio::test]
    async fn sends_periodic_alive_and_three_byebye_records_then_releases_udp_port() {
        let advertisements = TokioUdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let server =
            start_loopback_server(advertisements.local_addr().unwrap(), loopback_options()).await;
        let ssdp_addr = server.ssdp_addr();
        let first = recv_packet(&advertisements).await.0;
        assert_eq!(notify_header(&first, "NTS").as_deref(), Some("ssdp:alive"));
        let periodic = recv_packet(&advertisements).await.0;
        assert_eq!(
            notify_header(&periodic, "NTS").as_deref(),
            Some("ssdp:alive")
        );

        server.shutdown().await.unwrap();
        let mut byebye = Vec::new();
        while byebye.len() < 3 {
            let packet = recv_packet(&advertisements).await.0;
            if notify_header(&packet, "NTS").as_deref() == Some("ssdp:byebye") {
                byebye.push(notify_header(&packet, "USN").unwrap());
            }
        }
        assert!(byebye.iter().any(|usn| usn.ends_with("::upnp:rootdevice")));
        assert!(byebye.iter().any(|usn| usn == &sample_device().udn));
        assert!(
            byebye
                .iter()
                .any(|usn| usn.ends_with("::urn:home-assistant.io:device:HomeAssistant:1"))
        );
        assert!(TokioUdpSocket::bind(ssdp_addr).await.is_ok());
    }

    #[test]
    fn ipv6_device_url_keeps_scope_as_url_zone_identifier() {
        let address = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 1234, 0, 7));
        assert_eq!(device_url(address), "http://[::1%257]:1234/device.xml");
    }
}
