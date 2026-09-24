//! Real-socket clients for the hosted-game suites (arena, MMO,
//! GAME-MODULE G2): a framed connection over plain TCP or TLS whose
//! reader half runs in its own task and feeds every frame into a bounded
//! channel, so a test can drain many clients without starving any
//! socket; plus a game `View` per client that applies the frames.
//!
//! Included by each hosted suite as a plain module next to `common`
//! (the runtime-minted TLS PKI). Not a test target itself.

#![allow(dead_code)] // each suite uses its own subset

#[cfg(feature = "game-arena")]
pub mod arena;
#[cfg(feature = "game-mmo")]
pub mod mmo;

mod drive;
#[allow(unused_imports)] // each suite uses its own subset
pub use drive::{config_file, eventually, hold};

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gsb_protocol::base::{Auth, AuthResult, Error, JoinRoom, JoinRoomResult};
use gsb_protocol::op::base as op;
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::common;

/// One frame: opcode + payload.
pub type Frame = (u16, Vec<u8>);

/// Which door a client walks in through.
#[derive(Clone)]
pub enum Door {
    Tcp,
    Tls(Arc<common::TlsPki>),
}

impl Door {
    /// A code-built server config for `game` on this door (ephemeral
    /// port, one room).
    pub fn config(&self, game: &str) -> gsb_server::Config {
        let mut cfg = gsb_server::Config {
            bind: "127.0.0.1:0".into(),
            room_count: 1,
            game: game.into(),
            ..Default::default()
        };
        if let Door::Tls(pki) = self {
            cfg.tls_cert = pki.cert_pem_path.clone();
            cfg.tls_key = pki.key_pem_path.clone();
        }
        cfg
    }
}

/// A byte stream a client can run over.
trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// How a game's client applies the frames it receives.
pub trait View: Default {
    /// Apply one game-band frame (base frames never reach it).
    fn apply(&mut self, op: u16, payload: &[u8]);
}

/// One connected client and its game view.
pub struct Client<V: View> {
    write: WriteHalf<Box<dyn Io>>,
    rx: mpsc::Receiver<Frame>,
    /// The reader task, which owns the read half: aborted on drop, so
    /// dropping a client really closes its socket.
    reader: tokio::task::JoinHandle<()>,
    /// Frames read during the handshake, applied first.
    pending: VecDeque<Frame>,
    /// The session's entity (wire id) from JOIN_ROOM_RESULT.
    pub entity: u64,
    /// The game view.
    pub view: V,
    /// The transport ended (EOF).
    pub closed: bool,
}

/// Read one length-prefixed frame; `None` on EOF or a malformed frame.
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Option<Frame> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.ok()?;
    let len = u32::from_le_bytes(len) as usize;
    if !(2..=4 * 1024 * 1024).contains(&len) {
        return None;
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.ok()?;
    Some((u16::from_le_bytes([body[0], body[1]]), body[2..].to_vec()))
}

impl<V: View> Client<V> {
    /// Connect through `door`, authenticate as `name` (the resume key)
    /// and join `room`; returns once JOIN_ROOM_RESULT arrived.
    pub async fn join(door: &Door, addr: SocketAddr, name: &str, room: u64) -> Self {
        let tcp = TcpStream::connect(addr).await.expect("connect");
        tcp.set_nodelay(true).ok();
        let io: Box<dyn Io> = match door {
            Door::Tcp => Box::new(tcp),
            Door::Tls(pki) => {
                let dns: rustls::pki_types::ServerName<'static> =
                    common::TLS_SERVER_NAME.try_into().expect("dns name");
                let tls = common::tls_client_connector(pki)
                    .connect(dns, tcp)
                    .await
                    .expect("tls handshake");
                Box::new(tls)
            }
        };
        let (mut read, write) = tokio::io::split(io);
        let (tx, rx) = mpsc::channel::<Frame>(8192);
        let reader = tokio::spawn(async move {
            while let Some(f) = read_frame(&mut read).await {
                if tx.send(f).await.is_err() {
                    return;
                }
            }
        });
        let mut c = Self {
            write,
            rx,
            reader,
            pending: VecDeque::new(),
            entity: 0,
            view: V::default(),
            closed: false,
        };
        let auth = Auth {
            name: name.into(),
            ticket: Vec::new(),
            protocol_version: gsb_protocol::PROTOCOL_VERSION,
        };
        c.send(op::AUTH_REQ, &auth.encode_to_vec()).await;
        c.send(
            op::JOIN_ROOM_REQ,
            &JoinRoom { room_id: room }.encode_to_vec(),
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("{name}: timed out waiting for the join"));
            let (op, payload) = tokio::time::timeout(left, c.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("{name}: timed out waiting for the join"))
                .unwrap_or_else(|| panic!("{name}: closed during the handshake"));
            match op {
                op::AUTH_RESULT => {
                    assert!(AuthResult::decode(&payload[..]).unwrap().ok, "{name}: auth");
                }
                op::JOIN_ROOM_RESULT => {
                    c.entity = JoinRoomResult::decode(&payload[..]).unwrap().entity;
                    assert_ne!(c.entity, 0, "{name}: a joined session has an entity");
                    return c;
                }
                op::ERROR => {
                    let e = Error::decode(&payload[..]).unwrap();
                    panic!("{name}: join failed: code={} {}", e.code, e.message);
                }
                _ => c.pending.push_back((op, payload)),
            }
        }
    }

    /// Send one frame.
    pub async fn send(&mut self, op: u16, payload: &[u8]) {
        let mut out = Vec::with_capacity(6 + payload.len());
        out.extend_from_slice(&((2 + payload.len()) as u32).to_le_bytes());
        out.extend_from_slice(&op.to_le_bytes());
        out.extend_from_slice(payload);
        self.write.write_all(&out).await.expect("write frame");
        self.write.flush().await.expect("flush");
    }

    /// Apply everything received so far to the view (never waits).
    pub fn drain(&mut self) {
        while let Some((op, payload)) = self.pending.pop_front() {
            self.apply(op, &payload);
        }
        loop {
            match self.rx.try_recv() {
                Ok((op, payload)) => self.apply(op, &payload),
                Err(mpsc::error::TryRecvError::Empty) => return,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.closed = true;
                    return;
                }
            }
        }
    }

    fn apply(&mut self, op: u16, payload: &[u8]) {
        if op == op::ERROR {
            let e = Error::decode(payload).unwrap();
            panic!("server error: code={} {}", e.code, e.message);
        }
        if op >= gsb_protocol::op::GAME_BAND_START {
            self.view.apply(op, payload);
        }
    }
}

impl<V: View> Drop for Client<V> {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
