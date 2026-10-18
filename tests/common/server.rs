//! Controlled local HTTP mock server (Phase 2 test infrastructure).
//!
//! Every network scenario the transfer engine will later face is simulated
//! here on an ephemeral loopback port: honored/ignored/broken ranges,
//! truncation, delays, configurable status codes, redirects, omitted
//! `Content-Length`, and changing validators. No live external URL is ever
//! contacted (project rule); the server owns a single accept task and drops
//! every connection on shutdown.
//!
//! Test-support note: this module is compiled into every integration test
//! binary that declares `mod common;`, and each binary only exercises the
//! helpers it needs, so file-level `dead_code` is expected and allowed.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::body::{Body, Frame, SizeHint};
use hyper::header::{
    HeaderValue, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, ETAG, LAST_MODIFIED, LOCATION,
    RANGE,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::fixture::Fixture;

/// How a route's handler treats a client `Range` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeMode {
    /// Correct `206 Partial Content` + `Content-Range` + exact slice.
    Honor,
    /// Ignore the header entirely and return a normal full 200 response.
    Ignore,
    /// Claim 206 with a deliberately mismatched `Content-Range`
    /// (wrong total and shifted offsets) while still sending full bytes.
    BrokenHeaders,
    /// Claim 206 with a correct `Content-Range` but send bytes that do
    /// not belong to the requested range (next-seed fixture content).
    BrokenBody,
}

/// ETag / Last-Modified header behavior for a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Validators {
    /// Never send the header.
    Omit,
    /// Send one fixed value for the lifetime of the route.
    Static,
    /// Send a fresh value on every request (hit counter driven), which
    /// is how a remote file that changed while downloading is simulated.
    Changing,
}

/// HEAD-specific `Content-Length` handling for the Phase 3 metadata
/// scenarios (lying or missing HEAD answers).
///
/// # Limitation
/// An *unparseable* header text cannot be simulated here: hyper refuses to
/// transmit a response whose manually set `Content-Length` is not numeric
/// (verified empirically), so that case is covered by the strict parser
/// unit tests instead (an omitted header behaves identically to an
/// invalid one at the probe level).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadLength {
    /// Advertise this exact value as a manually set header, whether or not
    /// it is the real length (hyper transmits it verbatim, including `0` —
    /// an exact zero size hint would be omitted from the message).
    Value(u64),
    /// Omit the header entirely (no exact size hint on the HEAD response).
    Omit,
}

/// The canned behavior of one route on the mock server.
#[derive(Clone, Debug)]
pub struct Behavior {
    pub fixture: Fixture,
    /// HTTP status of the primary response; values other than 200 answer
    /// with a deterministic sentinel body (`status-<code>`) and never with
    /// fixture bytes.
    pub status: u16,
    pub range: RangeMode,
    /// Stream the body without an exact size (chunked, no `Content-Length`).
    pub omit_content_length: bool,
    /// Serve only this many body bytes and then abort the connection
    /// before the advertised transfer is complete.
    pub truncate_after: Option<u64>,
    /// Sleep before response headers are transmitted.
    pub delay_headers_ms: u64,
    /// Sleep before each streamed body frame.
    pub delay_body_ms: u64,
    /// Respond with this status + `Location` instead of normal handling.
    pub redirect: Option<(u16, String)>,
    pub etag: Validators,
    pub last_modified: Validators,
    /// Answer HEAD with this status instead of the metadata response
    /// (simulates HEAD-unsupported servers: 405, 501, broken handlers).
    pub head_status: Option<u16>,
    /// HEAD `Content-Length` override for HEAD-metadata scenarios.
    pub head_content_length: Option<HeadLength>,
    /// Answer GET with this status instead of normal handling, while HEAD
    /// keeps succeeding (a ranged verification request may fail elsewhere).
    pub get_status: Option<u16>,
    /// Attach this `Content-Encoding` to 200/206/HEAD responses, which is
    /// how a content transformation that invalidates byte offsets is
    /// simulated.
    pub content_encoding: Option<String>,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            fixture: Fixture::default(),
            status: 200,
            range: RangeMode::Honor,
            omit_content_length: false,
            truncate_after: None,
            delay_headers_ms: 0,
            delay_body_ms: 0,
            redirect: None,
            etag: Validators::Omit,
            last_modified: Validators::Omit,
            head_status: None,
            head_content_length: None,
            get_status: None,
            content_encoding: None,
        }
    }
}

