//! The room actor: one per room/map, owning its world and its tick loop.
//!
//! A room runs the four-phase tick described by the architecture:
//!
//! ```text
//! pacer task ──Tick──▶ room actor mailbox
//!                            │
//!             ┌──────────────┴─────────────────────────────────────────┐
//!  Phase 1  │  READ:     drain actions buffered since the last tick   │
//!  Phase 2  │  CONVERT:  actions → component writes  (RoomLogic)      │
//!  Phase 3  │  SYSTEMS:  run the ordered game systems    (RoomLogic)  │
//!  Phase 4  │  BROADCAST: dirty entities → frames → conn channels     │
//!             └────────────────────────────────────────────────────────┘
//! ```
//!
//! The room actor itself owns **no** game types: the world is an opaque
//! `W` and all game behaviour is delegated to [`RoomLogic`]. The whole tick
//! body is synchronous — the only `await` in the actor is the mailbox
//! receive.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::channel::{FrameBatch, Inbox};
use crate::id::{ConnectionId, EntityId, RoomId};

/// A client action forwarded by the connection actor. The payload is still
/// encoded; the game crate decodes it against its own message types.
#[derive(Debug)]
pub struct Action {
    pub conn: ConnectionId,
    pub op: u16,
    pub payload: Bytes,
}

/// Static configuration for a room.
#[derive(Debug, Clone)]
pub struct RoomConfig {
    pub id: RoomId,
    /// Simulation rate in ticks per second (default 30).
    pub tick_hz: f64,
    /// Capacity of the room mailbox (actions + control messages).
    pub mailbox_capacity: usize,
    /// High-water mark for buffered actions; beyond this, the *oldest*
    /// actions are dropped (a room behind real time must stay bounded).
    pub max_pending_actions: usize,
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            id: RoomId(0),
            tick_hz: 30.0,
            mailbox_capacity: 4096,
            max_pending_actions: 65536,
        }
    }
}

impl RoomConfig {
    pub fn period(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.tick_hz)
    }
}

/// Messages addressed to a room actor.
#[derive(Debug)]
pub enum RoomMsg {
    /// Tick trigger from the pacer.
    Tick,
    /// A client action to buffer for the next tick (phase 1).
    Action {
        conn: ConnectionId,
        op: u16,
        payload: Bytes,
    },
    /// A player joined: register its outbound channel and create its entity.
    PlayerJoined {
        conn: ConnectionId,
        out: mpsc::Sender<FrameBatch>,
        reply: tokio::sync::oneshot::Sender<EntityId>,
    },
    /// A player left: remove its entity and channel.
    ///
    /// Carries the entity this leave refers to: a *stale* leave (its
    /// connection re-joined in the meantime) is ignored, so it can never
    /// despawn the new entity.
    PlayerLeft {
        conn: ConnectionId,
        entity: EntityId,
    },
    /// Stop the room (drops the world, stops the pacer).
    Shutdown,
}

/// Per-tick metadata handed to the game logic.
#[derive(Debug, Clone, Copy)]
pub struct TickCtx {
    pub room: RoomId,
    /// Current tick (starts at 0, incremented after each run).
    pub tick: u64,
    /// Time since the previous tick.
    pub dt: Duration,
}

/// Fan-out sink used during the broadcast phase.
///
/// Frames are buffered per connection and flushed as **one batch per
/// connection per tick** (`Vec<FrameBody>` over the outbound channel) —
/// this is what keeps fan-out at O(connections) instead of
/// O(dirty × connections) at large scale.
///
/// Flushing is `try_send`: if a connection's outbound channel is full the
/// batch for that connection is dropped and counted. Snapshots are
/// self-contained per tick, so a dropped batch only costs one tick of
/// staleness for the affected client.
pub struct OutSink<'a> {
    conns: &'a HashMap<ConnectionId, RoomConn>,
    buffer: HashMap<ConnectionId, Vec<gsb_protocol::FrameBody>>,
    dropped: &'a mut u64,
}

