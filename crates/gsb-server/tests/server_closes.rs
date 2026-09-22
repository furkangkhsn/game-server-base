//! The server-close counters, end to end: a real server, a real TCP
//! client, and the `NetReport::server_closes` family read back off the
//! metrics channel.
//!
//! Each test drives ONE server-initiated close path for real and pins
//! the whole vector: the right reason moved by exactly one, and every
//! other reason stayed at zero. A reason that moved under a neighbour's
//! name fails here — which is the point: a capacity measurement reads
//! this family to tell "the server shed its clients for not reading"
//! from "the clients went quiet" from "the clients misbehaved", and a
//! counter that merely says "something closed" cannot answer that.
//!
//! The write-stall path is driven end to end in `write_stall.rs` (it
//! needs the QUIC door's client-controlled window).

use std::time::{Duration, Instant};

use gsb_core::conn::ServerClose;
use gsb_core::metrics::{MetricReport, ServerCloses};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// A single-room TCP server with the given idle window (0 = off), its
/// reports on a channel.
async fn server(
    idle_timeout_secs: f64,
) -> (
    gsb_server::ServerHandle,
    mpsc::UnboundedReceiver<MetricReport>,
) {
    let cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        idle_timeout_secs,
        ..Default::default()
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = gsb_server::start_server_metrics(cfg, tx)
        .await
        .expect("server starts");
    (handle, rx)
}

/// One length-prefixed frame (`[u32 LE len][u16 LE op][payload]`).
fn frame(op: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = ((2 + payload.len()) as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Read until the server closes the socket (EOF or error), with a bound.
async fn until_eof(stream: &mut TcpStream, within: Duration) {
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + within;
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("the server never closed the connection");
        match tokio::time::timeout(left, stream.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return,
            Ok(Ok(_)) => {}
            Err(_) => panic!("the server never closed the connection"),
        }
    }
}

/// Drain reports until the server-close total reaches `want`, then keep
/// reading two more reports so a late SECOND close (under any reason)
/// would still be seen. Returns the final vector.
async fn closes_settled(rx: &mut mpsc::UnboundedReceiver<MetricReport>, want: u64) -> ServerCloses {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = ServerCloses::default();
    while last.total() < want {
        let left = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("server closes never reached {want}: {last:?}"));
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(r)) => last = r.net.server_closes,
            Ok(None) => panic!("metrics channel closed"),
            Err(_) => panic!("server closes never reached {want}: {last:?}"),
        }
    }
    for _ in 0..2 {
        if let Ok(Some(r)) = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
            last = r.net.server_closes;
        }
    }
    last
}

/// Exactly one close, under `reason`, and nothing under any other.
fn assert_only(closes: &ServerCloses, reason: ServerClose) {
    for (r, n) in closes.iter() {
        let want = u64::from(r == reason);
        assert_eq!(
            n,
            want,
            "{}: expected {want} (the close was a {}): {}",
            r.label(),
            reason.label(),
            closes.nonzero_summary()
        );
    }
}

/// A client that connects and says nothing is ended by the reader
/// pump's idle window: booked as `idle_timeout`, nothing else.
#[tokio::test]
async fn a_silent_client_is_booked_as_idle_timeout() {
    let (handle, mut rx) = server(0.5).await;
    let mut stream = TcpStream::connect(handle.addr).await.expect("connect");
    until_eof(&mut stream, Duration::from_secs(10)).await;
    assert_only(&closes_settled(&mut rx, 1).await, ServerClose::IdleTimeout);
    handle.stop().await;
}

/// A client that sends four undefined opcodes (hard violations, weight 4
/// each) exhausts the violation budget: booked as `violation_budget` —
/// not as the idle window (disabled here, so it cannot race), and not
/// as the pre-auth frame budget the same unauthenticated peer is under.
#[tokio::test]
async fn a_violating_client_is_booked_as_violation_budget() {
    let (handle, mut rx) = server(0.0).await;
    let mut stream = TcpStream::connect(handle.addr).await.expect("connect");
    for op in 42..46u16 {
        stream.write_all(&frame(op, &[])).await.expect("write");
    }
    until_eof(&mut stream, Duration::from_secs(10)).await;
    assert_only(
        &closes_settled(&mut rx, 1).await,
        ServerClose::ViolationBudget,
    );
    handle.stop().await;
}

/// The converse, so neither test above can pass on a counter that books
/// every end: a client that connects and then CLOSES its socket itself
/// is not a server close — the family stays at zero in the very report
/// in which the registry has seen the close.
#[tokio::test]
async fn a_client_that_leaves_is_not_a_server_close() {
    let (handle, mut rx) = server(0.0).await;
    let stream = TcpStream::connect(handle.addr).await.expect("connect");
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("the registry never saw the close");
        let r = tokio::time::timeout(left, rx.recv())
            .await
            .expect("the registry never saw the close")
            .expect("metrics channel open");
        if r.registry.is_some_and(|g| g.opens >= 1 && g.conns == 0) {
            assert_eq!(
                r.net.server_closes.total(),
                0,
                "a client-side close was booked as a server close: {}",
                r.net.server_closes.nonzero_summary()
            );
            break;
        }
    }
    handle.stop().await;
}