impl Behavior {
    #[must_use]
    pub fn with_fixture(mut self, fixture: Fixture) -> Self {
        self.fixture = fixture;
        self
    }

    #[must_use]
    pub const fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    #[must_use]
    pub const fn with_range(mut self, range: RangeMode) -> Self {
        self.range = range;
        self
    }

    #[must_use]
    pub const fn with_omit_content_length(mut self, omit: bool) -> Self {
        self.omit_content_length = omit;
        self
    }

    #[must_use]
    pub const fn with_truncate_after(mut self, bytes: u64) -> Self {
        self.truncate_after = Some(bytes);
        self
    }

    #[must_use]
    pub const fn with_delay_headers_ms(mut self, delay: u64) -> Self {
        self.delay_headers_ms = delay;
        self
    }

    #[must_use]
    pub const fn with_delay_body_ms(mut self, delay: u64) -> Self {
        self.delay_body_ms = delay;
        self
    }

    #[must_use]
    pub fn with_redirect(mut self, status: u16, location: String) -> Self {
        self.redirect = Some((status, location));
        self
    }

    #[must_use]
    pub const fn with_etag(mut self, mode: Validators) -> Self {
        self.etag = mode;
        self
    }

    #[must_use]
    pub const fn with_last_modified(mut self, mode: Validators) -> Self {
        self.last_modified = mode;
        self
    }

    #[must_use]
    pub const fn with_head_status(mut self, status: u16) -> Self {
        self.head_status = Some(status);
        self
    }

    #[must_use]
    pub fn with_head_length(mut self, length: u64) -> Self {
        self.head_content_length = Some(HeadLength::Value(length));
        self
    }

    #[must_use]
    pub fn with_head_length_omitted(mut self) -> Self {
        self.head_content_length = Some(HeadLength::Omit);
        self
    }

    #[must_use]
    pub const fn with_get_status(mut self, status: u16) -> Self {
        self.get_status = Some(status);
        self
    }

    #[must_use]
    pub fn with_content_encoding(mut self, encoding: impl Into<String>) -> Self {
        self.content_encoding = Some(encoding.into());
        self
    }
}

/// One request as observed by the mock server, in arrival order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedRequest {
    pub method: String,
    /// Path (including any query string) exactly as the client sent it.
    pub path: String,
    /// Raw `Range` header value, if the client sent one.
    pub range: Option<String>,
    /// All request headers as received (names lowercased; duplicate names
    /// comma-joined). Phase 3 uses this to assert the probe's
    /// `Accept-Encoding: identity` byte-offset discipline.
    pub headers: BTreeMap<String, String>,
}

struct Shared {
    routes: Mutex<HashMap<String, Behavior>>,
    hits: Mutex<HashMap<String, u64>>,
    log: Mutex<Vec<RecordedRequest>>,
    shutdown: watch::Sender<bool>,
}

/// A mock HTTP server bound to an ephemeral loopback port.
///
/// Routes are mounted with [`MockServer::mount_route`] (before or after
/// start). Dropping the server signals shutdown and aborts the accept task;
/// [`MockServer::shutdown_wait`] additionally waits for the listener to be
/// released so tests can assert the port is free again.
pub struct MockServer {
    shared: Arc<Shared>,
    accept: Mutex<Option<JoinHandle<()>>>,
    port: u16,
}

