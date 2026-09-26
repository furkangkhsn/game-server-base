//! `gsb_client` against the REAL server: its session steps over every
//! door it opens, the server's `ERROR` frames surfaced typed (9 at a
//! capacity rejection, 14 at a stop), and the resume path.
//!
//! The client crate's own suite pins its framing and its wire bytes
//! against a scripted peer; this suite is where its steps meet the
//! connection actor they were written against. It lives here, not in
//! `gsb-client`, so the client crate needs no dev-dependency on the
//! composition root (which depends on it).

use std::net::SocketAddr;
use std::time::Duration;

use gsb_client::session::{self, Credentials};
use gsb_client::{ClientError, Conn, Recv};
use gsb_protocol::base::ErrorCode;
use gsb_protocol::op::base as op;
use tokio::net::TcpStream;

mod common;

use common::{TLS_SERVER_NAME, TlsPki};

const W: Duration = Duration::from_secs(10);

/// The door a test walks in through.
#[derive(Clone, Copy, Debug)]
enum Door {
    Tcp,
    Tls,
    Udp,
    Quic,
}

/// A one-room server with a single door of the given kind.
async fn server(
    door: Door,
    pki: &TlsPki,
    tweak: impl FnOnce(&mut gsb_server::Config),
) -> gsb_server::ServerHandle {
    let (transport, tls) = match door {
        Door::Tcp => (gsb_server::ListenerTransport::Tcp, false),
        Door::Tls => (gsb_server::ListenerTransport::Tls, true),
        Door::Udp => (gsb_server::ListenerTransport::Udp, false),
        Door::Quic => (gsb_server::ListenerTransport::Quic, true),
    };
    let mut cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(vec![gsb_server::ListenerEntry {
            transport,
            bind: "127.0.0.1:0".into(),
            tls_cert: tls.then(|| pki.cert_pem_path.clone()),
            tls_key: tls.then(|| pki.key_pem_path.clone()),
        }]),
        ..Default::default()
    };
    tweak(&mut cfg);
    gsb_server::start_server(cfg).await.expect("server starts")
}

/// Open a connection through `door`.
async fn connect(door: Door, pki: &TlsPki, addr: SocketAddr) -> Conn {
    match door {
        Door::Tcp => gsb_client::connect::tcp(addr).await.expect("tcp"),
        Door::Tls => {
            let tcp = TcpStream::connect(addr).await.expect("tcp under tls");
            let connector = gsb_client::tls::connector([pki.ca_der.clone()]).expect("roots");
            let dns = TLS_SERVER_NAME.try_into().expect("dns name");
            gsb_client::tls::connect(tcp, &connector, dns)
                .await
                .expect("TLS handshake")
        }
        Door::Udp => gsb_client::connect::udp(addr)
            .await
            .expect("rUDP handshake"),
        Door::Quic => {
            let config = gsb_client::quic::client_config([pki.ca_der.clone()]).expect("QUIC TLS");
            gsb_client::quic::connect(addr, TLS_SERVER_NAME, config)
                .await
                .expect("QUIC handshake")
        }
    }
}

/// Every step, in order, on one door: AUTH + JOIN (the entity), a
/// HEARTBEAT round (its tick echoed), LEAVE (its result).
async fn steps_over(door: Door) {
    let pki = common::mint_tls_pki("client-steps");
    let handle = server(door, &pki, |_| {}).await;
    let mut c = connect(door, &pki, handle.addrs[0]).await;
    let creds = Credentials::named(format!("steps-{door:?}"));
    let joined = session::auth_and_join(&mut c, &creds, 1, W, |_| {})
        .await
        .unwrap_or_else(|e| panic!("{door:?}: join: {e}"));
    assert!(joined.auth.ok, "{door:?}");
    assert_ne!(joined.entity, 0, "{door:?}: a joined session has an entity");
    let tick = session::heartbeat_round(&mut c, 41, W, |_| {})
        .await
        .unwrap_or_else(|e| panic!("{door:?}: heartbeat: {e}"));
    assert_eq!(tick, 41, "{door:?}: the ack echoes the tick");
    session::leave(&mut c, W, |_| {})
        .await
        .unwrap_or_else(|e| panic!("{door:?}: leave: {e}"));
    handle.stop().await;
}

#[tokio::test]
async fn the_session_steps_run_over_tcp() {
    steps_over(Door::Tcp).await;
}

