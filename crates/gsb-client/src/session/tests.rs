//! The session steps against a scripted peer (an in-memory stream): the
//! reply routing, and every way a step ends besides its reply — the
//! `ERROR` frame typed (9, 14, a code this build does not know), a
//! refused AUTH, EOF, the window. The real-server runs live in
//! `gsb-server/tests/client_session.rs`.

mod pins;

use std::time::Duration;

use gsb_protocol::base::{AuthResult, Error, ErrorCode, HeartbeatAck, JoinRoomResult};
use prost::Message;
use tokio::io::{DuplexStream, duplex};

use super::*;
use crate::frame::{FrameRx, FrameTx};

const W: Duration = Duration::from_secs(5);

/// A client connection and the peer's two halves.
fn pair() -> (Conn, FrameRx<DuplexStream>, FrameTx<DuplexStream>) {
    let (a, b) = duplex(1 << 16);
    let (c, d) = duplex(1 << 16);
    let conn = Conn::halves(Box::new(a), Box::new(c));
    (conn, FrameRx::new(d), FrameTx::new(b))
}

fn auth_ok() -> Vec<u8> {
    AuthResult {
        ok: true,
        ..Default::default()
    }
    .encode_to_vec()
}

fn error(code: i32, message: &str) -> Vec<u8> {
    Error {
        code,
        message: message.into(),
    }
    .encode_to_vec()
}

/// AUTH + JOIN: the peer sees exactly the two request frames; frames
/// that race the join result reach `other` in order; the step returns
/// the AUTH answer and the entity.
#[tokio::test]
async fn auth_and_join_returns_the_entity_and_hands_over_other_frames() {
    let (mut conn, mut rx, mut tx) = pair();
    let peer = tokio::spawn(async move {
        let a = rx.next().await.unwrap().unwrap();
        let j = rx.next().await.unwrap().unwrap();
        assert_eq!(
            (a.op, a.payload),
            (op::AUTH_REQ, auth_req(&Credentials::named("ana")).payload)
        );
        assert_eq!((j.op, j.payload), (op::JOIN_ROOM_REQ, join_req(4).payload));
        tx.send(op::AUTH_RESULT, &auth_ok()).await.unwrap();
        tx.send(1000, b"snap-1").await.unwrap();
        tx.send(1001, b"private").await.unwrap();
        tx.send(
            op::JOIN_ROOM_RESULT,
            &JoinRoomResult { entity: 42 }.encode_to_vec(),
        )
        .await
        .unwrap();
        (rx, tx)
    });
    let mut others = Vec::new();
    let joined = auth_and_join(&mut conn, &Credentials::named("ana"), 4, W, |f| {
        others.push((f.op, f.payload.to_vec()))
    })
    .await
    .expect("joined");
    assert_eq!(joined.entity, 42);
    assert!(joined.auth.ok);
    assert_eq!(
        others,
        vec![(1000, b"snap-1".to_vec()), (1001, b"private".to_vec())]
    );
    peer.await.unwrap();
}

/// An `ERROR` frame ends the wait as a typed server error: 9 (the
/// server closed the session) and 14 (the server is stopping) by name,
/// the message kept.
#[tokio::test]
async fn error_frames_come_back_typed() {
    for (code, name) in [
        (9, ErrorCode::ServerClosed),
        (14, ErrorCode::ServerStopping),
    ] {
        let (mut conn, _rx, mut tx) = pair();
        tx.send(op::ERROR, &error(code, "why")).await.unwrap();
        let e = join(&mut conn, 1, W, |_| {}).await.expect_err("an error");
        let s = e.server().expect("a server error");
        assert_eq!((s.code, s.raw, s.message.as_str()), (name, code, "why"));
    }
}

/// base.proto's forward-compatibility rule: a code this build does not
/// know reads as `Unspecified` (handled like OTHER) and keeps its raw
/// number for the report.
#[tokio::test]
async fn an_unknown_error_code_keeps_its_number_and_reads_as_unspecified() {
    let (mut conn, _rx, mut tx) = pair();
    tx.send(op::ERROR, &error(99, "future")).await.unwrap();
    let e = heartbeat_round(&mut conn, 1, W, |_| {})
        .await
        .expect_err("an error");
    let s = e.server().expect("a server error");
    assert_eq!((s.code, s.raw), (ErrorCode::Unspecified, 99));
}

#[tokio::test]
async fn a_refused_auth_is_its_own_error() {
    let (mut conn, _rx, mut tx) = pair();
    let no = AuthResult {
        ok: false,
        reason: "nope".into(),
        ..Default::default()
    };
    tx.send(op::AUTH_RESULT, &no.encode_to_vec()).await.unwrap();
    match auth(&mut conn, &Credentials::named("x"), W, |_| {}).await {
        Err(ClientError::AuthRefused(r)) => assert_eq!(r, "nope"),
        other => panic!("want AuthRefused, got {other:?}"),
    }
}

/// The stream ending before the reply is `Closed`; a silent peer is
/// `TimedOut` at the window.
#[tokio::test]
async fn eof_is_closed_and_silence_is_timed_out() {
    let (mut conn, rx, tx) = pair();
    drop((rx, tx));
    assert!(matches!(
        leave(&mut conn, W, |_| {}).await,
        Err(ClientError::Closed) | Err(ClientError::Io(_))
    ));
    let (mut conn, _rx, _tx) = pair();
    let started = std::time::Instant::now();
    let r = leave(&mut conn, Duration::from_millis(100), |_| {}).await;
    assert!(matches!(r, Err(ClientError::TimedOut)), "{r:?}");
    assert!(started.elapsed() >= Duration::from_millis(100));
}

/// HEARTBEAT returns the acknowledged tick; LEAVE its result.
#[tokio::test]
async fn heartbeat_and_leave_complete_on_their_replies() {
    let (mut conn, mut rx, mut tx) = pair();
    let peer = tokio::spawn(async move {
        let hb = rx.next().await.unwrap().unwrap();
        assert_eq!(hb.op, op::HEARTBEAT);
        tx.send(1000, b"snap").await.unwrap();
        tx.send(
            op::HEARTBEAT_ACK,
            &HeartbeatAck { tick: 77 }.encode_to_vec(),
        )
        .await
        .unwrap();
        let lv = rx.next().await.unwrap().unwrap();
        assert_eq!(lv.op, op::LEAVE_ROOM_REQ);
        tx.send(op::LEAVE_ROOM_RESULT, &[]).await.unwrap();
        (rx, tx)
    });
    let mut seen = 0;
    assert_eq!(
        heartbeat_round(&mut conn, 77, W, |_| seen += 1)
            .await
            .unwrap(),
        77
    );
    assert_eq!(seen, 1);
    leave(&mut conn, W, |_| {}).await.unwrap();
    peer.await.unwrap();
}
