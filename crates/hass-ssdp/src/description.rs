//! UPnP device description fetching, caching, and XML conversion.

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

use serde_json::{Map, Value};
use tokio::sync::watch;

/// A parsed UPnP `root.device` description.
pub type Description = Map<String, Value>;

const MAX_XML_NODES: u32 = 100_000;
const MAX_XML_DEPTH: usize = 128;

/// Converts a UPnP description document to its `root.device` object.
///
/// Element names have namespaces stripped, while namespaced attributes use
/// ElementTree's `{namespace}local-name` form. Text is trimmed, repeated child
/// elements become arrays, and all scalar values remain strings. DTDs are
/// disabled to prevent entity expansion.
pub fn parse_description_xml(xml: &str) -> Option<Description> {
    // roxmltree builds nested nodes recursively; reject excessive nesting
    // before parsing so an untrusted device cannot exhaust the caller's stack.
    if !xml_depth_within_limit(xml) {
        return None;
    }

    let document = roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: MAX_XML_NODES,
            ..roxmltree::ParsingOptions::default()
        },
    )
    .map_err(|error| tracing::debug!(%error, "could not parse SSDP description XML"))
    .ok()?;

    let root = document.root_element();
    if root.tag_name().name() != "root" {
        return None;
    }

    let Value::Object(root_object) = xml_element_to_value(root, 1)? else {
        return None;
    };
    match root_object.get("device") {
        Some(Value::Object(device)) => Some(device.clone()),
        _ => None,
    }
}

fn decode_http_text(body: &[u8], content_type: Option<&str>) -> Option<String> {
    let charset = content_type.and_then(charset_parameter).unwrap_or("utf-8");

    if is_ascii_charset(charset) {
        if !body.iter().all(u8::is_ascii) {
            return None;
        }
        return std::str::from_utf8(body).ok().map(str::to_owned);
    }
    if is_latin1_charset(charset) {
        return Some(body.iter().map(|byte| char::from(*byte)).collect());
    }

    let charset = charset.trim();
    let (encoding, body) = if charset.eq_ignore_ascii_case("utf-16") {
        if let Some(body) = body.strip_prefix(b"\xFF\xFE") {
            (UTF_16LE, body)
        } else {
            let body = body.strip_prefix(b"\xFE\xFF")?;
            (UTF_16BE, body)
        }
    } else {
        let encoding = Encoding::for_label(charset.as_bytes()).unwrap_or(UTF_8);
        let body = if encoding == UTF_16LE {
            body.strip_prefix(b"\xFF\xFE").unwrap_or(body)
        } else if encoding == UTF_16BE {
            body.strip_prefix(b"\xFE\xFF").unwrap_or(body)
        } else {
            body
        };
        (encoding, body)
    };

    encoding
        .decode_without_bom_handling_and_without_replacement(body)
        .map(|text| text.into_owned())
}

fn charset_parameter(content_type: &str) -> Option<&str> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("charset") {
            return None;
        }

        let value = value.trim();
        let bytes = value.as_bytes();
        if bytes.len() >= 2
            && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
                || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
        {
            Some(&value[1..value.len() - 1])
        } else {
            Some(value)
        }
    })
}

fn is_ascii_charset(charset: &str) -> bool {
    [
        "ascii",
        "us-ascii",
        "ansi_x3.4-1968",
        "iso-ir-6",
        "iso646-us",
        "us",
        "ibm367",
        "cp367",
        "csascii",
    ]
    .iter()
    .any(|label| charset.trim().eq_ignore_ascii_case(label))
}

fn is_latin1_charset(charset: &str) -> bool {
    [
        "latin1",
        "latin-1",
        "latin_1",
        "l1",
        "iso-8859-1",
        "iso8859-1",
        "iso8859_1",
        "iso_8859-1",
        "iso_8859_1",
        "iso_8859-1:1987",
        "iso-ir-100",
        "ibm819",
        "cp819",
        "csisolatin1",
    ]
    .iter()
    .any(|label| charset.trim().eq_ignore_ascii_case(label))
}

