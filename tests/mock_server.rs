//! Phase 2 integration tests for the controlled local HTTP mock server
//! (DOCS/phases.md §5: each mock-server mode produces the intended status
//! and headers; interruption and delay scenarios repeat reliably; a smoke
//! test starts and stops the server without leaked tasks or ports).
//!
//! All traffic stays on an ephemeral loopback port; nothing here ever
//! contacts a live external URL (project testing rule). The client is the
//! approved hyper-util legacy HTTP client, used from tests only.

mod common;

use bytes::Bytes;
use common::fixture::{pattern_byte, Fixture};
use common::server::{http_client, Behavior, MockServer, RangeMode, RecordedRequest, Validators};
use http_body_util::{BodyExt, Empty};
use hyper::header::{CONTENT_LENGTH, CONTENT_RANGE, ETAG, LAST_MODIFIED, LOCATION};
use hyper::{Method, Request, Response, StatusCode};
use std::collections::BTreeMap;
use std::time::Duration;

type TestClient = hyper_util::client::legacy::Client<
    hyper_util::client::legacy::connect::HttpConnector,
    Empty<Bytes>,
>;

async fn send(
    client: &TestClient,
    url: &str,
    method: Method,
    range: Option<&str>,
) -> Response<hyper::body::Incoming> {
    let uri: hyper::Uri = url.parse().expect("test URL parses");
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(range) = range {
        builder = builder.header("range", range);
    }
    let request = builder.body(Empty::<Bytes>::new()).expect("request builds");
    client.request(request).await.expect("mock server answers")
}

async fn body_bytes(response: Response<hyper::body::Incoming>) -> Bytes {
    response
        .into_body()
        .collect()
        .await
        .expect("body collects")
        .to_bytes()
}

/// Fallible variant of [`send`]: returns `None` when the connection aborts
/// before the response arrives (Windows may discard buffered response data
/// on the abrupt close, so truncation can surface before the headers).
async fn try_send(
    client: &TestClient,
    url: &str,
    method: Method,
    range: Option<&str>,
) -> Option<Response<hyper::body::Incoming>> {
    let uri: hyper::Uri = url.parse().expect("test URL parses");
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(range) = range {
        builder = builder.header("range", range);
    }
    let request = builder.body(Empty::<Bytes>::new()).expect("request builds");
    client.request(request).await.ok()
}

fn header(response: &Response<hyper::body::Incoming>, name: &hyper::header::HeaderName) -> String {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().expect("header is visible ascii").to_owned())
        .unwrap_or_else(|| panic!("header {name} missing"))
}

fn header_opt(
    response: &Response<hyper::body::Incoming>,
    name: &hyper::header::HeaderName,
) -> Option<String> {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().expect("header is visible ascii").to_owned())
}

async fn one_route(route: &'static str, behavior: Behavior) -> (MockServer, String) {
    let server = MockServer::start().await;
    let url = server.url(route);
    server.mount_route(route, behavior);
    (server, url)
}

#[tokio::test]
async fn smoke_start_stop_full_transfer_and_port_release() {
    let (mut server, url) = one_route("/f", Behavior::default()).await;
    let fixture = Fixture::default();
    let client = http_client();

    let response = send(&client, &url, Method::GET, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, &CONTENT_LENGTH),
        fixture.len.to_string(),
        "normal responses must advertise the exact Content-Length"
    );
    let body = body_bytes(response).await;
    for probe in [0usize, 1, 512, 2_048, 4_095] {
        assert_eq!(body[probe], pattern_byte(fixture.seed, probe as u64));
    }
    assert_eq!(body.as_ref(), fixture.bytes().as_ref());

    server.shutdown_wait().await;
    let port = url
        .parse::<hyper::Uri>()
        .expect("test URL parses")
        .port_u16()
        .expect("URL carries an explicit port");
    let connect = tokio::net::TcpStream::connect(("127.0.0.1", port)).await;
    assert!(connect.is_err(), "port {port} must refuse after shutdown");
}