impl OutSink<'_> {
    /// Queue a frame for one connection (no-op if it is not in the room).
    /// Flushed by [`OutSink::flush`] (called by the room after the
    /// broadcast phase).
    pub fn send(&mut self, conn: ConnectionId, frame: gsb_protocol::FrameBody) {
        if self.conns.contains_key(&conn) {
            self.buffer.entry(conn).or_default().push(frame);
        }
    }

    /// Queue a frame for every connection in the room.
    pub fn broadcast(&mut self, frame: gsb_protocol::FrameBody) {
        for conn in self.conns.keys() {
            self.buffer.entry(*conn).or_default().push(frame.clone());
        }
    }

    /// All connection ids currently in the room.
    pub fn connections(&self) -> impl Iterator<Item = ConnectionId> + '_ {
        self.conns.keys().copied()
    }

    pub fn count(&self) -> usize {
        self.conns.len()
    }

    /// Ship one batch to each queued connection.
    pub fn flush(&mut self) {
        for (conn, frames) in self.buffer.drain() {
            if let Some(c) = self.conns.get(&conn)
                && c.out.try_send(frames).is_err()
            {
                *self.dropped += 1;
            }
        }
    }
}

/// Game-side behaviour of a room. Implemented by the game crate; the core
/// never inspects the world `W`.
pub trait RoomLogic<W>: Send {
    /// A player entered the room: create (or restore) its entity and return
    /// its id.
    fn on_join(&mut self, world: &mut W, conn: ConnectionId) -> EntityId;

    /// A player left the room: remove its entity.
    fn on_leave(&mut self, world: &mut W, conn: ConnectionId);

    /// Phase 2 — convert buffered actions into component writes.
    fn ingest(&mut self, world: &mut W, ctx: &TickCtx, actions: &mut Vec<Action>);

    /// Phase 3 — run the game systems for this tick.
    fn update(&mut self, world: &mut W, ctx: &TickCtx);

    /// Phase 4 — encode dirty entities and fan them out via `sink`.
    fn broadcast(&mut self, world: &mut W, ctx: &TickCtx, sink: &mut OutSink<'_>);

    /// Called when the room shuts down (world is dropped right after).
    fn on_shutdown(&mut self) {}
}

struct RoomConn {
    out: mpsc::Sender<FrameBatch>,
    entity: EntityId,
}

/// The room actor. Owns the world and the connection table; everything
/// mutable is local, so no synchronization is needed.
pub struct RoomActor<W> {
    config: RoomConfig,
    world: W,
    logic: Box<dyn RoomLogic<W>>,
    inbox: Inbox<RoomMsg>,
    conns: HashMap<ConnectionId, RoomConn>,
    pending: Vec<Action>,
    tick: u64,
    last_tick: Instant,
    dropped_frames: u64,
}

impl<W> RoomActor<W> {
    pub fn new(
        config: RoomConfig,
        world: W,
        logic: Box<dyn RoomLogic<W>>,
        inbox: Inbox<RoomMsg>,
    ) -> Self {
        Self {
            config,
            world,
            logic,
            inbox,
            conns: HashMap::new(),
            pending: Vec::new(),
            tick: 0,
            last_tick: Instant::now(),
            dropped_frames: 0,
        }
    }

    /// Run until the mailbox is closed or [`RoomMsg::Shutdown`] arrives.
    pub async fn run(mut self) {
        debug!(room = %self.config.id, "room actor started");
        while let Some(msg) = self.inbox.recv().await {
            if !self.handle(msg) {
                break;
            }
        }
        self.logic.on_shutdown();
        debug!(
            room = %self.config.id,
            ticks = self.tick,
            dropped_frames = self.dropped_frames,
            "room actor stopped"
        );
    }

    /// Handle one mailbox message. Returns `false` when the actor should
    /// stop ([`RoomMsg::Shutdown`]).
    fn handle(&mut self, msg: RoomMsg) -> bool {
        match msg {
            RoomMsg::Tick => {
                self.tick_once();
                // Coalesce: while this tick ran, the pacer may have queued
                // more ticks. Drain the mailbox in order — pure ticks are
                // dropped (the tick we just ran covered the elapsed time
                // via its dt) while any other message is processed
                // immediately so mailbox ordering is preserved.
                while let Ok(queued) = self.inbox.try_recv() {
                    if !matches!(queued, RoomMsg::Tick) && !self.handle(queued) {
                        return false;
                    }
                }
                true
            }
            RoomMsg::Action { conn, op, payload } => {
                self.pending.push(Action { conn, op, payload });
                if self.pending.len() > self.config.max_pending_actions {
                    let over = self.pending.len() - self.config.max_pending_actions;
                    self.pending.drain(..over);
                    warn!(
                        room = %self.config.id,
                        dropped = over, "pending action overflow; dropped oldest"
                    );
                }
                true
            }
            RoomMsg::PlayerJoined { conn, out, reply } => {
                // A join replaces any stale state this connection had in the
                // room (e.g. a leave that has not been processed yet).
                if self.conns.contains_key(&conn) {
                    self.logic.on_leave(&mut self.world, conn);
                }
                let entity = self.logic.on_join(&mut self.world, conn);
                self.conns.insert(conn, RoomConn { out, entity });
                debug!(room = %self.config.id, %conn, "player joined");
                let _ = reply.send(entity);
                true
            }
            RoomMsg::PlayerLeft { conn, entity } => {
                match self.conns.get(&conn) {
                    Some(gone) if gone.entity == entity => {
                        self.conns.remove(&conn);
                        self.logic.on_leave(&mut self.world, conn);
                        debug!(room = %self.config.id, %conn, %entity, "player left");
                    }
                    // Unknown connection or stale leave (its connection
                    // re-joined since): ignore — the current entity stays.
                    _ => {}
                }
                true
            }
            RoomMsg::Shutdown => false,
        }
    }