impl MockServer {
    /// Binds `127.0.0.1:0` and starts the accept loop.
    pub async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("mock server binds an ephemeral loopback port");
        let port = listener
            .local_addr()
            .expect("bound socket reports an address")
            .port();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            routes: Mutex::new(HashMap::new()),
            hits: Mutex::new(HashMap::new()),
            log: Mutex::new(Vec::new()),
            shutdown: shutdown_tx,
        });
        let shared_for_accept = Arc::clone(&shared);
        let accept = tokio::spawn(async move {
            let mut shutdown_rx = shutdown_rx;
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, _addr)) => {
                                spawn_conn(Arc::clone(&shared_for_accept), stream, shutdown_rx.clone());
                            }
                            Err(_) => break,
                        }
                    }
                    _ = shutdown_rx.changed() => break,
                }
            }
            // Dropping the listener is what frees the port for the smoke test.
            drop(listener);
        });
        Self {
            shared,
            accept: Mutex::new(Some(accept)),
            port,
        }
    }

    /// Absolute base URL of the server (`http://127.0.0.1:<port>`).
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The `Host` header value every HTTP/1.1 client must send
    /// (`127.0.0.1:<port>`), for tests that assert captured headers.
    #[must_use]
    pub fn host_header(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Absolute URL for a route path, e.g. `/fixture`.
    ///
    /// # Panics
    /// Panics if `path` does not start with `/`.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        assert!(path.starts_with('/'), "route paths must start with '/'");
        format!("{}{}", self.base_url(), path)
    }

    /// Registers (or replaces) a route's behavior.
    ///
    /// # Panics
    /// Panics if `path` does not start with `/`.
    pub fn mount_route(&self, path: &str, behavior: Behavior) {
        assert!(path.starts_with('/'), "route paths must start with '/'");
        self.shared
            .routes
            .lock()
            .expect("route table lock")
            .insert(path.to_owned(), behavior);
    }

    /// Snapshot of every recorded request, in arrival order.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.shared.log.lock().expect("request log lock").clone()
    }

    /// Number of requests seen for `path` (all methods).
    #[must_use]
    pub fn route_hits(&self, path: &str) -> u64 {
        self.shared
            .hits
            .lock()
            .expect("hit counter lock")
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    /// Signals shutdown and waits for the accept loop to release the port.
    /// Every in-flight connection is dropped.
    pub async fn shutdown_wait(&mut self) {
        let _ = self.shared.shutdown.send(true);
        // Take the handle in its own statement: the MutexGuard temporary
        // must drop before `.await` (clippy::await_holding_lock).
        let handle = self.accept.lock().expect("accept handle lock").take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        let _ = self.shared.shutdown.send(true);
        if let Some(handle) = self.accept.lock().expect("accept handle lock").take() {
            handle.abort();
        }
    }
}

/// Reusable HTTP/1.1 client for the mock server (no redirects, no proxies).
#[must_use]
pub fn http_client() -> Client<HttpConnector, Empty<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http::<Empty<Bytes>>()
}

fn spawn_conn(shared: Arc<Shared>, stream: TcpStream, mut shutdown_rx: watch::Receiver<bool>) {
    tokio::spawn(async move {
        let service = service_fn(move |req| handle(Arc::clone(&shared), req));
        let conn = http1::Builder::new().serve_connection(TokioIo::new(stream), service);
        tokio::pin!(conn);
        tokio::select! {
            outcome = conn.as_mut() => {
                // Abrupt client disconnects / truncated bodies are expected
                // here; nothing to do about either outcome.
                if let Err(_err) = outcome {}
            }
            _ = shutdown_rx.changed() => {
                // Dropping the connection future closes the socket.
            }
        }
    });
}

/// Outcome of parsing a client's `Range` header against a fixture length.
enum RangeParse {
    /// No `Range` header at all.
    Absent,
    /// Malformed header: the server falls back to a full 200 response.
    Malformed,
    /// `start >= total`: unsatisfiable request (RFC 9110 §14.1.1).
    Unsatisfiable,
    /// Range covers the entire resource; a 200 full response is valid.
    FullCover,
    Normal {
        start: u64,
        end: u64,
    }, // end is inclusive and already clamped
}

fn parse_range(header: Option<&HeaderValue>, total: u64) -> RangeParse {
    let Some(raw) = header else {
        return RangeParse::Absent;
    };
    let Ok(raw) = raw.to_str() else {
        return RangeParse::Malformed;
    };
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return RangeParse::Malformed;
    };
    let Some((start_part, end_part)) = spec.split_once('-') else {
        return RangeParse::Malformed;
    };
    let parse_num = |text: &str| -> Option<u64> {
        if text.is_empty() || text.starts_with('+') || text.starts_with(' ') {
            return None;
        }
        text.parse::<u64>().ok()
    };
    // Suffix form "bytes=-N": last N bytes.
    if start_part.is_empty() {
        let Some(suffix) = parse_num(end_part) else {
            return RangeParse::Malformed;
        };
        if suffix == 0 || total == 0 {
            return RangeParse::Malformed;
        }
        let start = total.saturating_sub(suffix);
        if start == 0 {
            // Suffix covers the whole resource: a full 200 response is valid.
            return RangeParse::FullCover;
        }
        return RangeParse::Normal {
            start,
            end: total - 1,
        };
    }
    let Some(start) = parse_num(start_part) else {
        return RangeParse::Malformed;
    };
    let end = match parse_num(end_part) {
        Some(end) => end.min(total.saturating_sub(1)),
        None if end_part.is_empty() => total.saturating_sub(1),
        None => return RangeParse::Malformed,
    };
    if start >= total {
        return RangeParse::Unsatisfiable;
    }
    if start > end {
        return RangeParse::Malformed;
    }
    if start == 0 && end >= total - 1 {
        return RangeParse::FullCover;
    }
    RangeParse::Normal { start, end }
}

