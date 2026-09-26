//! A transport REFUSING the inbound byte stream tells the client why
//! (BACKLOG B13), end to end.
//!
//! Before: the reader pump's `InvalidData` exit (a frame over
//! `max_frame_bytes`, an undecodable frame body, a WebSocket protocol
//! violation, a corrupt TLS record) was the one server verdict with no
//! notice — the client saw the same silent close as a network failure.
//! Now the actor answers it like every other server verdict: `ERROR`
//! code 9 with the reason, best effort, then the close.
//!
//! The WebSocket door is the exception by design: it has ALREADY told
//! the client, in its own vocabulary — the close frame with the RFC 6455
//! status code (1002 / 1003 / 1007 / 1009) — and RFC 6455 §5.5.1 forbids
//! a data frame after a close frame. So there the notice is the close
//! frame, and nothing follows it.

use std::time::Duration;

use gsb_client::session::{self, Credentials};
use gsb_protocol::base::{Error, ErrorCode};
use gsb_protocol::op::base::{AUTH_RESULT, ERROR};
use gsb_server::{ListenerEntry, ListenerTransport};
use prost::Message;
use tokio::io::AsyncWriteExt;

#[path = "stop_notice/client.rs"]
mod client;
mod common;

use client::{Client, End};

const READ_WINDOW: Duration = Duration::from_secs(5);

/// A one-room server with a single door of the given kind, and an
/// authenticated client on it (so the session is past the pre-auth
/// budget's reach and plainly alive when the bad bytes arrive).
async fn session(
    door: ListenerTransport,
    tag: &str,
) -> (gsb_server::ServerHandle, Client, common::TlsPki) {
    let pki = common::mint_tls_pki(tag);
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
    let handle = gsb_server::start_server(cfg).await.expect("server starts");
    let mut c = client::connect(door, &pki, handle.addrs[0]).await;
    let auth = session::auth_req(&Credentials::named(tag));
    c.write_frame(auth.op, &auth.payload).await;
    let (op, _) = c.next(READ_WINDOW).await.expect("auth answered");
    assert_eq!(op, AUTH_RESULT);
    (handle, c, pki)
}

/// Read to the end of the session: the ERROR frames seen, and the end.
async fn read_to_end(c: &mut Client) -> (Vec<Error>, End) {
    let mut errors = Vec::new();
    loop {
        match c.next(READ_WINDOW).await {
            Ok((ERROR, p)) => errors.push(Error::decode(&p[..]).expect("decodes")),
            Ok((op, _)) => panic!("unexpected frame {op} after the rejection"),
            Err(end) => return (errors, end),
        }
    }
}

/// A length prefix far over `max_frame_bytes`: the framing codec refuses
/// the stream before reading the body.
fn oversized_prefix() -> [u8; 4] {
    u32::MAX.to_le_bytes()
}

fn assert_stream_rejected(door: &str, errors: &[Error]) {
    assert_eq!(errors.len(), 1, "{door}: exactly one notice: {errors:?}");
    assert_eq!(errors[0].code(), ErrorCode::ServerClosed, "{door}");
    assert!(
        errors[0].message.starts_with("stream rejected: "),
        "{door}: the message names the verdict: {:?}",
        errors[0].message
    );
}

#[tokio::test]
async fn an_oversized_tcp_frame_gets_error_9_before_the_close() {
    let (handle, mut c, _pki) = session(ListenerTransport::Tcp, "rej-tcp").await;
    c.raw().write_all(&oversized_prefix()).await.expect("write");
    let (errors, end) = read_to_end(&mut c).await;
    assert_stream_rejected("tcp", &errors);
    assert_eq!(end, End::Eof);
    handle.stop().await;
}

/// Inside a healthy TLS session the notice lands like on TCP. (A CORRUPT
/// record is the other TLS rejection, and there it cannot: the session
/// itself is dead — rustls has sent its fatal alert — so the write
/// fails and the client sees only the alert and the end.)
#[tokio::test]
async fn an_oversized_tls_frame_gets_error_9_before_the_close() {
    let (handle, mut c, _pki) = session(ListenerTransport::Tls, "rej-tls").await;
    let t = c.raw();
    t.write_all(&oversized_prefix()).await.expect("write");
    t.flush().await.expect("flush");
    let (errors, end) = read_to_end(&mut c).await;
    assert_stream_rejected("tls", &errors);
    assert_eq!(end, End::Eof);
    handle.stop().await;
}

#[tokio::test]
async fn an_oversized_quic_frame_gets_error_9_before_the_close() {
    let (handle, mut c, _pki) = session(ListenerTransport::Quic, "rej-quic").await;
    c.raw().write_all(&oversized_prefix()).await.expect("write");
    let (errors, end) = read_to_end(&mut c).await;
    assert_stream_rejected("quic", &errors);
    assert_eq!(end, End::Eof);
    handle.stop().await;
}

/// A text message (outside the wire contract): the door answers with its
/// close frame, status 1003 — and NOTHING after it, not even the notice.
#[tokio::test]
async fn a_websocket_violation_gets_the_close_frame_and_nothing_after_it() {
    let (handle, mut c, _pki) = session(ListenerTransport::Ws, "rej-ws").await;
    // One masked FIN text frame, "hi".
    c.ws_frame(true, gsb_client::ws::OP_TEXT, b"hi").await;
    let (errors, end) = read_to_end(&mut c).await;
    assert!(
        errors.is_empty(),
        "no data frame before the close: {errors:?}"
    );
    assert_eq!(end, End::WsClose(1003u16.to_be_bytes().to_vec()));
    // RFC 6455 §5.5.1: the close frame is the last frame on the wire.
    match c.next(Duration::from_millis(500)).await {
        Err(End::Eof) | Err(End::Quiet) => {}
        other => panic!("a frame after the close frame: {other:?}"),
    }
    handle.stop().await;
}