fn xml_depth_within_limit(xml: &str) -> bool {
    let bytes = xml.as_bytes();
    let mut position = 0;
    let mut depth = 0usize;

    while position < bytes.len() {
        if bytes[position] != b'<' {
            position += 1;
            continue;
        }

        if bytes[position..].starts_with(b"<!--") {
            position = find_token(bytes, position + 4, b"-->").unwrap_or(bytes.len());
            position = (position + 3).min(bytes.len());
            continue;
        }
        if bytes[position..].starts_with(b"<![CDATA[") {
            position = find_token(bytes, position + 9, b"]]>").unwrap_or(bytes.len());
            position = (position + 3).min(bytes.len());
            continue;
        }
        if bytes[position..].starts_with(b"<!DOCTYPE") {
            // Defused ElementTree rejects all document type declarations.
            return false;
        }
        if bytes[position..].starts_with(b"<?") {
            position = find_token(bytes, position + 2, b"?>").unwrap_or(bytes.len());
            position = (position + 2).min(bytes.len());
            continue;
        }
        if bytes[position..].starts_with(b"<!") {
            position = find_markup_end(bytes, position + 2).unwrap_or(bytes.len());
            position = (position + 1).min(bytes.len());
            continue;
        }

        let closing = bytes.get(position + 1) == Some(&b'/');
        let tag_start = position + if closing { 2 } else { 1 };
        let Some(end) = find_markup_end(bytes, tag_start) else {
            // The XML parser will return the syntax error.
            return true;
        };
        let self_closing = bytes[tag_start..end]
            .iter()
            .rev()
            .find(|byte| !byte.is_ascii_whitespace())
            == Some(&b'/');

        if closing {
            depth = depth.saturating_sub(1);
        } else if !self_closing {
            depth += 1;
            if depth > MAX_XML_DEPTH {
                return false;
            }
        }
        position = end + 1;
    }

    true
}

fn find_token(bytes: &[u8], from: usize, token: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(token.len())
        .position(|window| window == token)
        .map(|offset| from + offset)
}

fn find_markup_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, byte) in bytes.get(from..)?.iter().copied().enumerate() {
        match (quote, byte) {
            (Some(current), byte) if current == byte => quote = None,
            (Some(_), _) => {}
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return Some(from + offset),
            (None, _) => {}
        }
    }
    None
}

fn xml_element_to_value(node: roxmltree::Node<'_, '_>, depth: usize) -> Option<Value> {
    if depth > MAX_XML_DEPTH {
        return None;
    }

    let element_children: Vec<_> = node.children().filter(|child| child.is_element()).collect();
    let has_children = !element_children.is_empty();
    let mut object = Map::new();

    for child in element_children {
        let value = xml_element_to_value(child, depth + 1)?;
        let name = child.tag_name().name().to_owned();
        match object.entry(name) {
            serde_json::map::Entry::Vacant(entry) => {
                entry.insert(value);
            }
            serde_json::map::Entry::Occupied(mut entry) => match entry.get_mut() {
                Value::Array(values) => values.push(value),
                existing => {
                    let previous = std::mem::replace(existing, Value::Null);
                    *existing = Value::Array(vec![previous, value]);
                }
            },
        }
    }

    let mut has_attributes = false;
    for attribute in node.attributes() {
        has_attributes = true;
        let name = match attribute.namespace() {
            Some(namespace) => format!("{{{namespace}}}{}", attribute.name()),
            None => attribute.name().to_owned(),
        };
        object.insert(
            format!("@{name}"),
            Value::String(attribute.value().to_owned()),
        );
    }

    // ElementTree exposes the first text node as `.text`; comments and
    // processing instructions do not replace it.
    let first_text = node
        .children()
        .find(|child| child.is_text())
        .and_then(|child| child.text());
    match first_text {
        Some(text) => {
            let text = text.trim();
            if !has_children && !has_attributes {
                Some(Value::String(text.to_owned()))
            } else {
                if !text.is_empty() {
                    object.insert("#text".to_owned(), Value::String(text.to_owned()));
                }
                Some(Value::Object(object))
            }
        }
        None if !has_children && !has_attributes => Some(Value::Null),
        None => Some(Value::Object(object)),
    }
}

enum CacheSlot {
    Fetching(Arc<PendingFetch>),
    Ready(Option<Arc<Description>>),
}

struct PendingFetch {
    finished: watch::Sender<bool>,
}

struct CacheInner {
    entries: Mutex<HashMap<String, CacheSlot>>,
}