const STATIC_LAST_MODIFIED: &str = "Wed, 21 Oct 2015 07:28:00 GMT";
const CHANGING_LAST_MODIFIED: [&str; 2] = [
    "Mon, 02 Jan 2017 15:04:05 GMT",
    "Tue, 03 Jan 2017 15:04:05 GMT",
];

fn etag_value(mode: Validators, fixture: &Fixture, hit: u64) -> Option<HeaderValue> {
    let raw = match mode {
        Validators::Omit => return None,
        Validators::Static => format!("\"fixture-{}-{}\"", fixture.seed, fixture.len),
        Validators::Changing => format!("\"hit-{hit}\""),
    };
    Some(HeaderValue::from_str(&raw).expect("ETag header is a valid header value"))
}

fn last_modified_value(mode: Validators, hit: u64) -> Option<HeaderValue> {
    match mode {
        Validators::Omit => None,
        Validators::Static => Some(HeaderValue::from_static(STATIC_LAST_MODIFIED)),
        Validators::Changing => Some(
            HeaderValue::from_str(
                CHANGING_LAST_MODIFIED[(hit as usize) % CHANGING_LAST_MODIFIED.len()],
            )
            .expect("Last-Modified header is a valid header value"),
        ),
    }
}

fn status_code(code: u16) -> StatusCode {
    StatusCode::from_u16(code).expect("behavior uses a valid HTTP status code")
}

/// Attaches ETag / Last-Modified to a response builder (cloned header values).
fn with_validators(
    builder: hyper::http::response::Builder,
    etag: Option<&HeaderValue>,
    modified: Option<&HeaderValue>,
) -> hyper::http::response::Builder {
    let builder = match etag {
        Some(etag) => builder.header(ETAG, etag.clone()),
        None => builder,
    };
    match modified {
        Some(modified) => builder.header(LAST_MODIFIED, modified.clone()),
        None => builder,
    }
}

/// Attaches the optional `Content-Encoding` to a response builder.
fn with_encoding(
    builder: hyper::http::response::Builder,
    encoding: Option<&String>,
) -> hyper::http::response::Builder {
    match encoding {
        Some(value) => builder.header(
            CONTENT_ENCODING,
            HeaderValue::from_str(value).expect("content-encoding value is a valid header value"),
        ),
        None => builder,
    }
}

/// Body handed to hyper: either exact bytes (Content-Length derived from
/// the size hint) or a frame stream (delayed data, chunked encoding, and /
/// or a mid-body connection drop).
struct TestBody {
    frames: VecDeque<(Option<Duration>, Bytes)>,
    error_after: Option<usize>,
    advertised_len: Option<u64>,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    frames_yielded: usize,
}

impl TestBody {
    /// Full or sliced bytes, streamed in `chunk`-sized frames, each frame
    /// optionally delayed.
    fn stream(
        data: impl Into<Bytes>,
        chunk: usize,
        frame_delay: Option<Duration>,
        advertised_len: Option<u64>,
    ) -> Self {
        let data = data.into();
        let mut frames = VecDeque::new();
        for piece in data.chunks(chunk.max(1)) {
            frames.push_back((frame_delay, Bytes::copy_from_slice(piece)));
        }
        Self {
            frames,
            error_after: None,
            advertised_len,
            sleep: None,
            frames_yielded: 0,
        }
    }

    /// Zero frames but an advertised (full) length: the HEAD pattern, where
    /// hyper suppresses the body but still reports Content-Length.
    fn head_only(advertised_len: u64) -> Self {
        Self {
            frames: VecDeque::new(),
            error_after: None,
            advertised_len: Some(advertised_len),
            sleep: None,
            frames_yielded: 0,
        }
    }

