//! `max_unauth_conns_per_source` (BACKLOG D12): the per-source cap on
//! unauthenticated connections, end to end on the plain TCP door — the
//! one door with no handshake stage for D11's cap to bound. Omitted, the
//! server is as it was; set, a source at its cap has its next connection
//! refused at birth (`ERROR` code 9, then EOF) and counted under its own
//! reason, another source is served, and a place comes back when a
//! session authenticates or closes — not when its AUTH fails.

use std::net::SocketAddr;
use std::time::Duration;

use gsb_client::conn::{Conn, Recv};
use gsb_client::session::{self, Credentials};
use gsb_core::conn::ServerClose;
use gsb_core::metrics::MetricReport;
use gsb_protocol::base::{self, ErrorCode};
use gsb_protocol::op;
use gsb_server::Config;
use prost::Message;
use tokio::net::TcpSocket;
use tokio::sync::mpsc::UnboundedReceiver;

const PROMPT: Duration = Duration::from_secs(5);

fn parse(text: &str) -> Config {
    toml::from_str(text).expect("the config parses")
}

/// Omitted: no per-source cap — the server every test and load run (all
/// from one loopback address) had.
#[test]
fn omitted_there_is_no_per_source_cap() {
    assert_eq!(parse("").max_unauth_conns_per_source, None);
    assert_eq!(Config::default().max_unauth_conns_per_source, None);
    assert_eq!(
        parse("max_unauth_conns_per_source = 4").max_unauth_conns_per_source,
        Some(4)
    );
}

/// A server-level key: a negative value does not parse, and neither a
/// room override nor a door entry can carry it.
#[test]
fn it_is_a_server_level_key() {
    for text in [
        "max_unauth_conns_per_source = -1",
        "[rooms.1]\nmax_unauth_conns_per_source = 2",
        "[[listeners]]\ntransport = \"tcp\"\nbind = \"127.0.0.1:0\"\nmax_unauth_conns_per_source = 2",
    ] {
        let err = toml::from_str::<Config>(text).expect_err("refused");
        assert!(
            err.to_string().contains("max_unauth_conns_per_source"),
            "{text}: {err}"
        );
    }
}

/// A TCP session from `source` (any 127/8 address is the loopback).
async fn connect_from(source: [u8; 4], to: SocketAddr) -> Conn {
    let socket = TcpSocket::new_v4().expect("socket");
    socket
        .bind(SocketAddr::from((source, 0)))
        .expect("bind the source");
    gsb_client::connect::tcp_stream(socket.connect(to).await.expect("connect"))
}

/// The birth refusal: `ERROR` code 9 for the source's cap, then the end.
async fn refused(mut c: Conn) {
    match c.recv(PROMPT).await.expect("a frame") {
        Recv::Frame(f) if f.op == op::base::ERROR => {
            let e = base::Error::decode(&f.payload[..]).expect("an ERROR");
            assert_eq!(e.code, ErrorCode::ServerClosed as i32, "{e:?}");
            assert!(e.message.contains("per-source"), "{e:?}");
        }
        other => panic!("expected the refusal, got {other:?}"),
    }
    assert!(matches!(c.recv(PROMPT).await, Ok(Recv::Closed) | Err(_)));
}

async fn auth(c: &mut Conn, name: &str) {
    session::auth(c, &Credentials::named(name), PROMPT, |_| {})
        .await
        .expect("authenticated");
}

/// An AUTH the server refuses (a protocol version it does not speak):
/// `ERROR` 13, and the session stays open, unauthenticated.
async fn failed_auth(c: &mut Conn) {
    let req = base::Auth {
        name: "old".into(),
        ticket: Vec::new(),
        protocol_version: gsb_protocol::PROTOCOL_VERSION + 1,
    };
    c.send(op::base::AUTH_REQ, &req.encode_to_vec())
        .await
        .expect("sent");
    match c.recv(PROMPT).await.expect("a frame") {
        Recv::Frame(f) if f.op == op::base::ERROR => {
            let e = base::Error::decode(&f.payload[..]).expect("an ERROR");
            assert_eq!(e.code, ErrorCode::ProtocolVersion as i32, "{e:?}");
        }
        other => panic!("expected ERROR 13, got {other:?}"),
    }
}

/// Wait for a report whose registry holds `conns` rows.
async fn rows(rx: &mut UnboundedReceiver<MetricReport>, conns: u32) -> MetricReport {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let report = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("the registry never held {conns} rows"))
            .expect("metrics open");
        if report.registry.as_ref().is_some_and(|r| r.conns == conns) {
            return report;
        }
    }
}

#[tokio::test]
async fn a_source_at_its_cap_is_refused_another_is_served_and_places_come_back() {
    let cfg = Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        idle_timeout_secs: 0.0,
        max_unauth_conns_per_source: Some(2),
        ..Default::default()
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = gsb_server::start_server_metrics(cfg, tx)
        .await
        .expect("server starts");
    let to = handle.addr;
    const A: [u8; 4] = [127, 0, 0, 1];
    // The door takes them in order, so each birth is decided in order.
    let mut a1 = connect_from(A, to).await;
    let mut a2 = connect_from(A, to).await;
    refused(connect_from(A, to).await).await;

    // Another source is not this one's cap.
    let mut b = connect_from([127, 0, 0, 2], to).await;
    auth(&mut b, "b").await;

    // AUTH success gives a place back: a3 takes it, a4 finds none.
    auth(&mut a1, "a1").await;
    let mut a3 = connect_from(A, to).await;
    refused(connect_from(A, to).await).await;

    // A failed AUTH does not: a2 is still unauthenticated.
    failed_auth(&mut a2).await;
    refused(connect_from(A, to).await).await;

    // A close does: a2 goes; a1, b and a3 are the rows left.
    drop(a2);
    rows(&mut rx, 3).await;
    let mut a5 = connect_from(A, to).await;
    auth(&mut a5, "a5").await;
    auth(&mut a3, "a3").await;

    // Three refusals, each under its own reason and nothing else.
    drop((a1, a3, a5, b));
    let report = rows(&mut rx, 0).await;
    let closes = report.net.server_closes;
    assert_eq!(
        closes.get(ServerClose::UnauthSourceCap),
        3,
        "{}",
        closes.nonzero_summary()
    );
    assert_eq!(closes.total(), 3, "{}", closes.nonzero_summary());
    handle.stop().await;
}