#[tokio::test]
async fn request_log_captures_method_path_range_in_arrival_order() {
    let (server, url) = one_route("/f", Behavior::default()).await;
    let client = http_client();

    let _ = send(&client, &url, Method::GET, None).await;
    let _ = send(&client, &url, Method::GET, Some("bytes=0-9")).await;
    let _ = send(&client, &url, Method::HEAD, None).await;

    let log = server.requests();
    // Header capture is new in Phase 3. The `Host` entry is the HTTP/1.1
    // transport mandate (not a client choice), so the expectation names it
    // explicitly; unrelated clients stay free to send whatever they need.
    let host = ("host".to_owned(), server.host_header());
    assert_eq!(
        log,
        vec![
            RecordedRequest {
                method: "GET".to_owned(),
                path: "/f".to_owned(),
                range: None,
                headers: BTreeMap::from([host.clone()]),
            },
            RecordedRequest {
                method: "GET".to_owned(),
                path: "/f".to_owned(),
                range: Some("bytes=0-9".to_owned()),
                headers: BTreeMap::from([
                    host.clone(),
                    ("range".to_owned(), "bytes=0-9".to_owned())
                ]),
            },
            RecordedRequest {
                method: "HEAD".to_owned(),
                path: "/f".to_owned(),
                range: None,
                headers: BTreeMap::from([host]),
            },
        ]
    );
    assert_eq!(server.route_hits("/f"), 3);
}

#[tokio::test]
async fn honored_range_answers_206_with_exact_content_range_and_bytes() {
    let (_server, url) = one_route("/f", Behavior::default()).await;
    let fixture = Fixture::default();
    let client = http_client();

    let response = send(&client, &url, Method::GET, Some("bytes=100-199")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        header(&response, &CONTENT_RANGE),
        format!("bytes 100-199/{}", fixture.len)
    );
    assert_eq!(header(&response, &CONTENT_LENGTH), "100");
    let body = body_bytes(response).await;
    for (offset, byte) in body.iter().enumerate() {
        assert_eq!(*byte, fixture.byte_at(100 + offset as u64));
    }
}

#[tokio::test]
async fn full_range_requests_answer_a_normal_200() {
    let (_server, url) = one_route(
        "/f",
        Behavior::default().with_fixture(Fixture::new(3, 8_192)),
    )
    .await;
    let fixture = Fixture::new(3, 8_192);
    let client = http_client();

    for range in ["bytes=0-", "bytes=0-8191", "bytes=0-999999", "bytes=-8192"] {
        let response = send(&client, &url, Method::GET, Some(range)).await;
        assert_eq!(response.status(), StatusCode::OK, "range {range}");
        assert_eq!(header(&response, &CONTENT_LENGTH), fixture.len.to_string());
        let body = body_bytes(response).await;
        assert_eq!(body.as_ref(), fixture.bytes().as_ref());
    }
}

#[tokio::test]
async fn unsatisfiable_range_answers_416_with_bytes_star_total() {
    let (_server, url) = one_route("/f", Behavior::default()).await;
    let fixture = Fixture::default();
    let client = http_client();

    for range in ["bytes=4096-4099", "bytes=5000-"] {
        let response = send(&client, &url, Method::GET, Some(range)).await;
        assert_eq!(
            response.status(),
            StatusCode::RANGE_NOT_SATISFIABLE,
            "range {range}"
        );
        assert_eq!(
            header(&response, &CONTENT_RANGE),
            format!("bytes */{}", fixture.len)
        );
        assert_eq!(body_bytes(response).await.as_ref(), b"");
    }
}

#[tokio::test]
async fn malformed_ranges_fall_back_to_a_full_200_response() {
    let (_server, url) = one_route("/f", Behavior::default()).await;
    let fixture = Fixture::default();
    let client = http_client();

    for range in [
        "units=5-10",
        "bytes=",
        "bytes=banana",
        "bytes=100-50",
        "bytes=-0",
    ] {
        let response = send(&client, &url, Method::GET, Some(range)).await;
        assert_eq!(response.status(), StatusCode::OK, "range {range}");
        assert!(
            header_opt(&response, &CONTENT_RANGE).is_none(),
            "range {range} must not carry a Content-Range"
        );
        let body = body_bytes(response).await;
        assert_eq!(body.as_ref(), fixture.bytes().as_ref());
    }
}