    /// Keeps only the first `keep` bytes of the planned frames and marks
    /// the body so the frame after them returns an error, which aborts the
    /// connection mid-transfer. Callers decide the advertised length
    /// (remaining full length for the Content-Length-liar mode, `None` for
    /// chunked-with-early-error) before or after calling this.
    fn truncate_after(mut self, keep: u64) -> Self {
        let keep = usize::try_from(keep).unwrap_or(usize::MAX);
        let mut taken = 0;
        let mut frames = VecDeque::new();
        while let Some((delay, bytes)) = self.frames.pop_front() {
            if taken >= keep {
                break;
            }
            taken += bytes.len();
            if taken > keep {
                let cut = bytes.len() - (taken - keep);
                frames.push_back((delay, bytes.slice(..cut)));
                break;
            }
            frames.push_back((delay, bytes));
        }
        self.frames = frames;
        self.error_after = Some(self.frames.len());
        self
    }
}

impl Body for TestBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.as_mut().get_mut();
        if let Some(sleep) = this.sleep.as_mut() {
            if sleep.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            this.sleep = None;
        }
        if let Some(limit) = this.error_after {
            if this.frames_yielded >= limit {
                return Poll::Ready(Some(Err(std::io::Error::other(
                    "simulated mid-body connection drop",
                ))));
            }
        }
        match this.frames.pop_front() {
            Some((delay, bytes)) => {
                this.frames_yielded += 1;
                if let Some(delay) = delay {
                    this.sleep = Some(Box::pin(tokio::time::sleep(delay)));
                }
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            None => Poll::Ready(None),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self.advertised_len {
            Some(len) => SizeHint::with_exact(len),
            None => SizeHint::default(),
        }
    }
}

/// Splits the byte payload for streaming when a delay or a missing
/// `Content-Length` is requested. 512-byte frames keep frame counts small.
const STREAM_CHUNK: usize = 512;

fn choose_body(data: &Arc<Vec<u8>>, advertised_len: u64, behavior: &Behavior) -> TestBody {
    let omit_cl = behavior.omit_content_length;
    let delay = (behavior.delay_body_ms > 0).then(|| Duration::from_millis(behavior.delay_body_ms));
    let mut body = TestBody::stream(
        Bytes::from(data.as_ref().clone()),
        STREAM_CHUNK,
        delay,
        if omit_cl { None } else { Some(advertised_len) },
    );
    if let Some(keep) = behavior.truncate_after {
        body = body.truncate_after(keep);
    }
    body
}

