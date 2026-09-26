//! A five-door test client for the close-notice suite: connect, write a
//! length-prefixed frame, and read what the server sends up to the END
//! of the session — reporting HOW it ended, which is what this suite is
//! about. Trimmed from `multi_listener.rs` (the same wire shapes: the
//! stream doors' `[u32 LE len][u16 op][payload]`, one binary WS message
//! per frame, one QUIC bi-stream, the rUDP client from `gsb_net`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_server::ListenerTransport;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
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
    Tcp(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    Udp(Box<gsb_net::udp::UdpClient>),
    Quic(quinn::SendStream, quinn::RecvStream),
    Ws(TcpStream),
}

fn framed(op: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = ((2 + payload.len()) as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub async fn connect(door: ListenerTransport, pki: &common::TlsPki, addr: SocketAddr) -> Client {
    match door {
        ListenerTransport::Tcp => Client::Tcp(TcpStream::connect(addr).await.expect("tcp")),
        ListenerTransport::Tls => {
            let tcp = TcpStream::connect(addr).await.expect("tcp under tls");
            let dns: rustls::pki_types::ServerName<'static> =
                TLS_SERVER_NAME.try_into().expect("dns name");
            let tls = common::tls_client_connector(pki)
                .connect(dns, tcp)
                .await
                .expect("TLS handshake");
            Client::Tls(Box::new(tls))
        }
        ListenerTransport::Udp => Client::Udp(Box::new(
            gsb_net::udp::UdpClient::connect(addr)
                .await
                .expect("rUDP handshake"),
        )),
        ListenerTransport::Quic => {
            let (send, recv) = connect_quic(pki, addr).await;
            Client::Quic(send, recv)
        }
        ListenerTransport::Ws => Client::Ws(connect_ws(addr).await),
    }
}

impl Client {
    pub async fn write_frame(&mut self, op: u16, payload: &[u8]) {
        let bytes = framed(op, payload);
        match self {
            Client::Tcp(s) => s.write_all(&bytes).await.expect("write"),
            Client::Tls(t) => {
                t.write_all(&bytes).await.expect("write");
                t.flush().await.expect("flush");
            }
            Client::Udp(c) => c.send_frame(op, payload.to_vec()).await.expect("send"),
            Client::Quic(send, _) => send.write_all(&bytes).await.expect("write"),
            Client::Ws(s) => {
                // One masked FIN binary message carrying one game frame.
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

    /// The next frame, or how the session ended (`Err`) — `Quiet` when
    /// nothing arrives within `window`.
    pub async fn next(&mut self, window: Duration) -> Result<(u16, Vec<u8>), End> {
        let read = async {
            match self {
                Client::Tcp(s) => stream_frame(s).await,
                Client::Tls(t) => stream_frame(t.as_mut()).await,
                Client::Quic(_, recv) => stream_frame(recv).await,
                Client::Ws(s) => ws_frame(s).await,
                Client::Udp(c) => match c.recv_frame(window).await {
                    Ok(Some(f)) => Ok((f.op, f.payload.to_vec())),
                    _ => Err(End::Quiet),
                },
            }
        };
        tokio::time::timeout(window, read)
            .await
            .unwrap_or(Err(End::Quiet))
    }
}

/// One length-prefixed frame off a stream door; any read failure at a
/// frame boundary is the end of the stream.
async fn stream_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<(u16, Vec<u8>), End> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.map_err(|_| End::Eof)?;
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut body).await.expect("a whole frame");
    Ok((u16::from_le_bytes([body[0], body[1]]), body[2..].to_vec()))
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

async fn connect_quic(
    pki: &common::TlsPki,
    addr: SocketAddr,
) -> (quinn::SendStream, quinn::RecvStream) {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(pki.ca_der.clone()).expect("CA parses");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![gsb_net::quic::ALPN_PROTOCOL.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC TLS");
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("endpoint");
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
    let conn = endpoint
        .connect(addr, TLS_SERVER_NAME)
        .expect("connect setup")
        .await
        .expect("QUIC handshake");
    conn.open_bi().await.expect("bi-stream")
}
