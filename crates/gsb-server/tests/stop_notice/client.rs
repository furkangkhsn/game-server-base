//! A five-door test client for the close-notice suite: connect, write a
//! length-prefixed frame, and read what the server sends up to the END
//! of the session — reporting HOW it ended, which is what this suite is
//! about. Four doors are `gsb_client` connections (TCP, TLS, QUIC, rUDP);
//! the WebSocket door is a raw-TCP RFC 6455 client (one binary message
//! per frame), since the client building block has no WebSocket half.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::time::Duration;

use gsb_client::{Conn, Recv};
use gsb_server::ListenerTransport;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::common::{self, TLS_SERVER_NAME};

/// How the session ended, as the client observed it.
#[derive(Debug, PartialEq, Eq)]
pub enum End {
    /// The stream doors' EOF (TCP FIN, TLS close, QUIC stream finish).
    Eof,
    /// A WebSocket close frame, with its payload.
    WsClose(Vec<u8>),
    /// Nothing more arrived within the window (rUDP: there is no FIN).
    Quiet,
}

pub enum Client {
    Gsb(Conn),
    Ws(TcpStream),
}

pub async fn connect(door: ListenerTransport, pki: &common::TlsPki, addr: SocketAddr) -> Client {
    Client::Gsb(match door {
        ListenerTransport::Tcp => gsb_client::connect::tcp(addr).await.expect("tcp"),
        ListenerTransport::Tls => {
            let tcp = TcpStream::connect(addr).await.expect("tcp under tls");
            let dns: rustls::pki_types::ServerName<'static> =
                TLS_SERVER_NAME.try_into().expect("dns name");
            gsb_client::tls::connect(tcp, &common::tls_client_connector(pki), dns)
                .await
                .expect("TLS handshake")
        }
        ListenerTransport::Udp => gsb_client::connect::udp(addr)
            .await
            .expect("rUDP handshake"),
        ListenerTransport::Quic => {
            let config = gsb_client::quic::client_config([pki.ca_der.clone()]).expect("QUIC TLS");
            gsb_client::quic::connect(addr, TLS_SERVER_NAME, config)
                .await
                .expect("QUIC handshake")
        }
        ListenerTransport::Ws => return Client::Ws(connect_ws(addr).await),
    })
}

impl Client {
    pub async fn write_frame(&mut self, op: u16, payload: &[u8]) {
        match self {
            Client::Gsb(c) => c.send(op, payload).await.expect("write"),
            Client::Ws(s) => {
                // One masked FIN binary message carrying one game frame.
                let bytes = gsb_client::frame::encode(op, payload);
                let key = [0x5a, 0xa5, 0x3c, 0xc3];
                let mut msg = vec![0x82];
                assert!(bytes.len() < 126, "short frames only");
                msg.push(0x80 | bytes.len() as u8);
                msg.extend_from_slice(&key);
                msg.extend(bytes.iter().enumerate().map(|(i, b)| b ^ key[i & 3]));
                s.write_all(&msg).await.expect("write");
            }
        }
    }

    /// The byte sink under a stream door's frames — for bytes outside
    /// the frame contract (the `stream_rejected` suite's malformed
    /// prefix; the close-notice suite never writes one).
    #[allow(dead_code)]
    pub fn raw(&mut self) -> &mut gsb_client::conn::BoxWrite {
        match self {
            Client::Gsb(Conn::Stream { tx, .. }) => tx.get_mut(),
            _ => panic!("not a stream door"),
        }
    }

    /// The next frame, or how the session ended (`Err`) — `Quiet` when
    /// nothing arrives within `window`.
    pub async fn next(&mut self, window: Duration) -> Result<(u16, Vec<u8>), End> {
        match self {
            Client::Gsb(c) => match c.recv(window).await {
                Ok(Recv::Frame(f)) => Ok((f.op, f.payload.to_vec())),
                Ok(Recv::Closed) => Err(End::Eof),
                Ok(Recv::Quiet) => Err(End::Quiet),
                Err(_) if c.is_udp() => Err(End::Quiet),
                // A stream ends at a frame boundary, never inside one.
                Err(e) if matches!(e.kind(), ErrorKind::UnexpectedEof | ErrorKind::InvalidData) => {
                    panic!("a whole frame: {e}")
                }
                // Any other read failure at a frame boundary is the end
                // of the stream.
                Err(_) => Err(End::Eof),
            },
            Client::Ws(s) => tokio::time::timeout(window, ws_frame(s))
                .await
                .unwrap_or(Err(End::Quiet)),
        }
    }
}

/// One server WS frame: a binary message carrying one game frame, or the
/// close frame that ends the session.
async fn ws_frame(s: &mut TcpStream) -> Result<(u16, Vec<u8>), End> {
    let mut head = [0u8; 2];
    s.read_exact(&mut head).await.map_err(|_| End::Eof)?;
    let len = match head[1] & 0x7f {
        126 => s.read_u16().await.expect("len16") as usize,
        127 => s.read_u64().await.expect("len64") as usize,
        n => n as usize,
    };
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload).await.expect("a whole WS frame");
    match head[0] & 0x0f {
        0x8 => Err(End::WsClose(payload)),
        0x2 => Ok((
            u16::from_le_bytes([payload[4], payload[5]]),
            payload[6..].to_vec(),
        )),
        other => panic!("unexpected WS opcode {other:#x}"),
    }
}

async fn connect_ws(addr: SocketAddr) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.expect("ws tcp");
    let req = format!(
        "GET /gsb HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.expect("upgrade request");
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(s.read_u8().await.expect("upgrade response"));
    }
    assert!(head.starts_with(b"HTTP/1.1 101"), "the door upgrades");
    s
}
