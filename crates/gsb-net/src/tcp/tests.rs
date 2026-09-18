//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

use super::*;
use futures::SinkExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

/// A loopback peer that accepts one connection and echoes everything it
/// receives until the client closes the write side.
fn echo_server(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (peer, _) = listener.accept().await.expect("accept");
        let (mut r, mut w) = peer.into_split();
        let mut buf = vec![0u8; 65536];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break, // EOF or error: client went away
                Ok(n) => {
                    w.write_all(&buf[..n]).await.expect("echo write");
                    w.flush().await.expect("echo flush");
                }
            }
        }
    })
}

async fn connect(listener: TcpListener) -> (TcpStream, JoinHandle<()>) {
    let addr = listener.local_addr().unwrap();
    let server = echo_server(listener);
    let stream = TcpStream::connect(addr).await.unwrap();
    (stream, server)
}

/// Write a frame the wire way, then read the echoed frame back through
/// TcpReader.
#[tokio::test]
async fn framing_roundtrip() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (stream, server) = connect(listener).await;

    let (mut reader, mut writer) = TcpReader::for_stream(stream, 1024);

    // Encode via TcpWriter.
    writer
        .send(FrameBody::new(7, b"payload".as_slice()))
        .await
        .unwrap();
    writer.flush().await.unwrap();

    // Decode via TcpReader (echoed by the peer).
    let frame = reader.next().await.expect("frame").expect("io");
    assert_eq!(frame.op, 7);
    assert_eq!(frame.payload, b"payload".as_slice());

    drop(writer);
    server.await.expect("echo server");
}

/// Multiple frames in one write are re-assembled correctly, and EOF is
/// observed once the peer closes.
#[tokio::test]
async fn multiple_frames() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (stream, server) = connect(listener).await;

    let (mut reader, mut writer) = TcpReader::for_stream(stream, 1024);
    writer
        .send(FrameBody::new(1, b"a".as_slice()))
        .await
        .unwrap();
    writer
        .send(FrameBody::new(2, b"bb".as_slice()))
        .await
        .unwrap();
    writer
        .send(FrameBody::new(3, b"ccc".as_slice()))
        .await
        .unwrap();
    writer.flush().await.unwrap();

    for (i, op) in [1u16, 2, 3].into_iter().enumerate() {
        let frame = reader.next().await.expect("frame").expect("io");
        assert_eq!(frame.op, op, "frame {i}");
    }
    // Close our write side (OwnedWriteHalf half-closes on drop) so the
    // echo server exits and the reader observes EOF.
    drop(writer);
    server.await.expect("echo server");
    assert!(reader.next().await.is_none(), "stream ended");
}

/// A length prefix that exceeds `max_frame_bytes` must produce an error,
/// never a panic or a memory blow-up.
#[tokio::test]
async fn oversized_frame_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        // Claim a 10 MiB body (max is 8), then close.
        peer.write_all(&10_485_760u32.to_le_bytes()).await.unwrap();
        drop(peer);
    });
    let stream = TcpStream::connect(addr).await.unwrap();
    server.await.unwrap();

    let (mut reader, _writer) = TcpReader::for_stream(stream, 8);
    let result = reader.next().await.expect("codec result");
    assert!(result.is_err(), "reader must reject the oversized frame");
}

// ── session-lifecycle idle timeout (the reader pump's deadline) ──
//
// These lock the half-open-TCP guardrail: with `idle_timeout` armed,
// silence is fatal (ServerClosed), traffic resets the window, and a
// clean EOF still reports "peer closed" — not a timeout.

use std::time::Duration;

use gsb_core::channel::channel;
use gsb_core::id::ConnectionId;

mod idle;

// ── session-lifecycle write stall (the writer pump's deadline) ──
//
// The symmetric guardrail: a peer that stops READING wedges the writer
// inside its socket write, which no inbound clock can see.

mod stall;
