//! The client side of the suite: one `gsb_client` connection on a door,
//! joined with the session steps, then driven by condition waits that
//! keep the session warm (a heartbeat every few hundred milliseconds —
//! the idle window must never be what a test measures by accident).

use std::time::{Duration, Instant};

use gsb_client::session::{self, Credentials};
use gsb_client::{ClientError, Conn, Recv, ServerError};
use gsb_demo::game::{MoveTo, WorldSnapshot};
use gsb_protocol::op::base as op;
use prost::Message;

use super::rig::{Door, GUARD};

/// How often a waiting player heartbeats.
const WARM: Duration = Duration::from_millis(300);

pub struct Player {
    conn: Conn,
    /// The session's entity (wire id), once joined.
    pub entity: u64,
    /// The next input sequence number (from 1 on every session).
    seq: u64,
    tick: u64,
    next_warm: Instant,
}

impl Player {
    /// Open a connection through `door` (rUDP: a fresh ephemeral local
    /// port and a full cookie handshake).
    pub async fn connect(door: Door, addr: std::net::SocketAddr) -> Self {
        let conn = match door {
            Door::Tcp => gsb_client::connect::tcp(addr).await.expect("tcp connect"),
            Door::Udp | Door::Migrating => gsb_client::connect::udp(addr)
                .await
                .expect("rUDP handshake"),
        };
        Self {
            conn,
            entity: 0,
            seq: 1,
            tick: 0,
            next_warm: Instant::now(),
        }
    }

    /// The local address this connection speaks from (rUDP), the half of
    /// the 4-tuple a NAT rebinding changes.
    pub fn udp_local(&self) -> Option<std::net::SocketAddr> {
        self.conn.udp_client().and_then(|c| c.local_addr())
    }

    /// Move the rUDP session to a new local socket (a network change,
    /// B3): the same session, no handshake. Returns the new address.
    pub async fn rebind(&mut self) -> std::net::SocketAddr {
        let c = self.conn.udp_client_mut().expect("an rUDP connection");
        assert!(c.migratable(), "the door granted a connection id");
        c.rebind().await.expect("rebind")
    }

    /// AUTH as `name` (the resume key) and JOIN room 1.
    pub async fn join(&mut self, name: &str) -> u64 {
        let creds = Credentials::named(name);
        let joined = session::auth_and_join(&mut self.conn, &creds, 1, GUARD, |_| {})
            .await
            .unwrap_or_else(|e| panic!("{name}: join: {e}"));
        assert_ne!(joined.entity, 0, "{name}: a joined session has an entity");
        self.entity = joined.entity;
        self.seq = 1;
        joined.entity
    }

    /// Heartbeat when due (keeps the idle window from firing).
    async fn warm(&mut self) {
        if Instant::now() >= self.next_warm {
            self.tick += 1;
            let hb = session::heartbeat(self.tick);
            self.conn.send(hb.op, &hb.payload).await.expect("heartbeat");
            self.next_warm = Instant::now() + WARM;
        }
    }

    /// Read (warm) until a world snapshot satisfies `done`; an `ERROR`
    /// or the end of the stream fails the test.
    async fn until_snapshot(&mut self, what: &str, done: impl Fn(&WorldSnapshot) -> bool) {
        let deadline = Instant::now() + GUARD;
        loop {
            assert!(Instant::now() < deadline, "timed out: {what}");
            self.warm().await;
            match self.conn.recv(Duration::from_millis(100)).await {
                Ok(Recv::Frame(f)) if f.op == gsb_demo::op::WORLD_SNAPSHOT => {
                    let snap = WorldSnapshot::decode(&f.payload[..]).expect("snapshot");
                    if done(&snap) {
                        return;
                    }
                }
                Ok(Recv::Frame(f)) if f.op == op::ERROR => {
                    panic!("{what}: {}", ServerError::decode_lossy(&f.payload))
                }
                Ok(Recv::Frame(_)) | Ok(Recv::Quiet) => {}
                Ok(Recv::Closed) => panic!("{what}: the stream ended"),
                Err(e) => panic!("{what}: {e}"),
            }
        }
    }

    /// Where this player's entity is in the next snapshot naming it: the
    /// session is in game and the world still holds the entity.
    pub async fn position(&mut self) -> (i32, i32) {
        let entity = self.entity;
        let at = std::cell::Cell::new(None);
        self.until_snapshot("a snapshot naming the own entity", |s| {
            let e = s.entities.iter().find(|e| e.entity == entity);
            at.set(e.map(|e| (e.x, e.y)));
            e.is_some()
        })
        .await;
        at.get().expect("found")
    }

    /// Send one move to a point four units from where the own entity
    /// is, and wait until it is seen exactly there: only this session's
    /// input can put it on that point (a move still under way from an
    /// earlier input would pass it at best, never stop on it).
    pub async fn moves(&mut self) {
        let (x, y) = self.position().await;
        let to = (if x < 40 { x + 4 } else { x - 4 }, y);
        let input = MoveTo {
            x: to.0,
            y: to.1,
            seq: self.seq,
        }
        .encode_to_vec();
        self.seq += 1;
        self.conn
            .send(gsb_demo::op::MOVE_TO, &input)
            .await
            .expect("move");
        let entity = self.entity;
        self.until_snapshot("the own entity to arrive", |s| {
            s.entities
                .iter()
                .any(|e| e.entity == entity && (e.x, e.y) == to)
        })
        .await;
    }

    /// Keep the session warm and its frames drained until `done` is set
    /// (by work running beside it on the same task: [`warm_during`]).
    pub async fn warm_until(&mut self, done: &std::cell::Cell<bool>) {
        while !done.get() {
            self.warm().await;
            match self.conn.recv(Duration::from_millis(50)).await {
                Ok(Recv::Frame(f)) if f.op == op::ERROR => {
                    panic!("kept warm: {}", ServerError::decode_lossy(&f.payload))
                }
                Ok(Recv::Frame(_)) | Ok(Recv::Quiet) => {}
                Ok(Recv::Closed) => panic!("kept warm: the stream ended"),
                Err(e) => panic!("kept warm: {e}"),
            }
        }
    }

    /// Wait (without sending: a superseded session is being closed) for
    /// the server's `ERROR` frame; returns it.
    pub async fn closed_by_server(&mut self) -> ServerError {
        let deadline = Instant::now() + GUARD;
        match session::reply(&mut self.conn, op::HEARTBEAT_ACK, deadline, &mut |_| {}).await {
            Err(ClientError::Server(e)) => e,
            other => panic!("want the server's close notice, got {other:?}"),
        }
    }
}

/// Run `work` while `p` is kept warm beside it: a joined player that
/// waits on the server's reports must not go idle meanwhile.
pub async fn warm_during<T>(p: &mut Player, work: impl std::future::Future<Output = T>) -> T {
    let done = std::cell::Cell::new(false);
    let (out, ()) = tokio::join!(
        async {
            let out = work.await;
            done.set(true);
            out
        },
        p.warm_until(&done)
    );
    out
}
