//! The WebSocket half against a scripted server over in-memory pipes:
//! the handshake, masking, reassembly, the ping/pong and close rules,
//! the guard, refusals, and cancel safety both ways. The real door runs
//! live in `gsb-server/tests/ws_client.rs`.

mod close;
mod frames;
mod guard;
mod handshake;

use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

use super::*;
use crate::Recv;
use crate::frame::encode;

/// A generous bound on every scripted step (never reached when green).
const W: Duration = Duration::from_secs(5);

/// The scripted server's ends of the two pipes.
struct Peer {
    /// What the client wrote.
    rd: DuplexStream,
    /// What the client reads.
    wr: DuplexStream,
}

/// One client frame as the server sees it, payload unmasked.
#[derive(Debug)]
struct ClientFrame {
    fin: bool,
    opcode: u8,
    masked: bool,
    key: [u8; 4],
    payload: Vec<u8>,
}

/// A WS connection (already upgraded) with a frame guard of `max`, and
/// the server's ends.
fn pair(max: usize) -> (Conn, Peer) {
    let (client_rd, server_wr) = duplex(1 << 20);
    let (server_rd, client_wr) = duplex(1 << 20);
    let conn = super::conn(
        Box::new(client_rd),
        Box::new(client_wr),
        BytesMut::new(),
        max,
    );
    (
        conn,
        Peer {
            rd: server_rd,
            wr: server_wr,
        },
    )
}

/// One server frame, unmasked, lengths in their minimal form.
fn server_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![if fin { 0x80 } else { 0 } | opcode];
    match payload.len() {
        n if n < 126 => out.push(n as u8),
        n if n <= 0xffff => {
            out.push(126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

impl Peer {
    async fn send(&mut self, bytes: &[u8]) {
        self.wr.write_all(bytes).await.unwrap();
    }

    /// One gsb frame as one binary message.
    async fn game(&mut self, op: u16, payload: &[u8]) {
        self.send(&server_frame(true, OP_BIN, &encode(op, payload)))
            .await;
    }

    /// The client's next frame (bounded: a missing frame fails the test).
    async fn read(&mut self) -> ClientFrame {
        tokio::time::timeout(W, self.read_inner())
            .await
            .expect("a client frame")
    }

    async fn read_inner(&mut self) -> ClientFrame {
        let mut h = [0u8; 2];
        self.rd.read_exact(&mut h).await.unwrap();
        let len = match h[1] & 0x7f {
            126 => usize::from(self.rd.read_u16().await.unwrap()),
            127 => self.rd.read_u64().await.unwrap() as usize,
            n => usize::from(n),
        };
        let masked = h[1] & 0x80 != 0;
        let mut key = [0u8; 4];
        if masked {
            self.rd.read_exact(&mut key).await.unwrap();
        }
        let mut payload = vec![0u8; len];
        self.rd.read_exact(&mut payload).await.unwrap();
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= key[i & 3];
        }
        ClientFrame {
            fin: h[0] & 0x80 != 0,
            opcode: h[0] & 0x0f,
            masked,
            key,
            payload,
        }
    }

    /// Nothing more from the client within a short window.
    async fn silent(&mut self) {
        let mut b = [0u8; 1];
        let got = tokio::time::timeout(Duration::from_millis(100), self.rd.read(&mut b)).await;
        assert!(got.is_err(), "the client wrote more: {got:?}");
    }
}

/// The next frame, which must be one.
async fn frame(conn: &mut Conn) -> gsb_protocol::FrameBody {
    match conn.recv(W).await.expect("no error") {
        Recv::Frame(f) => f,
        other => panic!("want a frame, got {other:?}"),
    }
}
