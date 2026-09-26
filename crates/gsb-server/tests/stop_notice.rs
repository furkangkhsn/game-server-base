//! `ServerHandle::stop` tells the connected clients (BACKLOG B12), end to
//! end, on every door.
//!
//! Before: a stop was a SILENT disconnect — the connection actor's
//! `Shutdown` arm just ended — so a client could not tell "the server is
//! going down" (reconnect later, or elsewhere) from a network failure
//! (retry now). Now the actor queues `ERROR` code 14 (`ServerStopping`)
//! as its last frame, best effort and without waiting: the notice goes
//! out AHEAD of the close on every door that has one, and `stop()` still
//! never waits on a client (the last test).
//!
//! Per door, what arrives after the notice: TCP / TLS / QUIC — the end
//! of the stream; WebSocket — the door's close frame; rUDP — nothing
//! (the transport has no FIN: the notice is the only close signal a
//! rUDP client ever gets, which is exactly why it matters there).

use std::time::{Duration, Instant};

use gsb_protocol::base::{Auth, Error, ErrorCode, JoinRoom};
use gsb_protocol::op::base::{AUTH_REQ, ERROR, JOIN_ROOM_REQ, JOIN_ROOM_RESULT};
use gsb_server::{ListenerEntry, ListenerTransport};
use prost::Message;

#[path = "stop_notice/client.rs"]
mod client;
mod common;

use client::{Client, End};

/// `stop()` must finish well inside this, whatever the clients do.
const STOP_WITHIN: Duration = Duration::from_secs(5);
/// How long a client waits for each next frame after the stop.
const READ_WINDOW: Duration = Duration::from_secs(5);

/// A one-room server with a single door of the given kind.
async fn server(door: ListenerTransport, pki: &common::TlsPki) -> gsb_server::ServerHandle {
    let tls = matches!(door, ListenerTransport::Tls | ListenerTransport::Quic);
    let cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(vec![ListenerEntry {
            transport: door,
            bind: "127.0.0.1:0".into(),
            tls_cert: tls.then(|| pki.cert_pem_path.clone()),
            tls_key: tls.then(|| pki.key_pem_path.clone()),
        }]),
        ..Default::default()
    };
    gsb_server::start_server(cfg).await.expect("server starts")
}

/// AUTH + JOIN; returns once the join result arrived.
async fn join(c: &mut Client, name: &str) {
    let auth = Auth {
        name: name.into(),
        ticket: Vec::new(),
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    };
    c.write_frame(AUTH_REQ, &auth.encode_to_vec()).await;
    c.write_frame(JOIN_ROOM_REQ, &JoinRoom { room_id: 1 }.encode_to_vec())
        .await;
    loop {
        let (op, payload) = c.next(READ_WINDOW).await.expect("joined");
        assert_ne!(op, ERROR, "join refused: {:?}", Error::decode(&payload[..]));
        if op == JOIN_ROOM_RESULT {
            return;
        }
    }
}

/// Stop the server (bounded), then read what the client gets: every
/// frame up to the end of the session. Returns the ERROR frames seen
/// (in order) and how the session ended.
async fn stop_and_read(handle: gsb_server::ServerHandle, c: &mut Client) -> (Vec<Error>, End) {
    tokio::time::timeout(STOP_WITHIN, handle.stop())
        .await
        .expect("stop() did not complete");
    let mut errors = Vec::new();
    loop {
        match c.next(READ_WINDOW).await {
            Ok((ERROR, payload)) => errors.push(Error::decode(&payload[..]).expect("decodes")),
            Ok(_) => assert!(errors.is_empty(), "a frame after the notice"),
            Err(end) => return (errors, end),
        }
    }
}

/// The notice is the ONE error the client gets, and it says "stopping".
fn assert_stopping(door: &str, errors: &[Error]) {
    assert_eq!(errors.len(), 1, "{door}: exactly one notice: {errors:?}");
    assert_eq!(
        errors[0].code(),
        ErrorCode::ServerStopping,
        "{door}: {errors:?}"
    );
}

/// One door: join, stop, and the notice arrives before the door's end.
async fn door_gets_the_notice(door: ListenerTransport, tag: &str) -> End {
    let pki = common::mint_tls_pki(tag);
    let handle = server(door, &pki).await;
    let mut c = client::connect(door, &pki, handle.addrs[0]).await;
    join(&mut c, tag).await;
    let (errors, end) = stop_and_read(handle, &mut c).await;
    assert_stopping(tag, &errors);
    end
}

#[tokio::test]
async fn a_tcp_client_gets_the_stop_notice_before_the_close() {
    let end = door_gets_the_notice(ListenerTransport::Tcp, "stop-tcp").await;
    assert_eq!(end, End::Eof);
}

#[tokio::test]
async fn a_tls_client_gets_the_stop_notice_before_the_close() {
    let end = door_gets_the_notice(ListenerTransport::Tls, "stop-tls").await;
    assert_eq!(end, End::Eof);
}

#[tokio::test]
async fn a_websocket_client_gets_the_stop_notice_before_the_close_frame() {
    let end = door_gets_the_notice(ListenerTransport::Ws, "stop-ws").await;
    assert!(matches!(end, End::WsClose(_)), "{end:?}");
}

#[tokio::test]
async fn a_quic_client_gets_the_stop_notice_before_the_close() {
    let end = door_gets_the_notice(ListenerTransport::Quic, "stop-quic").await;
    assert_eq!(end, End::Eof);
}

/// rUDP has no FIN: after the notice there is only silence — the notice
/// IS the close signal.
#[tokio::test]
async fn a_rudp_client_gets_the_stop_notice() {
    let end = door_gets_the_notice(ListenerTransport::Udp, "stop-udp").await;
    assert_eq!(end, End::Quiet);
}

/// THE BOUND, end to end: clients that joined and then never read a byte
/// cannot hold `stop()` — it completes promptly — and whatever they read
/// afterwards still ends (a notice, when it had room in their queue, is
/// the last frame; a full queue drops it: best effort).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_completes_promptly_with_clients_that_do_not_read() {
    let pki = common::mint_tls_pki("stop-deaf");
    let handle = server(ListenerTransport::Tcp, &pki).await;
    let mut deaf = Vec::new();
    for i in 0..8 {
        let mut c = client::connect(ListenerTransport::Tcp, &pki, handle.addrs[0]).await;
        join(&mut c, &format!("deaf-{i}")).await;
        deaf.push(c);
    }
    // They stop reading; the room keeps producing for them meanwhile.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    tokio::time::timeout(STOP_WITHIN, handle.stop())
        .await
        .expect("stop() waited on clients that do not read");
    assert!(started.elapsed() < STOP_WITHIN);
    for mut c in deaf {
        let mut last_error = None;
        let end = loop {
            match c.next(READ_WINDOW).await {
                Ok((ERROR, p)) => last_error = Some(Error::decode(&p[..]).expect("decodes")),
                Ok(_) => assert!(last_error.is_none(), "a frame after the notice"),
                Err(end) => break end,
            }
        };
        assert_eq!(end, End::Eof, "the session still ends");
        if let Some(e) = last_error {
            assert_eq!(e.code(), ErrorCode::ServerStopping);
        }
    }
}
