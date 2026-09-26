//! The stream wire: round trip, the size guard, the short-body refusal,
//! EOF inside a frame, and cancel safety under a bounded read.

use std::time::Duration;

use tokio::io::{AsyncWriteExt, duplex};

use super::*;

/// Frames written by the writer come back from the reader, in order,
/// and a clean close after the last one is `Ok(None)`.
#[tokio::test]
async fn frames_round_trip_and_eof_at_a_boundary_is_none() {
    let (a, b) = duplex(1 << 16);
    let mut tx = FrameTx::new(a);
    let mut rx = FrameRx::new(b);
    tx.send(7, b"hello").await.unwrap();
    tx.send_batch(&[
        FrameBody::new(1, vec![1u8, 2]),
        FrameBody::new(1000, Vec::new()),
    ])
    .await
    .unwrap();
    tx.feed(9, &[0xff; 300]).await.unwrap();
    tx.flush().await.unwrap();
    drop(tx);
    let mut got = Vec::new();
    while let Some(f) = rx.next().await.unwrap() {
        got.push((f.op, f.payload.to_vec()));
    }
    assert_eq!(
        got,
        vec![
            (7, b"hello".to_vec()),
            (1, vec![1, 2]),
            (1000, vec![]),
            (9, vec![0xff; 300]),
        ]
    );
}

/// A declared body longer than the guard is refused as `InvalidData`
/// from its prefix alone (the body is never awaited); the guard itself
/// is inclusive.
#[tokio::test]
async fn a_frame_over_the_guard_is_refused_from_its_prefix() {
    let (mut a, b) = duplex(1 << 16);
    let mut rx = FrameRx::with_max(b, 64);
    a.write_all(&encode(5, &[0u8; 62])).await.unwrap(); // body 64: at the guard
    a.write_all(&65u32.to_le_bytes()).await.unwrap(); // body 65: over it
    let at = rx.next().await.unwrap().expect("a frame at the guard");
    assert_eq!((at.op, at.payload.len()), (5, 62));
    let over = tokio::time::timeout(Duration::from_secs(1), rx.next())
        .await
        .expect("refused without waiting for the body")
        .expect_err("over the guard");
    assert_eq!(over.kind(), std::io::ErrorKind::InvalidData);
}

/// The default guard is the 4 MiB every replaced copy used.
#[tokio::test]
async fn the_default_guard_is_four_mebibytes() {
    assert_eq!(DEFAULT_MAX_FRAME_BYTES, 4 * 1024 * 1024);
    let (mut a, b) = duplex(64);
    let mut rx = FrameRx::new(b);
    a.write_all(&((4 * 1024 * 1024 + 1) as u32).to_le_bytes())
        .await
        .unwrap();
    let e = tokio::time::timeout(Duration::from_secs(1), rx.next())
        .await
        .expect("refused without waiting for the body")
        .expect_err("over the default guard");
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
}

/// A body too short to carry an opcode (length 0 or 1) is refused.
#[tokio::test]
async fn a_body_without_an_opcode_is_refused() {
    for len in [0u32, 1] {
        let (mut a, b) = duplex(64);
        let mut rx = FrameRx::new(b);
        a.write_all(&len.to_le_bytes()).await.unwrap();
        a.write_all(&vec![7u8; len as usize]).await.unwrap();
        let e = rx.next().await.expect_err("no opcode");
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "len {len}");
    }
}

/// A stream that ends inside a frame is `UnexpectedEof` — whether it
/// stopped in the length prefix or in the body — not a clean end.
#[tokio::test]
async fn eof_inside_a_frame_is_unexpected_eof() {
    for cut in [2, 7] {
        let (mut a, b) = duplex(64);
        let mut rx = FrameRx::new(b);
        a.write_all(&encode(3, b"abcdef")[..cut]).await.unwrap();
        drop(a);
        let e = rx.next().await.expect_err("inside a frame");
        assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof, "cut at {cut}");
    }
}

/// THE cancel-safety rule: a bounded read that gives up between the
/// length prefix and the body loses nothing — the next read returns the
/// whole frame, and the stream stays in step for the one after it.
#[tokio::test]
async fn a_read_cancelled_mid_frame_loses_nothing() {
    let (mut a, b) = duplex(1 << 16);
    let mut rx = FrameRx::new(b);
    let first = encode(1001, b"0123456789");
    a.write_all(&first[..5]).await.unwrap(); // the prefix and one op byte
    let quiet = tokio::time::timeout(Duration::from_millis(50), rx.next()).await;
    assert!(quiet.is_err(), "no whole frame yet");
    a.write_all(&first[5..]).await.unwrap();
    a.write_all(&encode(1002, b"next")).await.unwrap();
    let f = rx.next().await.unwrap().expect("the frame");
    assert_eq!((f.op, &f.payload[..]), (1001, &b"0123456789"[..]));
    let g = rx.next().await.unwrap().expect("still in step");
    assert_eq!((g.op, &g.payload[..]), (1002, &b"next"[..]));
}
