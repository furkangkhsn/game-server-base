//! A five-door test client for the close-notice suite: connect, write a
//! length-prefixed frame, and read what the server sends up to the END
//! of the session — reporting HOW it ended, which is what this suite is
//! about. Every door is a `gsb_client` connection (TCP, TLS, QUIC, rUDP,
//! and the WebSocket one — one binary message per frame, the door's
//! close frame surfaced by `Conn::ws_close`).

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::time::Duration;

use gsb_client::{Conn, Recv};
use gsb_server::ListenerTransport;
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

pub struct Client {
    conn: Conn,
    /// The WebSocket close frame was reported: a later end is the TCP
    /// end behind it, not a second close frame.
    ws_close_reported: bool,
}

pub async fn connect(door: ListenerTransport, pki: &common::TlsPki, addr: SocketAddr) -> Client {
    let conn = match door {
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
        ListenerTransport::Ws => gsb_client::connect::ws(addr)
            .await
            .expect("the door upgrades"),
    };
    Client {
        conn,
        ws_close_reported: false,
    }
}

impl Client {
    pub async fn write_frame(&mut self, op: u16, payload: &[u8]) {
        self.conn.send(op, payload).await.expect("write");
    }

    /// The byte sink under a stream door's frames — for bytes outside
    /// the frame contract (the `stream_rejected` suite's malformed
    /// prefix; the close-notice suite never writes one).
    #[allow(dead_code)]
    pub fn raw(&mut self) -> &mut gsb_client::conn::BoxWrite {
        match &mut self.conn {
            Conn::Stream { tx, .. } => tx.get_mut(),
            _ => panic!("not a stream door"),
        }
    }

    /// One WebSocket frame exactly as given (masked) — for messages
    /// outside the wire contract (the `stream_rejected` suite's text
    /// message; the close-notice suite never writes one).
    #[allow(dead_code)]
    pub async fn ws_frame(&mut self, fin: bool, opcode: u8, payload: &[u8]) {
        match &mut self.conn {
            Conn::Stream { tx, .. } => tx.ws_frame(fin, opcode, payload).await.expect("write"),
            _ => panic!("not a WebSocket door"),
        }
    }

    /// The next frame, or how the session ended (`Err`) — `Quiet` when
    /// nothing arrives within `window`.
    pub async fn next(&mut self, window: Duration) -> Result<(u16, Vec<u8>), End> {
        let c = &mut self.conn;
        match c.recv(window).await {
            Ok(Recv::Frame(f)) => Ok((f.op, f.payload.to_vec())),
            Ok(Recv::Closed) => match c.ws_close() {
                // The close frame, reported once, with its payload as
                // sent: the status code (none in an empty close), then
                // the reason.
                Some(close) if !self.ws_close_reported => {
                    self.ws_close_reported = true;
                    let mut payload = close
                        .code
                        .map_or_else(Vec::new, |code| code.to_be_bytes().to_vec());
                    payload.extend_from_slice(close.reason.as_bytes());
                    Err(End::WsClose(payload))
                }
                _ => Err(End::Eof),
            },
            Ok(Recv::Quiet) => Err(End::Quiet),
            Err(_) if c.is_udp() => Err(End::Quiet),
            // A stream ends at a frame boundary, never inside one (and a
            // WebSocket sends nothing after its close frame).
            Err(e) if matches!(e.kind(), ErrorKind::UnexpectedEof | ErrorKind::InvalidData) => {
                panic!("a whole frame: {e}")
            }
            // Any other read failure at a frame boundary is the end of
            // the stream.
            Err(_) => Err(End::Eof),
        }
    }
}