/// An asynchronous cache of UPnP device descriptions.
///
/// Each exact location string has at most one in-flight request. Both parsed
/// descriptions and missing/invalid responses are cached until [`uncache`].
pub struct DescriptionCache {
    client: reqwest::Client,
    inner: Arc<CacheInner>,
}

impl DescriptionCache {
    /// Creates a description cache using the supplied HTTP client.
    ///
    /// The client controls TLS validation and request timeouts. Home
    /// Assistant's SSDP adapter can therefore supply its configured discovery
    /// client, while callers can use a short timeout in loopback tests.
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            inner: Arc::new(CacheInner {
                entries: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Returns whether the cache contains a completed result and that result.
    ///
    /// A missing location is treated as a completed absence. In-flight
    /// requests and uncached locations return `(false, None)`.
    pub fn peek(&self, location: Option<&str>) -> (bool, Option<Arc<Description>>) {
        let Some(location) = location else {
            return (true, None);
        };

        match lock_entries(&self.inner).get(location) {
            Some(CacheSlot::Ready(description)) => (true, description.clone()),
            Some(CacheSlot::Fetching(_)) | None => (false, None),
        }
    }

    /// Gets a description from cache or fetches and parses it.
    ///
    /// Only HTTP 200 responses are parsed. Fetch, status, empty-body, and XML
    /// errors return `None` and are negatively cached. Cancelling the request
    /// leader removes its in-flight entry and wakes waiters without caching a
    /// failure, so a later call can retry.
    pub async fn get(&self, location: Option<&str>) -> Option<Arc<Description>> {
        let location = location?;

        enum Lookup {
            Cached(Option<Arc<Description>>),
            Wait(Arc<PendingFetch>, watch::Receiver<bool>),
            Fetch(Arc<PendingFetch>),
        }

        let lookup = {
            let mut entries = lock_entries(&self.inner);
            match entries.get(location) {
                Some(CacheSlot::Ready(description)) => Lookup::Cached(description.clone()),
                Some(CacheSlot::Fetching(pending)) => {
                    Lookup::Wait(pending.clone(), pending.finished.subscribe())
                }
                None => {
                    let (finished, _) = watch::channel(false);
                    let pending = Arc::new(PendingFetch { finished });
                    entries.insert(location.to_owned(), CacheSlot::Fetching(pending.clone()));
                    Lookup::Fetch(pending)
                }
            }
        };

        match lookup {
            Lookup::Cached(description) => description,
            Lookup::Wait(pending, mut finished) => {
                while !*finished.borrow() {
                    if finished.changed().await.is_err() {
                        break;
                    }
                }

                // Match the source cache: waiters inspect the current map after
                // the event, so an intervening uncache/replacement is visible.
                let _keep_pending_alive = pending;
                match lock_entries(&self.inner).get(location) {
                    Some(CacheSlot::Ready(description)) => description.clone(),
                    Some(CacheSlot::Fetching(_)) | None => None,
                }
            }
            Lookup::Fetch(pending) => {
                let mut guard = FetchGuard::new(self.inner.clone(), location.to_owned(), pending);
                let description = self.fetch_and_parse(location).await;
                guard.finish(description.clone());
                description
            }
        }
    }

    /// Removes a location from the cache.
    ///
    /// An in-flight request is not cancelled. Its caller still receives its
    /// response, but it will not repopulate the cache after this invalidation.
    pub fn uncache(&self, location: &str) {
        lock_entries(&self.inner).remove(location);
    }

    async fn fetch_and_parse(&self, location: &str) -> Option<Arc<Description>> {
        let response = match self.client.get(location).send().await {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(%error, %location, "failed to fetch SSDP description");
                return None;
            }
        };

        if response.status() != reqwest::StatusCode::OK {
            tracing::debug!(status = %response.status(), %location, "SSDP description request was not HTTP 200");
            return None;
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => {
                tracing::debug!(%error, %location, "failed to read SSDP description body");
                return None;
            }
        };

        let charset = content_type
            .as_deref()
            .and_then(charset_parameter)
            .unwrap_or("utf-8");
        let xml = match decode_http_text(&body, content_type.as_deref()) {
            Some(xml) => xml,
            None => {
                tracing::debug!(charset = %charset, %location, "SSDP description body is invalid for its HTTP charset");
                return None;
            }
        };
        if xml.is_empty() {
            return None;
        }

        parse_description_xml(&xml).map(Arc::new)
    }
}

struct FetchGuard {
    inner: Arc<CacheInner>,
    location: String,
    pending: Arc<PendingFetch>,
    finished: bool,
}

impl FetchGuard {
    fn new(inner: Arc<CacheInner>, location: String, pending: Arc<PendingFetch>) -> Self {
        Self {
            inner,
            location,
            pending,
            finished: false,
        }
    }

