//! Real-socket clients for the hosted-game suites (arena, MMO, war —
//! GAME-MODULE G2): a `gsb_client` connection over plain TCP or TLS,
//! joined with its session steps, whose reader half then runs in its own
//! task and feeds every frame into a bounded channel, so a test can
//! drain many clients without starving any socket; plus a game `View`
//! per client that applies the frames.
//!
//! Included by each hosted suite as a plain module next to `common`
//! (the runtime-minted TLS PKI). Not a test target itself.

#![allow(dead_code)] // each suite uses its own subset

#[cfg(feature = "game-arena")]
pub mod arena;
#[cfg(feature = "game-mmo")]
pub mod mmo;
#[cfg(feature = "game-war")]
pub mod war;

mod drive;
mod ops;
#[allow(unused_imports)] // each suite uses its own subset
pub use drive::{config_file, eventually, hold};
#[allow(unused_imports)] // each suite uses its own subset
pub use ops::{http, metric_sum, until_metric};

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_client::conn::BoxWrite;
use gsb_client::frame::FrameTx;
use gsb_client::session::{self, Credentials};
use gsb_client::{ClientError, ServerError};
use gsb_protocol::op::base as op;
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

/// How a game's client applies the frames it receives.
pub trait View: Default {
    /// Apply one game-band frame (base frames never reach it).
    fn apply(&mut self, op: u16, payload: &[u8]);
}

/// One connected client and its game view.
pub struct Client<V: View> {
    write: FrameTx<BoxWrite>,
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

impl<V: View> Client<V> {
    /// Connect through `door`, authenticate as `name` (the resume key)
    /// and join `room`; returns once JOIN_ROOM_RESULT arrived.
    pub async fn join(door: &Door, addr: SocketAddr, name: &str, room: u64) -> Self {
        Self::join_with_ticket(door, addr, name, &[], room).await
    }

    /// [`Self::join`] presenting `ticket` in AUTH (a ticket-auth server
    /// takes the identity from it; `name` is only what the client claims).
    pub async fn join_with_ticket(
        door: &Door,
        addr: SocketAddr,
        name: &str,
        ticket: &[u8],
        room: u64,
    ) -> Self {
        let tcp = TcpStream::connect(addr).await.expect("connect");
        let mut conn = match door {
            Door::Tcp => gsb_client::connect::tcp_stream(tcp),
            Door::Tls(pki) => {
                let dns: rustls::pki_types::ServerName<'static> =
                    common::TLS_SERVER_NAME.try_into().expect("dns name");
                gsb_client::tls::connect(tcp, &common::tls_client_connector(pki), dns)
                    .await
                    .expect("tls handshake")
            }
        };
        // The game frames that race the join result are kept, in order,
        // and applied first.
        let mut pending = VecDeque::new();
        let creds = Credentials::named(name).with_ticket(ticket);
        let joined =
            session::auth_and_join(&mut conn, &creds, room, Duration::from_secs(10), |f| {
                pending.push_back((f.op, f.payload.to_vec()))
            })
            .await;
        let entity = match joined {
            Ok(j) => j.entity,
            Err(ClientError::Server(e)) => {
                panic!("{name}: join failed: code={} {}", e.raw, e.message)
            }
            Err(ClientError::AuthRefused(r)) => panic!("{name}: auth: {r}"),
            Err(ClientError::TimedOut) => panic!("{name}: timed out waiting for the join"),
            Err(e) => panic!("{name}: closed during the handshake: {e}"),
        };
        assert_ne!(entity, 0, "{name}: a joined session has an entity");
        let Ok((mut read, write)) = conn.into_split() else {
            unreachable!("a stream door")
        };
        let (tx, rx) = mpsc::channel::<Frame>(8192);
        let reader = tokio::spawn(async move {
            while let Ok(Some(f)) = read.next().await {
                if tx.send((f.op, f.payload.to_vec())).await.is_err() {
                    return;
                }
            }
        });
        Self {
            write,
            rx,
            reader,
            pending,
            entity,
            view: V::default(),
            closed: false,
        }
    }

    /// Send one frame.
    pub async fn send(&mut self, op: u16, payload: &[u8]) {
        self.write.send(op, payload).await.expect("write frame");
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
            let e = ServerError::decode(payload).unwrap();
            panic!("server error: code={} {}", e.raw, e.message);
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