#[tokio::test]
async fn the_session_steps_run_over_tls() {
    steps_over(Door::Tls).await;
}

#[tokio::test]
async fn the_session_steps_run_over_rudp() {
    steps_over(Door::Udp).await;
}

#[tokio::test]
async fn the_session_steps_run_over_quic() {
    steps_over(Door::Quic).await;
}

/// A connection over the server's capacity is refused at birth with
/// `ERROR` 9 — the first step's wait returns it typed, and the stream
/// ends after it.
#[tokio::test]
async fn a_capacity_rejection_comes_back_as_server_closed() {
    let pki = common::mint_tls_pki("client-cap");
    let handle = server(Door::Tcp, &pki, |c| c.max_connections = Some(1)).await;
    let mut a = connect(Door::Tcp, &pki, handle.addrs[0]).await;
    session::auth(&mut a, &Credentials::named("cap-a"), W, |_| {})
        .await
        .expect("A is admitted");
    let mut b = connect(Door::Tcp, &pki, handle.addrs[0]).await;
    let e = session::auth(&mut b, &Credentials::named("cap-b"), W, |_| {})
        .await
        .expect_err("B is over capacity");
    let s = e
        .server()
        .unwrap_or_else(|| panic!("a server error, got {e:?}"));
    assert_eq!((s.code, s.raw), (ErrorCode::ServerClosed, 9), "{s}");
    assert!(
        matches!(b.recv(W).await, Ok(Recv::Closed) | Err(_)),
        "B's stream ends"
    );
    handle.stop().await;
}

/// A stop reaches a joined client as `ERROR` 14, typed, on its next
/// wait — on TCP (followed by the end of the stream) and on rUDP (where
/// the notice is the only close signal there is).
#[tokio::test]
async fn a_server_stop_comes_back_as_server_stopping() {
    for door in [Door::Tcp, Door::Udp] {
        let pki = common::mint_tls_pki("client-stop");
        let handle = server(door, &pki, |_| {}).await;
        let mut c = connect(door, &pki, handle.addrs[0]).await;
        let creds = Credentials::named(format!("stop-{door:?}"));
        session::auth_and_join(&mut c, &creds, 1, W, |_| {})
            .await
            .unwrap_or_else(|e| panic!("{door:?}: join: {e}"));
        tokio::time::timeout(W, handle.stop())
            .await
            .expect("stop() completes");
        let deadline = std::time::Instant::now() + W;
        let e = session::reply(&mut c, op::HEARTBEAT_ACK, deadline, &mut |_| {})
            .await
            .expect_err("the session ended");
        match &e {
            ClientError::Server(s) => {
                assert_eq!(
                    (s.code, s.raw),
                    (ErrorCode::ServerStopping, 14),
                    "{door:?}: {s}"
                )
            }
            other => panic!("{door:?}: want the stop notice, got {other:?}"),
        }
    }
}

/// THE resume path: the same credentials on a new connection get the
/// parked entity back (the demo parks a dropped player for the grace).
#[tokio::test]
async fn the_same_credentials_resume_the_same_entity() {
    let pki = common::mint_tls_pki("client-resume");
    let handle = server(Door::Tcp, &pki, |c| c.disconnect_grace_secs = 8.0).await;
    let creds = Credentials::named("resumer");
    let mut first = connect(Door::Tcp, &pki, handle.addrs[0]).await;
    let before = session::auth_and_join(&mut first, &creds, 1, W, |_| {})
        .await
        .expect("first join")
        .entity;
    drop(first); // the transport dies; no LEAVE: the room parks the entity
    // The close cascade (reader-pump EOF → ConnClosed → Detach) settles
    // first (the e2e resume flow's allowance), so the next join meets a
    // park rather than a still-live session.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let mut again = connect(Door::Tcp, &pki, handle.addrs[0]).await;
    let after = session::auth_and_join(&mut again, &creds, 1, W, |_| {})
        .await
        .expect("resume join")
        .entity;
    assert_eq!(after, before, "the resume key brings the same entity back");
    let mut other = connect(Door::Tcp, &pki, handle.addrs[0]).await;
    let fresh = session::auth_and_join(&mut other, &Credentials::named("other"), 1, W, |_| {})
        .await
        .expect("a different identity joins")
        .entity;
    assert_ne!(fresh, before, "a different key is a different entity");
    handle.stop().await;
}