#[tokio::test]
async fn ignore_range_mode_returns_a_full_response() {
    let (_server, url) = one_route("/f", Behavior::default().with_range(RangeMode::Ignore)).await;
    let fixture = Fixture::default();
    let client = http_client();

    let response = send(&client, &url, Method::GET, Some("bytes=10-19")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(header_opt(&response, &CONTENT_RANGE).is_none());
    let body = body_bytes(response).await;
    assert_eq!(body.as_ref(), fixture.bytes().as_ref());
}

#[tokio::test]
async fn broken_range_headers_claim_206_but_metadata_lies() {
    let (_server, url) = one_route(
        "/f",
        Behavior::default().with_range(RangeMode::BrokenHeaders),
    )
    .await;
    let fixture = Fixture::default();
    let client = http_client();

    let response = send(&client, &url, Method::GET, Some("bytes=100-199")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        header(&response, &CONTENT_RANGE),
        format!("bytes {}-{}/{}", 101, 200, fixture.len + 7),
        "the header must mismatch the actual request and total"
    );
    assert_eq!(header(&response, &CONTENT_LENGTH), fixture.len.to_string());
    let body = body_bytes(response).await;
    assert_eq!(body.as_ref(), fixture.bytes().as_ref());
}

#[tokio::test]
async fn broken_body_mode_answers_correct_headers_with_wrong_content() {
    let (_server, url) =
        one_route("/f", Behavior::default().with_range(RangeMode::BrokenBody)).await;
    let fixture = Fixture::default();
    let client = http_client();

    let response = send(&client, &url, Method::GET, Some("bytes=100-199")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        header(&response, &CONTENT_RANGE),
        format!("bytes 100-199/{}", fixture.len),
        "headers must look right, so only the bytes betray the lie"
    );
    assert_eq!(header(&response, &CONTENT_LENGTH), "100");
    let body = body_bytes(response).await;
    let truthful = fixture.slice(100, 199);
    assert_ne!(
        body.as_ref(),
        truthful.as_ref(),
        "body must not be the real range"
    );
    let next_seed = Fixture::new(fixture.seed + 1, fixture.len).slice(100, 199);
    assert_eq!(body.as_ref(), next_seed.as_ref());
}

#[tokio::test]
async fn omit_content_length_sends_all_bytes_with_no_length_header() {
    let (_server, url) = one_route("/f", Behavior::default().with_omit_content_length(true)).await;
    let fixture = Fixture::default();
    let client = http_client();

    let response = send(&client, &url, Method::GET, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get(CONTENT_LENGTH).is_none(),
        "omit_content_length must not send a Content-Length"
    );
    let body = body_bytes(response).await;
    assert_eq!(body.as_ref(), fixture.bytes().as_ref());
}

#[tokio::test]
async fn truncated_transfers_abort_mid_body_repeatedly() {
    let (_server, url) = one_route("/f", Behavior::default().with_truncate_after(1_024)).await;

    for attempt in 0..3 {
        // Fresh client per attempt: a real retry would dial a new connection.
        let client = http_client();
        match try_send(&client, &url, Method::GET, None).await {
            None => {
                // Aborted before the response arrived: the connection died
                // mid-transfer, which is exactly the interruption simulated.
            }
            Some(response) => {
                assert_eq!(response.status(), StatusCode::OK, "attempt {attempt}");
                let collected = response.into_body().collect().await;
                assert!(
                    collected.is_err(),
                    "attempt {attempt}: the connection must end before the body is complete"
                );
            }
        }
    }
}

#[tokio::test]
async fn delayed_responses_honor_configured_bounds_twice() {
    let behavior = Behavior::default()
        .with_delay_headers_ms(100)
        .with_delay_body_ms(40);
    let (_server, url) = one_route("/f", behavior).await;
    let fixture = Fixture::default();

    for run in 0..2 {
        let client = http_client();
        let start = std::time::Instant::now();
        let response = send(&client, &url, Method::GET, None).await;
        let headers_elapsed = start.elapsed();
        assert_eq!(response.status(), StatusCode::OK, "run {run}");
        let body = body_bytes(response).await;
        let total_elapsed = start.elapsed();
        assert_eq!(body.as_ref(), fixture.bytes().as_ref(), "run {run}");
        assert!(
            headers_elapsed >= Duration::from_millis(90),
            "run {run}: headers arrived too early: {headers_elapsed:?}"
        );
        assert!(
            total_elapsed >= Duration::from_millis(340),
            "run {run}: full body arrived too early: {total_elapsed:?}"
        );
        assert!(
            total_elapsed < Duration::from_millis(2_000),
            "run {run}: failed to deliver within bounds: {total_elapsed:?}"
        );
    }
}

#[tokio::test]
async fn static_validators_stay_constant_across_requests() {
    let (_server, url) = one_route(
        "/f",
        Behavior::default()
            .with_etag(Validators::Static)
            .with_last_modified(Validators::Static),
    )
    .await;
    let client = http_client();

    let first = send(&client, &url, Method::GET, None).await;
    let etag_1 = header(&first, &ETAG);
    let modified_1 = header(&first, &LAST_MODIFIED);
    let second = send(&client, &url, Method::GET, None).await;
    let etag_2 = header(&second, &ETAG);
    let modified_2 = header(&second, &LAST_MODIFIED);

    assert!(etag_1.starts_with('"'), "ETag must be quoted: {etag_1}");
    assert!(!etag_1.is_empty());
    assert!(!modified_1.is_empty());
    assert_eq!(etag_1, etag_2, "static ETag must not drift");
    assert_eq!(
        modified_1, modified_2,
        "static Last-Modified must not drift"
    );
}

#[tokio::test]
async fn changing_validators_differ_between_requests() {
    let (_server, url) = one_route(
        "/f",
        Behavior::default()
            .with_etag(Validators::Changing)
            .with_last_modified(Validators::Changing),
    )
    .await;
    let client = http_client();

    let mut etags = Vec::new();
    let mut modifications = Vec::new();
    let mut contents = Vec::new();
    for _ in 0..3 {
        let response = send(&client, &url, Method::GET, None).await;
        etags.push(header(&response, &ETAG));
        modifications.push(header(&response, &LAST_MODIFIED));
        contents.push(body_bytes(response).await.to_vec());
    }

    assert_ne!(etags[0], etags[1], "changing ETag must differ per request");
    assert_ne!(etags[1], etags[2]);
    assert_ne!(etags[0], etags[2]);
    assert_ne!(
        modifications[0], modifications[1],
        "changing Last-Modified must differ"
    );
    assert_ne!(modifications[1], modifications[2]);
    assert_eq!(
        modifications[0], modifications[2],
        "Last-Modified cycles deterministically"
    );
    assert_eq!(
        contents[0], contents[2],
        "content itself stays unchanged while validators change"
    );
}

#[tokio::test]
async fn redirect_routes_answer_with_location_and_no_follower_side_effects() {
    let server = MockServer::start().await;
    let target = server.url("/new-file");
    server.mount_route(
        "/old-file",
        Behavior::default().with_redirect(301, target.clone()),
    );
    server.mount_route("/new-file", Behavior::default());

    let client = http_client();
    let response = send(&client, &server.url("/old-file"), Method::GET, None).await;
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(header(&response, &LOCATION), target);
    assert_eq!(body_bytes(response).await.as_ref(), b"");
    assert_eq!(
        server.route_hits("/new-file"),
        0,
        "the client must not silently follow the redirect"
    );
}

#[tokio::test]
async fn configurable_statuses_answer_with_sentinel_never_fixture_bytes() {
    let server = MockServer::start().await;
    server.mount_route("/gone", Behavior::default().with_status(404));
    server.mount_route("/teapot", Behavior::default().with_status(418));
    let fixture = Fixture::default();
    let client = http_client();

    let response = send(&client, &server.url("/gone"), Method::GET, None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let gone_body = body_bytes(response).await;
    assert_eq!(gone_body.as_ref(), b"status-404");
    assert_ne!(gone_body.as_ref(), &fixture.bytes()[..gone_body.len()]);

    let response = send(&client, &server.url("/teapot"), Method::GET, None).await;
    assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
    let teapot_body = body_bytes(response).await;
    assert_eq!(teapot_body.as_ref(), b"status-418");
    assert_ne!(teapot_body.as_ref(), &fixture.bytes()[..teapot_body.len()]);
}

#[tokio::test]
async fn head_reports_full_content_length_with_an_empty_body() {
    let (_server, url) = one_route("/f", Behavior::default()).await;
    let fixture = Fixture::default();
    let client = http_client();

    let response = send(&client, &url, Method::HEAD, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, &CONTENT_LENGTH), fixture.len.to_string());
    assert_eq!(body_bytes(response).await.as_ref(), b"");
}

#[tokio::test]
async fn unknown_routes_answer_404_sentinel() {
    let server = MockServer::start().await;
    let client = http_client();

    let response = send(&client, &server.url("/never-mounted"), Method::GET, None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_bytes(response).await.as_ref(), b"status-404");
}