    /// One full tick: read → convert → systems → broadcast. Synchronous.
    fn tick_once(&mut self) {
        // Phase 1 — READ: drain everything buffered since the last tick.
        let mut actions = std::mem::take(&mut self.pending);

        let now = Instant::now();
        let dt = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;

        let ctx = TickCtx {
            room: self.config.id,
            tick: self.tick,
            dt,
        };

        // Phase 2 — CONVERT: actions → component writes (game logic).
        self.logic.ingest(&mut self.world, &ctx, &mut actions);

        // Phase 3 — SYSTEMS: run the ordered game systems.
        self.logic.update(&mut self.world, &ctx);

        // Phase 4 — BROADCAST: dirty entities → connection channels.
        // `OutSink` borrows `self.conns` immutably while the logic borrows
        // `self.world` mutably — disjoint fields, no synchronization.
        let mut dropped: u64 = 0;
        {
            let mut sink = OutSink {
                conns: &self.conns,
                buffer: HashMap::new(),
                dropped: &mut dropped,
            };
            self.logic.broadcast(&mut self.world, &ctx, &mut sink);
            sink.flush();
        }
        self.dropped_frames += dropped;

        self.tick += 1;
    }
}

/// Spawn the pacer task for a room: a dedicated tokio task that fires
/// [`RoomMsg::Tick`] at a fixed rate. Drift-corrected: it anchors on the
/// nominal schedule and skips ticks when the wall clock has outrun it.
///
/// Keeping pacing in a separate task means the room actor never sleeps,
/// never polls a timer, and never multiplexes — its only `await` is the
/// mailbox receive.
pub fn spawn_pacer(mailbox: crate::channel::Mailbox<RoomMsg>, period: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut next = Instant::now() + period;
        loop {
            let delay = next.saturating_duration_since(Instant::now());
            tokio::time::sleep(delay).await;
            if mailbox.send(RoomMsg::Tick).await.is_err() {
                break; // room mailbox closed
            }
            next += period;
            if next < Instant::now() {
                // Fell behind (e.g. a long tick); resync to wall clock.
                next = Instant::now() + period;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::channel;

    struct NullLogic;
    impl RoomLogic<()> for NullLogic {
        fn on_join(&mut self, _w: &mut (), _c: ConnectionId) -> EntityId {
            1
        }
        fn on_leave(&mut self, _w: &mut (), _c: ConnectionId) {}
        fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
            a.clear();
        }
        fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
        fn broadcast(&mut self, _w: &mut (), _c: &TickCtx, _s: &mut OutSink<'_>) {}
    }

    #[tokio::test]
    async fn room_processes_ticks_and_players() {
        let config = RoomConfig {
            id: RoomId(1),
            ..Default::default()
        };
        let (tx, rx) = channel(config.mailbox_capacity);
        let actor = RoomActor::new(config, (), Box::new(NullLogic), rx);

        tx.send(RoomMsg::PlayerJoined {
            conn: ConnectionId(7),
            out: mpsc::channel(1).0,
            reply: {
                let (s, r) = tokio::sync::oneshot::channel();
                drop(r); // reply is intentionally ignored in this test
                s
            },
        })
        .await
        .unwrap();
        tx.send(RoomMsg::Tick).await.unwrap();
        tx.send(RoomMsg::Tick).await.unwrap();

        let pacer = spawn_pacer(tx.clone(), Duration::from_millis(50));
        tokio::time::sleep(Duration::from_millis(160)).await;
        tx.send(RoomMsg::Shutdown).await.unwrap();

        actor.run().await;
        pacer.abort();
    }

    #[test]
    fn config_period() {
        let c = RoomConfig {
            tick_hz: 30.0,
            ..Default::default()
        };
        assert!((c.period().as_secs_f64() - 1.0 / 30.0).abs() < 1e-9);
    }
}