    fn finish(&mut self, description: Option<Arc<Description>>) {
        {
            let mut entries = lock_entries(&self.inner);
            if matches!(
                entries.get(&self.location),
                Some(CacheSlot::Fetching(current)) if Arc::ptr_eq(current, &self.pending)
            ) {
                entries.insert(self.location.clone(), CacheSlot::Ready(description));
            }
        }
        self.pending.finished.send_replace(true);
        self.finished = true;
    }
}

impl Drop for FetchGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }

        {
            let mut entries = lock_entries(&self.inner);
            if matches!(
                entries.get(&self.location),
                Some(CacheSlot::Fetching(current)) if Arc::ptr_eq(current, &self.pending)
            ) {
                entries.remove(&self.location);
            }
        }
        self.pending.finished.send_replace(true);
    }
}

fn lock_entries(inner: &CacheInner) -> MutexGuard<'_, HashMap<String, CacheSlot>> {
    inner
        .entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::oneshot,
        task::JoinHandle,
    };

    use super::{DescriptionCache, parse_description_xml};

    const DEVICE_XML: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0" xmlns:vendor="urn:vendor">
  <device vendor:id=" 7 ">
    <deviceType> urn:example:device </deviceType>
    <friendlyName>A &amp; B</friendlyName>
    <iconList>
      <icon><mimetype> image/png </mimetype></icon>
      <icon><mimetype>image/jpeg</mimetype></icon>
    </iconList>
    <deviceList><device><deviceType>urn:example:embedded</deviceType></device></deviceList>
  </device>
