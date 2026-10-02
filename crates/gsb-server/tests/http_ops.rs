//! Integration tests for the HTTP ops surface (`docs/OPS.md`): raw
//! `TcpStream` clients against an in-process server started exactly like
//! `e2e.rs` does it — ephemeral game port AND ephemeral ops port
//! (`http_listen = "127.0.0.1:0"`; the bound address comes back on
//! `ServerHandle::http_addr`). The renderer/sink internals are unit-tested
//! in gsb-core; everything here goes over the wire.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use gsb_server::Config;

/// A server with the ops surface on an ephemeral loopback port.
async fn start_with_http() -> gsb_server::ServerHandle {
    let cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        http_listen: "127.0.0.1:0".into(),
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg).await.expect("server starts");
    assert!(
        handle.http_addr.is_some(),
        "a configured http_listen must expose its bound address"
    );
    handle
}

/// One raw request over a fresh TCP connection: write the bytes, read to
/// EOF (`Connection: close` ⇒ the server closes after one response),
/// return (status, full text).
async fn raw_request(addr: SocketAddr, request: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("ops listener accepts");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("request written");
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .await
        .expect("response read to EOF");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status_line = text.lines().next().expect("a status line arrives");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("unparseable status line: {status_line:?}"));
    (status, text)
}

/// Body of a response text (everything after the CRLFCRLF separator).
fn body_of(text: &str) -> &str {
    text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
}

