//! The ops surface from a hosted suite: one raw request, and a metric
//! family read off `/metrics` — the server's own word on what happened,
//! where a client-side clock can only guess (BACKLOG F52).

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// One raw ops-surface request; returns the response text.
pub async fn http(addr: SocketAddr, request: &str) -> String {
    let mut s = TcpStream::connect(addr).await.expect("ops listener");
    s.write_all(request.as_bytes()).await.expect("request");
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.expect("response");
    String::from_utf8_lossy(&buf).into_owned()
}

/// The sum of `family` over every row of room `room` in one `/metrics`
/// scrape — its own row, or each of its shards' (a shard reports under
/// `room << 16 | index`, gsb-core `shard` "Metrics identity"); `None`
/// while the family has no such row yet (before the room's first sample).
pub async fn metric_sum(ops: SocketAddr, family: &str, room: u64) -> Option<f64> {
    let text = http(ops, "GET /metrics HTTP/1.1\r\n\r\n").await;
    let head = format!("{family}{{room=\"r");
    let mut sum = None;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(&head) else {
            continue;
        };
        let (id, value) = rest
            .split_once("\"} ")
            .unwrap_or_else(|| panic!("a row: {line}"));
        let id: u64 = id.parse().unwrap_or_else(|_| panic!("a room id: {line}"));
        if id == room || id >> 16 == room {
            let v: f64 = value
                .parse()
                .unwrap_or_else(|_| panic!("a sample value: {line}"));
            *sum.get_or_insert(0.0) += v;
        }
    }
    sum
}

/// Scrape until `done` holds for room `room`'s sum of `family` (the
/// room samples once per metrics period, so its word arrives within a
/// period or two); panic naming `what` after `within` — a hang guard,
/// not the assertion.
pub async fn until_metric(
    ops: SocketAddr,
    family: &str,
    room: u64,
    within: Duration,
    what: &str,
    done: impl Fn(f64) -> bool,
) -> f64 {
    let deadline = Instant::now() + within;
    loop {
        let v = metric_sum(ops, family, room).await;
        if let Some(v) = v.filter(|&v| done(v)) {
            return v;
        }
        assert!(
            Instant::now() < deadline,
            "timed out: {what} ({family} for r{room}: {v:?})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
