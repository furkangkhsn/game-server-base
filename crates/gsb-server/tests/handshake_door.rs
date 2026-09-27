//! The handshaking doors (WebSocket, TLS) behind the composition root
//! run each handshake off the accept loop (BACKLOG B31): a silent peer
//! no longer holds a door, and the door's bound on handshakes in flight
//! is the server's pre-auth cap.
//!
//! Before: the B29 measurement — one socket that never sent its upgrade
//! held the WebSocket door for the 10 s handshake deadline (20 clients
//! behind it: connect p50 9610 ms); the TLS door had the same shape.

use std::time::Duration;

use gsb_client::session::{self, Credentials};
use gsb_server::{ListenerEntry, ListenerTransport};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

mod common;

/// Far below the 10 s handshake deadline.
const PROMPT: Duration = Duration::from_secs(2);

/// A one-room server with a TLS and a WebSocket door (in that order).
async fn server(pki: &common::TlsPki, max_unauth_conns: Option<u64>) -> gsb_server::ServerHandle {
    let door = |transport, tls: bool| ListenerEntry {
        transport,
        bind: "127.0.0.1:0".into(),
        tls_cert: tls.then(|| pki.cert_pem_path.clone()),
        tls_key: tls.then(|| pki.key_pem_path.clone()),
    };
    let cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(vec![
            door(ListenerTransport::Tls, true),
            door(ListenerTransport::Ws, false),
        ]),
        max_unauth_conns,
        ..Default::default()
    };
    gsb_server::start_server(cfg).await.expect("server starts")
}

#[tokio::test]
async fn a_silent_peer_holds_no_door() {
    let pki = common::mint_tls_pki("handshake-door");
    let handle = server(&pki, None).await;
    let (tls_addr, ws_addr) = (handle.addrs[0], handle.addrs[1]);
    // Silent peers: connected, not one byte of TLS hello or upgrade.
    let _silent_tls = TcpStream::connect(tls_addr).await.expect("to TLS");
    let _silent_ws = TcpStream::connect(ws_addr).await.expect("to WS");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let tls = async {
        let tcp = TcpStream::connect(tls_addr).await?;
        let name = common::TLS_SERVER_NAME.try_into().expect("dns name");
        common::tls_client_connector(&pki).connect(name, tcp).await
    };
    tokio::time::timeout(PROMPT, tls)
        .await
        .expect("the TLS handshake completes past the silent peer")
        .expect("a verified TLS session");

    // Over WebSocket, all the way to a session: the accept loop took it.
    let session = async {
        let mut c = gsb_client::connect::ws(ws_addr).await.expect("upgrade");
        session::auth(
            &mut c,
            &Credentials::named("past-the-silent-one"),
            PROMPT,
            |_| {},
        )
        .await
        .expect("auth");
    };
    tokio::time::timeout(PROMPT, session)
        .await
        .expect("the upgrade and AUTH complete past the silent peer");
    handle.stop().await;
}

/// The pre-auth cap is each door's handshake bound: with a cap of one,
/// a silent peer holds the only slot and the next connection is closed
/// at once, unhandshaken.
#[tokio::test]
async fn the_unauth_cap_bounds_each_doors_handshakes() {
    let pki = common::mint_tls_pki("handshake-bound");
    let handle = server(&pki, Some(1)).await;
    for addr in handle.addrs.clone() {
        let _silent = TcpStream::connect(addr).await.expect("the slot holder");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut refused = TcpStream::connect(addr).await.expect("connect");
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(PROMPT, refused.read(&mut byte))
            .await
            .expect("refused at once, not held to the handshake deadline");
        assert!(
            matches!(read, Ok(0)) || read.is_err(),
            "{addr}: EOF or reset, got {read:?}"
        );
    }
    handle.stop().await;
}

/// The doors' refusals reach the server's metrics report (BACKLOG B58):
/// the composition root gives every handshaking door the collector's
/// channel. Two refusals per door; the second, past the intake's flush
/// interval, flushes both.
#[tokio::test]
async fn the_doors_refusals_reach_the_report() {
    let pki = common::mint_tls_pki("handshake-metrics");
    let door = |transport, tls: bool| ListenerEntry {
        transport,
        bind: "127.0.0.1:0".into(),
        tls_cert: tls.then(|| pki.cert_pem_path.clone()),
        tls_key: tls.then(|| pki.key_pem_path.clone()),
    };
    let cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(vec![
            door(ListenerTransport::Tls, true),
            door(ListenerTransport::Ws, false),
        ]),
        max_unauth_conns: Some(1),
        ..Default::default()
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = gsb_server::start_server_metrics(cfg, tx)
        .await
        .expect("server starts");
    let mut held = Vec::new();
    for addr in handle.addrs.clone() {
        held.push(TcpStream::connect(addr).await.expect("the slot holder"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        held.push(TcpStream::connect(addr).await.expect("refused"));
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    for addr in handle.addrs.clone() {
        held.push(TcpStream::connect(addr).await.expect("refused, flushing"));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let report = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("the refusals reported in time")
            .expect("metrics channel open");
        if report.transport.handshakes_refused == 4 {
            break;
        }
    }
    handle.stop().await;
}