/// Poll an endpoint until a predicate holds (the first REAL metrics report
/// lands about one report period after startup; before that the watch's
/// placeholder answers honestly as stale).
async fn poll_until<F>(addr: SocketAddr, path: &'static str, mut pred: F) -> (u16, String)
where
    F: FnMut(u16, &str) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = raw_request(addr, &format!("GET {path} HTTP/1.1\r\n\r\n")).await;
        if pred(r.0, &r.1) || Instant::now() >= deadline {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Liveness, honest branch: BEFORE the collector's first real emission the
/// watch carries the back-dated placeholder, so `/healthz` must answer 503
/// naming staleness — never ok from a report nobody produced.
///
/// "Before the first report" is proven, not hoped for: the collector's
/// first report is due one report period (1 s) after it was built, which
/// is after `t0`, so an answer read less than a period after `t0` was
/// served before any report existed. A run whose start and probe took a
/// whole period (a starved one) proves nothing either way: it is
/// repeated, and the claim is checked, unchanged, on the first run that
/// is evidence — a conclusive run that answers 200 fails at once
/// (BACKLOG F52; the F30 pattern, not "retry until green").
#[tokio::test]
async fn healthz_answers_503_before_the_first_report() {
    const REPORT_PERIOD: Duration = Duration::from_secs(1);
    for attempt in 1..=10 {
        let t0 = Instant::now();
        let handle = start_with_http().await;
        let addr = handle.http_addr.expect("ops addr");
        let (status, text) = raw_request(addr, "GET /healthz HTTP/1.1\r\n\r\n").await;
        let took = t0.elapsed();
        handle.stop().await;
        if took >= REPORT_PERIOD {
            eprintln!("attempt {attempt}: start and probe took {took:?}, inconclusive");
            continue;
        }
        assert_eq!(status, 503, "pre-first-report healthz must be 503: {text}");
        assert!(
            body_of(&text).contains("stale"),
            "the reason names staleness: {text}"
        );
        return;
    }
    panic!("no conclusive run: every start-and-probe took a whole report period");
}

/// Liveness, ok branch (OPS §4 item 1): while the ticker runs, `/healthz`
/// flips to 200 "ok" once the first real report lands.
#[tokio::test]
async fn healthz_reports_ok_while_ticker_runs() {
    let handle = start_with_http().await;
    let addr = handle.http_addr.expect("ops addr");
    let (status, text) = poll_until(addr, "/healthz", |s, _| s == 200).await;
    assert_eq!(
        status, 200,
        "healthz must turn ok while the ticker runs: {text}"
    );
    assert_eq!(body_of(&text), "ok\n", "the documented body: {text}");
    handle.stop().await;
}

/// Metrics scrape (OPS §4 items 2/5): once a real report exists, GET
/// /metrics serves the Prometheus text exposition — known counter families
/// present, HELP/TYPE header pairs intact, exposition version 0.0.4.
#[tokio::test]
async fn metrics_endpoint_exposes_known_counters() {
    let handle = start_with_http().await;
    let addr = handle.http_addr.expect("ops addr");
    let (status, text) = poll_until(addr, "/metrics", |_, body| {
        body.contains("gsb_registry_rooms")
    })
    .await;
    assert_eq!(status, 200);
    let body = body_of(&text);
    assert!(body.contains("# TYPE"), "TYPE headers present:\n{body}");
    assert!(
        body.contains("# TYPE gsb_registry_rooms gauge"),
        "the registry rooms family is typed:\n{body}"
    );
    assert!(
        text.contains("Content-Type: text/plain; version=0.0.4"),
        "exposition version declared: {text}"
    );
    handle.stop().await;
}

/// Admin lifecycle over plain-text HTTP (OPS §4 item 3): open room 42,
/// see it listed running, close it, see it absent. The listing keeps the
/// seen id after the close ON PURPOSE — it shows the REGISTRY's current
/// status (absent), not the surface's memory.
#[tokio::test]
async fn admin_open_status_close_round_trip() {
    let handle = start_with_http().await;
    let addr = handle.http_addr.expect("ops addr");

    // Open: idempotent-create contract rendered as the resulting status.
    let (status, text) =
        raw_request(addr, "POST /rooms/open?id=42&tick_hz=30 HTTP/1.1\r\n\r\n").await;
    assert_eq!(status, 200, "open succeeds: {text}");
    assert!(
        body_of(&text).contains("r42 running"),
        "the reply renders RoomStatus: {text}"
    );

    // Listed (configured room r1 plus the admin-opened r42).
    let (status, text) = poll_until(addr, "/rooms", |_, body| body.contains("r42 running")).await;
    assert_eq!(status, 200);
    assert!(
        body_of(&text).contains("r1 running"),
        "configured room listed: {text}"
    );

    // Close: the registry's Destroyed answer…
    let (status, text) = raw_request(addr, "POST /rooms/close?id=42 HTTP/1.1\r\n\r\n").await;
    assert_eq!(status, 200, "close succeeds: {text}");
    assert!(body_of(&text).contains("r42 destroyed"), "{text}");

    // …and the listing reflects gone-ness through the registry.
    let (status, text) = poll_until(addr, "/rooms", |_, body| body.contains("r42 absent")).await;
    assert_eq!(status, 200);
    assert!(
        body_of(&text).contains("r42 absent"),
        "closed room reads absent: {text}"
    );

    handle.stop().await;
}

/// Rejection matrix: unknown path 404; wrong verb on a read endpoint 405
/// (with Allow); wrong verb on an admin endpoint 405; unparseable request
/// line 400; missing/non-numeric parameters 400.
#[tokio::test]
async fn unknown_path_wrong_verb_and_bad_params_are_rejected() {
    let handle = start_with_http().await;
    let addr = handle.http_addr.expect("ops addr");

    let (status, _) = raw_request(addr, "GET /nope HTTP/1.1\r\n\r\n").await;
    assert_eq!(status, 404);

    let (status, text) = raw_request(addr, "POST /healthz HTTP/1.1\r\n\r\n").await;
    assert_eq!(status, 405);
    assert!(
        text.contains("Allow: GET"),
        "405 advertises the method: {text}"
    );

    let (status, _) = raw_request(addr, "GET /rooms/open?id=9&tick_hz=30 HTTP/1.1\r\n\r\n").await;
    assert_eq!(status, 405);

    let (status, _) = raw_request(addr, "DELETE /rooms HTTP/1.1\r\n\r\n").await;
    assert_eq!(status, 405);

    let (status, _) = raw_request(addr, "garbage\r\n\r\n").await;
    assert_eq!(status, 400);

    let (status, _) = raw_request(addr, "POST /rooms/open?tick_hz=30 HTTP/1.1\r\n\r\n").await;
    assert_eq!(status, 400, "missing id is a bad request");

    let (status, _) = raw_request(addr, "POST /rooms/close?id=banana HTTP/1.1\r\n\r\n").await;
    assert_eq!(status, 400, "non-numeric id is a bad request");

    handle.stop().await;
}

/// Disabled by default (OPS §4 item 4): the config default is the empty
/// string, and starting a server WITHOUT `http_listen` spawns no surface —
/// observed structurally via `ServerHandle::http_addr == None` (only the
/// enabled wiring sets it, and only after actually binding a listener).
#[tokio::test]
async fn disabled_by_default_and_no_listener_without_config() {
    let cfg = gsb_server::Config::default();
    assert_eq!(cfg.http_listen, "", "http_listen defaults to disabled");

    let handle = gsb_server::start_server(Config {
        bind: "127.0.0.1:0".into(),
        ..Default::default()
    })
    .await
    .expect("server starts");
    assert!(handle.http_addr.is_none(), "no ops listener was spawned");
    handle.stop().await;
}