</root>"#;

    struct FixtureServer {
        url: String,
        requests: Arc<AtomicUsize>,
        first_request: Option<oneshot::Receiver<()>>,
        release_first: Option<oneshot::Sender<()>>,
        task: JoinHandle<usize>,
    }

    impl FixtureServer {
        async fn wait_for_first_request(&mut self) {
            self.first_request.take().unwrap().await.unwrap();
        }

        fn release(&mut self) {
            if let Some(release) = self.release_first.take() {
                let _ = release.send(());
            }
        }
    }

    async fn start_server(
        requests_to_serve: usize,
        status: u16,
        body: &'static str,
        hold_first_response: bool,
    ) -> FixtureServer {
        start_server_with_bytes(
            requests_to_serve,
            status,
            body.as_bytes(),
            hold_first_response,
        )
        .await
    }

    async fn start_server_with_bytes(
        requests_to_serve: usize,
        status: u16,
        body: &'static [u8],
        hold_first_response: bool,
    ) -> FixtureServer {
        start_server_with_content_type(
            requests_to_serve,
            status,
            body.to_vec(),
            "text/xml; charset=utf-8",
            hold_first_response,
        )
        .await
    }
    async fn start_server_with_content_type(
        requests_to_serve: usize,
        status: u16,
        body: Vec<u8>,
        content_type: &'static str,
        hold_first_response: bool,
    ) -> FixtureServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = requests.clone();
        let (first_request_tx, first_request) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let release_rx = hold_first_response.then_some(release_rx);

        let task = tokio::spawn(async move {
            let mut first_request_tx = Some(first_request_tx);
            let mut release_rx = release_rx;
            for request_index in 0..requests_to_serve {
                let (mut stream, _) = listener.accept().await.unwrap();
                server_requests.fetch_add(1, Ordering::SeqCst);
                let _ = stream.read(&mut [0; 2048]).await;
                if let Some(notify) = first_request_tx.take() {
                    let _ = notify.send(());
                }
                if request_index == 0
                    && let Some(release) = release_rx.take()
                {
                    let _ = release.await;
                }

                let reason = match status {
                    200 => "OK",
                    500 => "Internal Server Error",
                    404 => "Not Found",
                    _ => "Fixture Response",
                };
                let headers = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len(),
                );
                let mut response = headers.into_bytes();
                response.extend_from_slice(&body);
                let _ = stream.write_all(&response).await;
                let _ = stream.shutdown().await;
            }
            server_requests.load(Ordering::SeqCst)
        });

        FixtureServer {
            url: format!("http://{address}/description.xml"),
            requests,
            first_request: Some(first_request),
            release_first: hold_first_response.then_some(release_tx),
            task,
        }
    }

    fn cache_with_timeout(timeout: Duration) -> DescriptionCache {
        let client = reqwest::Client::builder().timeout(timeout).build().unwrap();
        DescriptionCache::new(client)
    }

    async fn assert_http_name(content_type: &'static str, body: Vec<u8>, expected: &str) {
        let server = start_server_with_content_type(1, 200, body, content_type, false).await;
        let cache = DescriptionCache::new(reqwest::Client::new());
        let description = cache.get(Some(&server.url)).await.unwrap();

        assert_eq!(description.get("name"), Some(&serde_json::json!(expected)));
        assert_eq!(server.task.await.unwrap(), 1);
    }

    async fn assert_http_invalid(content_type: &'static str, body: Vec<u8>) {
        let server = start_server_with_content_type(1, 200, body, content_type, false).await;
        let cache = DescriptionCache::new(reqwest::Client::new());

        assert!(cache.get(Some(&server.url)).await.is_none());
        assert_eq!(cache.peek(Some(&server.url)), (true, None));
        assert_eq!(server.task.await.unwrap(), 1);
    }

    fn utf16_with_bom(xml: &str, little_endian: bool) -> Vec<u8> {
        let mut body = if little_endian {
            vec![0xFF, 0xFE]
        } else {
            vec![0xFE, 0xFF]
        };
        for unit in xml.encode_utf16() {
            let bytes = if little_endian {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            };
            body.extend_from_slice(&bytes);
        }
        body
    }

    #[test]
    fn xml_conversion_matches_elementtree_shapes_and_namespace_rules() {
        let description = parse_description_xml(DEVICE_XML).unwrap();
        let value = serde_json::Value::Object(description);

        assert_eq!(
            value,
            serde_json::json!({
                "@{urn:vendor}id": " 7 ",
                "deviceType": "urn:example:device",
                "friendlyName": "A & B",
                "iconList": {
                    "icon": [
                        {"mimetype": "image/png"},
                        {"mimetype": "image/jpeg"}
                    ]
                },
                "deviceList": {
                    "device": {"deviceType": "urn:example:embedded"}
                }
            })
        );
        assert!(value.get("deviceList").is_some());
        assert!(value.get("embeddedDevice").is_none());
    }

    #[test]
    fn xml_conversion_preserves_leaf_and_mixed_content_shapes() {
        let description = parse_description_xml(
            r#"<root><device><empty/><space> </space><mixed code="x"> before <!-- <ignored> --><child>inside</child> after</mixed><cdata><![CDATA[<!DOCTYPE root>]]></cdata></device></root>"#,
        )
        .unwrap();

        assert_eq!(description.get("empty"), Some(&serde_json::Value::Null));
        assert_eq!(description.get("space"), Some(&serde_json::json!("")));
        assert_eq!(
            description.get("mixed"),
            Some(&serde_json::json!({"@code":"x", "child":"inside", "#text":"before"}))
        );
        assert_eq!(
            description.get("cdata"),
            Some(&serde_json::json!("<!DOCTYPE root>"))
        );
        assert!(parse_description_xml("<other><device/></other>").is_none());
        assert!(parse_description_xml("<root><device>scalar</device></root>").is_none());
    }

    #[test]
    fn xml_conversion_rejects_dtds_and_deep_input() {
        let dtd = r#"<!DOCTYPE root [<!ENTITY payload "unsafe">]><root><device><name>&payload;</name></device></root>"#;
        assert!(parse_description_xml(dtd).is_none());

        let mut deep = String::from("<root><device>");
        for _ in 0..300 {
            deep.push_str("<nested>");
        }
        deep.push_str("value");
        for _ in 0..300 {
            deep.push_str("</nested>");
        }
        deep.push_str("</device></root>");
        assert!(parse_description_xml(&deep).is_none());
    }

    #[tokio::test]
    async fn non_200_empty_and_malformed_responses_are_negative_cached() {
        for (status, body) in [(500, DEVICE_XML), (200, ""), (200, "<root><device>")] {
            let server = start_server(1, status, body, false).await;
            let cache = DescriptionCache::new(reqwest::Client::new());
            assert!(cache.get(Some(&server.url)).await.is_none());
            assert_eq!(cache.peek(Some(&server.url)), (true, None));
            assert!(cache.get(Some(&server.url)).await.is_none());
            assert_eq!(server.task.await.unwrap(), 1);
        }
    }

    #[tokio::test]
    async fn request_errors_are_negative_cached() {
        let cache = DescriptionCache::new(reqwest::Client::new());
        let location = "not a valid URL";
        assert!(cache.get(Some(location)).await.is_none());
        assert_eq!(cache.peek(Some(location)), (true, None));
        assert!(cache.get(Some(location)).await.is_none());
    }

    #[tokio::test]
    async fn invalid_utf8_response_is_negative_cached() {
        let server = start_server_with_bytes(
            1,
            200,
            b"<root><device><name>invalid: \xff</name></device></root>",
            false,
        )
        .await;
        let cache = DescriptionCache::new(reqwest::Client::new());

        assert!(cache.get(Some(&server.url)).await.is_none());
        assert_eq!(cache.peek(Some(&server.url)), (true, None));
        assert_eq!(server.task.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn http_content_type_charset_decodes_common_encodings() {
        for (content_type, body, expected) in [
            (
                "text/xml; CHARSET=\"Windows-1252\"",
                b"<root><device><name>caf\xe9 \x80</name></device></root>".to_vec(),
                "café €",
            ),
            (
                "text/xml; charset=iso-8859-1",
                b"<root><device><name>\x80</name></device></root>".to_vec(),
                "\u{0080}",
            ),
            (
                "text/xml; charset=us-ascii",
                b"<root><device><name>ascii</name></device></root>".to_vec(),
                "ascii",
            ),
            (
                "text/xml; charset=shift_jis",
                b"<root><device><name>\x82\xa0</name></device></root>".to_vec(),
                "あ",
            ),
            (
                "text/xml; charset=unknown-codec",
                b"<root><device><name>caf\xc3\xa9</name></device></root>".to_vec(),
                "café",
            ),
            (
                "text/xml",
                b"<root><device><name>caf\xc3\xa9</name></device></root>".to_vec(),
                "café",
            ),
        ] {
            assert_http_name(content_type, body, expected).await;
        }
    }

    #[tokio::test]
    async fn utf16_charset_uses_little_and_big_endian_boms() {
        let xml = "<root><device><name>café</name></device></root>";

        for little_endian in [true, false] {
            assert_http_name(
                "text/xml; charset=utf-16",
                utf16_with_bom(xml, little_endian),
                "café",
            )
            .await;
        }
    }

    #[tokio::test]
    async fn ascii_and_unknown_charset_decoding_remain_strict() {
        assert_http_invalid(
            "text/xml; charset=ascii",
            b"<root><device><name>\x80</name></device></root>".to_vec(),
        )
        .await;
        assert_http_invalid(
            "text/xml; charset=unknown-codec",
            b"<root><device><name>\xff</name></device></root>".to_vec(),
        )
        .await;
        assert_http_invalid(
            "text/xml; charset=shift_jis",
            b"<root><device><name>\x82</name></device></root>".to_vec(),
        )
        .await;
        assert_http_invalid(
            "text/xml; charset=utf-16",
            "<root><device><name>missing BOM</name></device></root>"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect(),
        )
        .await;
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_fetch() {
        const CALLERS: usize = 8;
        let mut server = start_server(1, 200, DEVICE_XML, true).await;
        let cache = Arc::new(DescriptionCache::new(reqwest::Client::new()));
        let mut callers = Vec::new();
        let mut started = Vec::new();

        for _ in 0..CALLERS {
            let cache = cache.clone();
            let location = server.url.clone();
            let (started_tx, started_rx) = oneshot::channel();
            started.push(started_rx);
            callers.push(tokio::spawn(async move {
                let _ = started_tx.send(());
                cache.get(Some(&location)).await
            }));
        }
        for caller_started in started {
            caller_started.await.unwrap();
        }
        server.wait_for_first_request().await;
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);

        server.release();
        for caller in callers {
            assert!(caller.await.unwrap().is_some());
        }
        assert_eq!(server.task.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn negative_result_stays_cached_until_uncache() {
        let server = start_server(2, 200, "<malformed>", false).await;
        let cache = DescriptionCache::new(reqwest::Client::new());

        assert!(cache.get(Some(&server.url)).await.is_none());
        assert!(cache.get(Some(&server.url)).await.is_none());
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        cache.uncache(&server.url);
        assert!(cache.get(Some(&server.url)).await.is_none());
        assert_eq!(server.task.await.unwrap(), 2);
    }

    #[tokio::test]
    async fn uncache_during_a_fetch_keeps_the_old_result_out_of_the_new_entry() {
        let mut server = start_server(2, 200, DEVICE_XML, true).await;
        let cache = Arc::new(DescriptionCache::new(reqwest::Client::new()));
        let location = server.url.clone();

        let first_cache = cache.clone();
        let first_location = location.clone();
        let first = tokio::spawn(async move { first_cache.get(Some(&first_location)).await });
        server.wait_for_first_request().await;

        let waiting_cache = cache.clone();
        let waiting_location = location.clone();
        let (waiting_started_tx, waiting_started_rx) = oneshot::channel();
        let waiting = tokio::spawn(async move {
            let _ = waiting_started_tx.send(());
            waiting_cache.get(Some(&waiting_location)).await
        });
        waiting_started_rx.await.unwrap();
        tokio::task::yield_now().await;

        cache.uncache(&location);
        let second_cache = cache.clone();
        let second_location = location.clone();
        let (second_started_tx, second_started_rx) = oneshot::channel();
        let second = tokio::spawn(async move {
            let _ = second_started_tx.send(());
            second_cache.get(Some(&second_location)).await
        });
        second_started_rx.await.unwrap();
        tokio::task::yield_now().await;

        server.release();
        assert!(first.await.unwrap().is_some());
        assert!(waiting.await.unwrap().is_none());
        assert!(second.await.unwrap().is_some());
        assert!(cache.peek(Some(&location)).0);
        assert_eq!(server.task.await.unwrap(), 2);
    }

    #[tokio::test]
    async fn cache_keys_preserve_the_exact_location_string() {
        let server = start_server(2, 200, DEVICE_XML, false).await;
        let cache = DescriptionCache::new(reqwest::Client::new());
        let upper = server.url.replace("description.xml", "Description.xml");
        let lower = server.url.clone();

        assert!(cache.get(Some(&upper)).await.is_some());
        assert!(cache.get(Some(&lower)).await.is_some());
        assert_eq!(server.task.await.unwrap(), 2);
    }

    #[tokio::test]
    async fn timed_out_request_is_cached_without_a_retry() {
        let mut server = start_server(1, 200, DEVICE_XML, true).await;
        let cache = cache_with_timeout(Duration::from_millis(40));

        assert!(cache.get(Some(&server.url)).await.is_none());
        server.wait_for_first_request().await;
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        assert_eq!(cache.peek(Some(&server.url)), (true, None));
        server.release();
        assert_eq!(server.task.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn cancelling_fetch_leader_wakes_waiters_and_allows_a_later_retry() {
        let mut server = start_server(2, 200, DEVICE_XML, true).await;
        let cache = Arc::new(DescriptionCache::new(reqwest::Client::new()));
        let location = server.url.clone();
        let leader_cache = cache.clone();
        let leader = tokio::spawn(async move { leader_cache.get(Some(&location)).await });
        server.wait_for_first_request().await;

        let waiter_cache = cache.clone();
        let waiter_location = server.url.clone();
        let (started_tx, started_rx) = oneshot::channel();
        let waiter = tokio::spawn(async move {
            let _ = started_tx.send(());
            waiter_cache.get(Some(&waiter_location)).await
        });
        started_rx.await.unwrap();
        tokio::task::yield_now().await;

        leader.abort();
        assert!(leader.await.unwrap_err().is_cancelled());
        assert!(
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        assert_eq!(cache.peek(Some(&server.url)), (false, None));

        server.release();
        assert!(cache.get(Some(&server.url)).await.is_some());
        assert_eq!(server.task.await.unwrap(), 2);
    }
}