async fn handle(
    shared: Arc<Shared>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<TestBody>, Infallible> {
    let method = req.method().clone();
    let route_key = req.uri().path().to_owned();
    let recorded_path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| route_key.clone());
    let range_header = req.headers().get(RANGE).cloned();

    let mut captured_headers: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in req.headers() {
        let entry = captured_headers
            .entry(name.as_str().to_owned())
            .or_default();
        if !entry.is_empty() {
            entry.push_str(", ");
        }
        entry.push_str(value.to_str().unwrap_or(""));
    }

    shared
        .log
        .lock()
        .expect("request log lock")
        .push(RecordedRequest {
            method: method.as_str().to_owned(),
            path: recorded_path,
            range: range_header
                .as_ref()
                .map(|value| value.to_str().unwrap_or("").to_owned()),
            headers: captured_headers,
        });

    let behavior = shared
        .routes
        .lock()
        .expect("route table lock")
        .get(&route_key)
        .cloned()
        .unwrap_or_else(|| Behavior {
            // Unmounted routes are 404 sentinel responses, never fixture bytes.
            status: 404,
            ..Behavior::default()
        });

    if behavior.delay_headers_ms > 0 {
        tokio::time::sleep(Duration::from_millis(behavior.delay_headers_ms)).await;
    }

    if let Some((code, location)) = behavior.redirect {
        // Redirect responses count as visits to this route too, so tests
        // can assert the hops a probe actually took.
        {
            let mut counters = shared.hits.lock().expect("hit counter lock");
            *counters.entry(route_key.clone()).or_insert(0) += 1;
        }
        let response = Response::builder()
            .status(status_code(code))
            .header(LOCATION, location)
            .body(TestBody::head_only(0))
            .expect("redirect response builds");
        return Ok(response);
    }

    if behavior.status != 200 {
        let sentinel = format!("status-{}", behavior.status);
        let len = sentinel.len() as u64;
        let response = Response::builder()
            .status(status_code(behavior.status))
            .body(TestBody::stream(sentinel, STREAM_CHUNK, None, Some(len)))
            .expect("status response builds");
        return Ok(response);
    }

    let hit = {
        let mut counters = shared.hits.lock().expect("hit counter lock");
        let counter = counters.entry(route_key.clone()).or_insert(0);
        let current = *counter;
        *counter += 1;
        current
    };

    // Validators apply to HEAD (probing) and normal 200/206 responses alike.
    let etag_header = etag_value(behavior.etag, &behavior.fixture, hit);
    let modified_header = last_modified_value(behavior.last_modified, hit);

    let fixture = &behavior.fixture;
    if method == Method::HEAD {
        if let Some(code) = behavior.head_status {
            return Ok(Response::builder()
                .status(status_code(code))
                .body(TestBody::head_only(0))
                .expect("HEAD status-override response builds"));
        }
        let builder = with_encoding(
            with_validators(
                Response::builder().status(StatusCode::OK),
                etag_header.as_ref(),
                modified_header.as_ref(),
            ),
            behavior.content_encoding.as_ref(),
        );
        return Ok(match &behavior.head_content_length {
            Some(HeadLength::Value(length)) => {
                // A manually set header: hyper transmits it verbatim,
                // including `0` (an exact size hint of 0 would be omitted).
                let text = length.to_string();
                builder
                    .header(
                        CONTENT_LENGTH,
                        HeaderValue::from_str(&text)
                            .expect("a numeric length is a valid header value"),
                    )
                    .body(TestBody::stream(Bytes::new(), STREAM_CHUNK, None, None))
                    .expect("HEAD length-override response builds")
            }
            Some(HeadLength::Omit) => builder
                // No exact size hint: hyper omits Content-Length on HEAD.
                .body(TestBody::stream(Bytes::new(), STREAM_CHUNK, None, None))
                .expect("HEAD length-omitted response builds"),
            None => builder
                .body(TestBody::head_only(fixture.len))
                .expect("HEAD response builds"),
        });
    }

    if let Some(code) = behavior.get_status {
        let sentinel = format!("status-{code}");
        let len = sentinel.len() as u64;
        return Ok(Response::builder()
            .status(status_code(code))
            .body(TestBody::stream(sentinel, STREAM_CHUNK, None, Some(len)))
            .expect("GET status-override response builds"));
    }
    let (range_status, content_range, data, advertised): (u16, Option<String>, Arc<Vec<u8>>, u64) =
        match parse_range(range_header.as_ref(), fixture.len) {
            RangeParse::Absent | RangeParse::Malformed => (200, None, fixture.bytes(), fixture.len),
            RangeParse::FullCover => (200, None, fixture.bytes(), fixture.len),
            RangeParse::Unsatisfiable => {
                let response = Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(
                        CONTENT_RANGE,
                        HeaderValue::from_str(&format!("bytes */{}", fixture.len))
                            .expect("valid Content-Range"),
                    )
                    .body(TestBody::head_only(0))
                    .expect("416 response builds");
                return Ok(response);
            }
            RangeParse::Normal { start, end } => match behavior.range {
                RangeMode::Ignore => (200, None, fixture.bytes(), fixture.len),
                RangeMode::Honor => (
                    206,
                    Some(format!("bytes {start}-{end}/{}", fixture.len)),
                    fixture.slice(start, end),
                    end - start + 1,
                ),
                RangeMode::BrokenHeaders => (
                    206,
                    Some(format!(
                        "bytes {}-{}/{}",
                        start + 1,
                        end + 1,
                        fixture.len + 7
                    )),
                    fixture.bytes(),
                    fixture.len,
                ),
                RangeMode::BrokenBody => (
                    206,
                    Some(format!("bytes {start}-{end}/{}", fixture.len)),
                    Fixture::new(fixture.seed + 1, fixture.len).slice(start, end),
                    end - start + 1,
                ),
            },
        };

    let mut response_builder = with_encoding(
        with_validators(
            Response::builder().status(status_code(range_status)),
            etag_header.as_ref(),
            modified_header.as_ref(),
        ),
        behavior.content_encoding.as_ref(),
    );
    if let Some(header) = content_range {
        response_builder = response_builder.header(
            CONTENT_RANGE,
            HeaderValue::from_str(&header).expect("valid Content-Range value"),
        );
    }
    let response = response_builder
        .body(choose_body(&data, advertised, &behavior))
        .expect("mock response builds");
    Ok(response)
}
