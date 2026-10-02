//! `max_handshakes_per_source` (BACKLOG D11): the server-level per-source
//! cap of every handshaking door. Omitted, the doors are as they were;
//! set, one source address at its cap has its next connection closed
//! unhandshaken — on the TLS and the WebSocket door alike — while
//! another source is served, and the refusals reach the metrics report.

use std::net::SocketAddr;
use std::time::Duration;

use gsb_client::session::{self, Credentials};
use gsb_server::{Config, ListenerEntry, ListenerTransport};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpSocket, TcpStream};

mod common;

/// Far below the 10 s handshake deadline.
const PROMPT: Duration = Duration::from_secs(2);

fn parse(text: &str) -> Config {
    toml::from_str(text).expect("the config parses")
}

/// Omitted: no per-source cap — the doors every test and load run
/// (all from one loopback address) had.
#[test]
fn omitted_there_is_no_per_source_cap() {
    assert_eq!(parse("").max_handshakes_per_source, None);
    assert_eq!(Config::default().max_handshakes_per_source, None);
    assert_eq!(
        parse("max_handshakes_per_source = 8").max_handshakes_per_source,
        Some(8)
    );
}

/// A server-level key: a negative value does not parse, and neither a
/// room override nor a door entry can carry it.
#[test]
fn it_is_a_server_level_key() {
    let err = toml::from_str::<Config>("max_handshakes_per_source = -1").expect_err("refused");
    assert!(
        err.to_string().contains("max_handshakes_per_source"),
        "{err}"
    );
    let err =
        toml::from_str::<Config>("[rooms.1]\nmax_handshakes_per_source = 2").expect_err("refused");
    assert!(
        err.to_string().contains("max_handshakes_per_source"),
        "{err}"
    );
    let entry =
        "[[listeners]]\ntransport = \"ws\"\nbind = \"127.0.0.1:0\"\nmax_handshakes_per_source = 2";
    let err = toml::from_str::<Config>(entry).expect_err("refused");
    assert!(
        err.to_string().contains("max_handshakes_per_source"),
        "{err}"
    );
}

/// A TCP connection from `source` (any 127/8 address is the loopback).
async fn connect_from(source: [u8; 4], to: SocketAddr) -> TcpStream {
    let socket = TcpSocket::new_v4().expect("socket");
    socket
        .bind(SocketAddr::from((source, 0)))
        .expect("bind the source");
    socket.connect(to).await.expect("connect")
}

/// A TLS and a WebSocket door, each source capped at one handshake.
fn config(pki: &common::TlsPki) -> Config {
    let door = |transport, tls: bool| ListenerEntry {
        transport,
        bind: "127.0.0.1:0".into(),
        tls_cert: tls.then(|| pki.cert_pem_path.clone()),
        tls_key: tls.then(|| pki.key_pem_path.clone()),
    };
    Config {
        room_count: 1,
        listeners: Some(vec![
            door(ListenerTransport::Tls, true),
            door(ListenerTransport::Ws, false),
        ]),
        max_handshakes_per_source: Some(1),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_source_at_its_cap_is_refused_another_is_served_and_it_is_reported() {
    let pki = common::mint_tls_pki("per-source");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = gsb_server::start_server_metrics(config(&pki), tx)
        .await
        .expect("server starts");
    let (tls_addr, ws_addr) = (handle.addrs[0], handle.addrs[1]);
    let mut held = Vec::new();
    for addr in [tls_addr, ws_addr] {
        // The source's one slot, held by a silent peer.
        held.push(connect_from([127, 0, 0, 1], addr).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut refused = connect_from([127, 0, 0, 1], addr).await;
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(PROMPT, refused.read(&mut byte))
            .await
            .expect("refused at once, not held to the handshake deadline");
        assert!(
            matches!(read, Ok(0)) || read.is_err(),
            "{addr}: EOF or reset, got {read:?}"
        );
    }

    // Another source: a TLS session, and a WebSocket session through AUTH.
    let tls = async {
        let tcp = connect_from([127, 0, 0, 2], tls_addr).await;
        let name = common::TLS_SERVER_NAME.try_into().expect("dns name");
        common::tls_client_connector(&pki).connect(name, tcp).await
    };
    tokio::time::timeout(PROMPT, tls)
        .await
        .expect("prompt")
        .expect("a verified TLS session from another source");
    let session = async {
        let tcp = connect_from([127, 0, 0, 2], ws_addr).await;
        let mut c = gsb_client::connect::ws_stream(tcp, &ws_addr.to_string())
            .await
            .expect("upgrade");
        session::auth(&mut c, &Credentials::named("other-source"), PROMPT, |_| {})
            .await
            .expect("auth");
    };
    tokio::time::timeout(PROMPT, session)
        .await
        .expect("another source's upgrade and AUTH complete");

    // One more refusal per door past the intake's flush interval flushes
    // them all (B58): two per door.
    tokio::time::sleep(Duration::from_millis(600)).await;
    for addr in [tls_addr, ws_addr] {
        held.push(connect_from([127, 0, 0, 1], addr).await);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let report = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("the refusals reported in time")
            .expect("metrics channel open");
        let t = report.transport;
        if t.handshakes_refused_per_source == 4 {
            assert_eq!(t.handshakes_refused, 0, "not the door's bound");
            break;
        }
    }
    handle.stop().await;
}
