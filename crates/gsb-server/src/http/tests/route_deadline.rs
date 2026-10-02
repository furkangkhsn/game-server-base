//! B90: the routing step — `/rooms` and the room open/close waiting for
//! the registry's answer — runs under one deadline. A registry that
//! never answers (or whose mailbox is full) gets the request a `504` at
//! the deadline, counted; one that answers in time is served as before;
//! with the deadline off, the wait is the registry's.

use std::collections::BTreeSet;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, timeout};

use gsb_core::channel::{Inbox, channel};
use gsb_core::metrics::MetricReport;
use gsb_core::registry::{RegistryMsg, RoomStatus};

use super::*;

/// The hang guard: far past every window under test.
const GUARD: Duration = Duration::from_secs(600);

/// The surface over `cfg` with a registry stand-in nobody reads: every
/// request reaches its mailbox (`capacity` slots) and none is answered.
/// The bookkeeper knows room 1. Returns the stand-in's inbox — kept, so
/// the mailbox stays open.
fn silent(cfg: &Config, capacity: usize) -> (OpsHttp, Inbox<RegistryMsg>) {
    let (registry, inbox) = channel::<RegistryMsg>(capacity);
    let (rooms, rooms_rx) = mpsc::channel(8);
    tokio::spawn(run_room_bookkeeper(rooms_rx, BTreeSet::from([1])));
    let (_, reports) = watch::channel(MetricReport::initial_stale(Duration::from_secs(1)));
    let ops = OpsHttp {
        reports,
        registry,
        rooms,
        period: Duration::from_secs(1),
        room_template: cfg.room_template(),
        limits: OpsLimits::of(cfg),
        counters: Default::default(),
    };
    (ops, inbox)
}

/// Serve `head` over a pipe; the status code and when the answer came.
async fn serve(ops: OpsHttp, head: &str) -> (u16, Duration) {
    let (mut peer, ours) = duplex(4096);
    let start = Instant::now();
    let task = tokio::spawn(serve_one(ours, ops));
    peer.write_all(format!("{head}\r\n\r\n").as_bytes())
        .await
        .expect("head written");
    let mut text = Vec::new();
    timeout(GUARD, peer.read_to_end(&mut text))
        .await
        .expect("answered before the hang guard")
        .expect("read to EOF");
    let took = start.elapsed();
    timeout(GUARD, task).await.expect("ends").expect("no panic");
    let text = String::from_utf8(text).expect("utf-8");
    let status = text
        .strip_prefix("HTTP/1.1 ")
        .and_then(|t| t.get(..3))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("a status line: {text}"));
    (status, took)
}

#[test]
fn the_deadline_resolves_from_the_config() {
    let on = OpsLimits::of(&Config::default());
    assert_eq!(on.route_timeout, Some(Duration::from_secs(10)));
    for secs in [0.0, -1.0, f64::INFINITY, f64::NAN] {
        let cfg = Config {
            http_route_timeout_secs: secs,
            ..Config::default()
        };
        assert_eq!(OpsLimits::of(&cfg).route_timeout, None, "{secs}");
    }
}

/// Every route that waits for the registry is cut exactly at the
/// deadline with a 504, and each one counted.
#[tokio::test(start_paused = true)]
async fn a_registry_that_never_answers_gets_a_504_at_the_deadline() {
    let (ops, _inbox) = silent(&Config::default(), 8);
    let counters = Arc::clone(&ops.counters);
    let deadline = ops.limits.route_timeout.expect("on by default");
    let heads = [
        "GET /rooms HTTP/1.1",
        "POST /rooms/open?id=5 HTTP/1.1",
        "POST /rooms/close?id=5 HTTP/1.1",
    ];
    for (n, head) in heads.into_iter().enumerate() {
        let (status, took) = serve(ops.clone(), head).await;
        assert_eq!(status, 504, "{head}");
        assert!(took >= deadline, "{head}: cut early: {took:?}");
        assert!(
            took < deadline + Duration::from_millis(5),
            "{head}: cut late: {took:?}"
        );
        assert_eq!(counters.totals().ops_http_routes_timed_out, n as u64 + 1);
    }
    // A route that does not wait is never cut.
    let (status, _) = serve(ops, "GET /healthz HTTP/1.1").await;
    assert_eq!(status, 503, "the stale report's answer");
    assert_eq!(counters.totals().ops_http_routes_timed_out, 3);
}

/// A full registry mailbox: the request cannot even be queued — the
/// same deadline covers the wait for a slot.
#[tokio::test(start_paused = true)]
async fn a_full_registry_mailbox_is_cut_at_the_deadline_too() {
    let (ops, _inbox) = silent(&Config::default(), 1);
    let (filler, _) = tokio::sync::oneshot::channel();
    ops.registry
        .try_send(RegistryMsg::RoomStatus {
            id: gsb_core::id::RoomId(9),
            reply: filler,
        })
        .expect("the one slot");
    let counters = Arc::clone(&ops.counters);
    let (status, took) = serve(ops, "POST /rooms/open?id=5 HTTP/1.1").await;
    assert_eq!(status, 504);
    assert!(took >= Duration::from_secs(10), "{took:?}");
    assert_eq!(counters.totals().ops_http_routes_timed_out, 1);
}

/// A registry that answers just inside the deadline is served as before;
/// with the deadline off, even a minute's wait is the registry's.
#[tokio::test(start_paused = true)]
async fn an_answer_in_time_is_served_and_none_is_cut_without_a_deadline() {
    for (secs, delay) in [(10.0, 9_990), (0.0, 60_000)] {
        let cfg = Config {
            http_route_timeout_secs: secs,
            ..Config::default()
        };
        let (ops, mut inbox) = silent(&cfg, 8);
        tokio::spawn(async move {
            while let Some(msg) = inbox.recv().await {
                if let RegistryMsg::DestroyRoom { reply, .. } = msg {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let _ = reply.send(RoomStatus::Destroyed);
                }
            }
        });
        let counters = Arc::clone(&ops.counters);
        let (status, took) = serve(ops, "POST /rooms/close?id=5 HTTP/1.1").await;
        assert_eq!(status, 200, "{secs} s, answered after {delay} ms");
        assert!(took >= Duration::from_millis(delay), "{took:?}");
        assert_eq!(counters.totals().ops_http_routes_timed_out, 0);
    }
}
