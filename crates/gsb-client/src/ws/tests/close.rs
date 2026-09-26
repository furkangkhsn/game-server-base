//! The end of a session: a close frame is `Recv::Closed` with its code
//! and reason on `ws_close`, echoed once (code only); no data frame
//! follows it either way; an empty close has no code; a TCP end without
//! a close is `Closed` with no close frame.

use std::io::ErrorKind;

use super::*;

fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    let mut p = code.to_be_bytes().to_vec();
    p.extend_from_slice(reason.as_bytes());
    p
}

/// A frame that arrived before the close is still read first; then the
/// close is `Closed`, its code and reason surfaced, echoed with the code
/// alone; no data goes out after it; the TCP end is `Closed` again.
#[tokio::test]
async fn a_close_frame_surfaces_its_code_and_is_echoed_once() {
    let (mut conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
    peer.game(4, b"last").await;
    peer.send(&server_frame(true, OP_CLOSE, &close_payload(1003, "text")))
        .await;
    assert_eq!(frame(&mut conn).await.op, 4);
    assert!(conn.ws_close().is_none(), "no close before it is read");
    let end = conn.recv(W).await.unwrap();
    assert!(matches!(end, Recv::Closed), "{end:?}");
    assert_eq!(
        conn.ws_close(),
        Some(&WsClose {
            code: Some(1003),
            reason: "text".into()
        })
    );
    let echo = peer.read().await;
    assert!(echo.masked);
    assert_eq!(
        (echo.opcode, echo.payload),
        (OP_CLOSE, 1003u16.to_be_bytes().to_vec())
    );
    // No data frame after a close (§5.5.1), and no second echo.
    let e = conn.send(1, b"late").await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::BrokenPipe);
    // A later read waits for the server's TCP end: quiet while the peer
    // lingers, `Closed` at its end — the close still on record.
    let quiet = conn.recv(Duration::from_millis(50)).await.unwrap();
    assert!(matches!(quiet, Recv::Quiet), "{quiet:?}");
    peer.silent().await;
    drop(peer.wr);
    assert!(matches!(conn.recv(W).await.unwrap(), Recv::Closed));
    assert_eq!(conn.ws_close().and_then(|c| c.code), Some(1003));
    assert!(
        matches!(conn.recv(W).await.unwrap(), Recv::Closed),
        "sticky"
    );
}

/// Nothing may follow the close frame on the wire (§5.5.1): a frame
/// after it — in the same segment or later — is refused.
#[tokio::test]
async fn a_frame_after_the_close_frame_is_refused() {
    for same_segment in [true, false] {
        let (mut conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
        let close = server_frame(true, OP_CLOSE, &close_payload(1000, ""));
        let data = server_frame(true, OP_BIN, &encode(1, b"late"));
        if same_segment {
            peer.send(&[close, data].concat()).await;
        } else {
            peer.send(&close).await;
        }
        assert!(matches!(conn.recv(W).await.unwrap(), Recv::Closed));
        if !same_segment {
            peer.game(1, b"late").await;
        }
        let e = conn.recv(W).await.unwrap_err();
        assert_eq!(
            e.kind(),
            ErrorKind::InvalidData,
            "same segment: {same_segment}"
        );
    }
}

/// An empty close: `code: None`, echoed empty.
#[tokio::test]
async fn an_empty_close_has_no_code() {
    let (mut conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
    peer.send(&server_frame(true, OP_CLOSE, &[])).await;
    assert!(matches!(conn.recv(W).await.unwrap(), Recv::Closed));
    assert_eq!(
        conn.ws_close(),
        Some(&WsClose {
            code: None,
            reason: String::new()
        })
    );
    let echo = peer.read().await;
    assert_eq!((echo.opcode, echo.payload.len()), (OP_CLOSE, 0));
}

/// The server ends the TCP stream at a message boundary with no close
/// frame: `Closed`, and `ws_close` says there was none.
#[tokio::test]
async fn an_end_without_a_close_frame_has_no_close() {
    let (mut conn, peer) = pair(DEFAULT_MAX_FRAME_BYTES);
    drop(peer.wr);
    assert!(matches!(conn.recv(W).await.unwrap(), Recv::Closed));
    assert!(conn.ws_close().is_none());
}

/// A close the client started: the server's echo ends the session, and
/// the client does not echo the echo.
#[tokio::test]
async fn the_echo_of_a_client_close_is_not_echoed_back() {
    let (conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
    let Conn::Stream { mut rx, mut tx } = conn else {
        unreachable!()
    };
    tx.ws_frame(true, OP_CLOSE, &1000u16.to_be_bytes())
        .await
        .unwrap();
    let sent = peer.read().await;
    assert_eq!(sent.opcode, OP_CLOSE);
    peer.send(&server_frame(true, OP_CLOSE, &1000u16.to_be_bytes()))
        .await;
    assert!(rx.next().await.unwrap().is_none());
    assert_eq!(rx.ws_close().and_then(|c| c.code), Some(1000));
    tx.flush().await.unwrap();
    peer.silent().await;
}

/// A 1-byte close payload has no status code; a non-UTF-8 reason is not
/// a reason: both refused.
#[tokio::test]
async fn a_malformed_close_is_refused() {
    for payload in [vec![3u8], vec![0x03, 0xe8, 0xff, 0xfe]] {
        let (mut conn, mut peer) = pair(DEFAULT_MAX_FRAME_BYTES);
        peer.send(&server_frame(true, OP_CLOSE, &payload)).await;
        let e = conn.recv(W).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidData, "{payload:?}");
        assert!(conn.ws_close().is_none());
    }
}
