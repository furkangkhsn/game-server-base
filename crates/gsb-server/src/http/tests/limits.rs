//! B49: a peer that sends its head and never reads the response is cut
//! at the write deadline, counted, and its task ends — a slow reader is
//! not; over the connection cap a new connection is closed at once and
//! counted, a slot given back serves the next one, and the counts reach
//! the collector.

use std::net::SocketAddr;
use std::time::Duration;

use gsb_core::metrics::MetricsEvent;
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout};

use super::*;

/// The hang guard: far past every window under test.
const GUARD: Duration = Duration::from_secs(600);
/// Real-clock promptness: far below the 5 s head deadline.
const PROMPT: Duration = Duration::from_secs(2);

#[test]
fn the_limits_resolve_from_the_config() {
    let on = OpsLimits::of(&Config::default());
    assert_eq!(on.max_connections, Some(64));
    assert_eq!(on.write_timeout, Some(Duration::from_secs(10)));
    for (max, secs) in [
        (Some(0), 0.0),
        (None, -1.0),
        (None, f64::INFINITY),
        (None, f64::NAN),
    ] {
        let cfg = Config {
            http_max_connections: max,
            http_write_timeout_secs: secs,
            ..Config::default()
        };
        let off = OpsLimits::of(&cfg);
        assert_eq!(
            (off.max_connections, off.write_timeout),
            (None, None),
            "{max:?} {secs}"
        );
    }
}

/// A peer that sends a head and never reads a response larger than the
/// pipe: cut exactly at the deadline, counted, the task over.
#[tokio::test(start_paused = true)]
async fn a_peer_that_never_reads_is_cut_at_the_write_deadline() {
    let (ops, _) = surface(&Config::default());
    let counters = Arc::clone(&ops.counters);
    let deadline = ops.limits.write_timeout.expect("on by default");
    // 64 bytes of pipe: less than any response's head.
    let (mut peer, ours) = duplex(64);
    let start = Instant::now();
    let task = tokio::spawn(serve_one(ours, ops));
    peer.write_all(b"GET /healthz HTTP/1.1\r\n\r\n")
        .await
        .expect("head written");
    timeout(GUARD, task)
        .await
        .expect("the task ends")
        .expect("no panic");
    let took = start.elapsed();
    assert!(took >= deadline, "cut early: {took:?}");
    assert!(
        took < deadline + Duration::from_millis(5),
        "cut late: {took:?}"
    );
    assert_eq!(counters.totals().ops_http_writes_timed_out, 1);
}

/// A reader slower than the pipe but reading gets the whole response,
/// and nothing is counted.
#[tokio::test(start_paused = true)]
async fn a_slow_reader_gets_the_whole_response() {
    let (ops, _) = surface(&Config::default());
    let counters = Arc::clone(&ops.counters);
    let (mut peer, ours) = duplex(64);
    let task = tokio::spawn(serve_one(ours, ops));
    peer.write_all(b"GET /healthz HTTP/1.1\r\n\r\n")
        .await
        .expect("head written");
    let mut text = Vec::new();
    timeout(GUARD, peer.read_to_end(&mut text))
        .await
        .expect("answered")
        .expect("read to EOF");
    let text = String::from_utf8(text).expect("utf-8");
    assert!(text.starts_with("HTTP/1.1 503 "), "{text}");
    assert!(text.ends_with('\n'), "the whole body: {text}");
    timeout(GUARD, task).await.expect("ends").expect("no panic");
    assert_eq!(counters.totals().ops_http_writes_timed_out, 0);
}

/// Wait until `cond` holds (bounded by [`PROMPT`]).
async fn until(what: &str, cond: impl Fn() -> bool) {
    let end = Instant::now() + PROMPT;
    while !cond() {
        assert!(Instant::now() < end, "never: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn closed_at_once(stream: &mut TcpStream) {
    let mut byte = [0u8; 1];
    let read = timeout(PROMPT, stream.read(&mut byte))
        .await
        .expect("closed at once, not held to the head deadline");
    assert!(
        matches!(read, Ok(0)) || read.is_err(),
        "no answer: {read:?}"
    );
}

#[tokio::test]
async fn over_the_cap_a_connection_is_refused_and_counted() {
    let cfg = Config {
        http_max_connections: Some(2),
        ..Config::default()
    };
    let (ops, _) = surface(&cfg);
    let counters = Arc::clone(&ops.counters);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().unwrap();
    let door = Arc::new(Door::new());
    let (tx, mut rx) = mpsc::channel(16);
    let lp = tokio::spawn(accept_loop(listener, ops, Arc::clone(&door), Some(tx)));

    let first = TcpStream::connect(addr).await.expect("connect");
    let _second = TcpStream::connect(addr).await.expect("connect");
    until("both live", || counters.live() == 2).await;
    let mut third = TcpStream::connect(addr).await.expect("connect");
    closed_at_once(&mut third).await;
    assert_eq!(counters.totals().ops_http_conns_refused, 1);

    // The first hangs up: its task ends, its slot serves the next one.
    drop(first);
    until("a slot given back", || counters.live() == 1).await;
    let mut next = TcpStream::connect(addr).await.expect("connect");
    next.write_all(b"GET /healthz HTTP/1.1\r\n\r\n")
        .await
        .expect("write");
    let mut text = Vec::new();
    timeout(PROMPT, next.read_to_end(&mut text))
        .await
        .expect("answered")
        .expect("read");
    assert!(
        text.starts_with(b"HTTP/1.1 503 "),
        "{:?}",
        String::from_utf8_lossy(&text)
    );
    assert_eq!(counters.totals().ops_http_conns_refused, 1);

    // The door's last sample carries the refusal.
    door.close();
    timeout(PROMPT, lp)
        .await
        .expect("the loop ends")
        .expect("no panic");
    let mut refused = 0;
    while let Ok(MetricsEvent::Transport(t)) = rx.try_recv() {
        refused += t.ops_http_conns_refused;
    }
    assert_eq!(refused, 1);
}
