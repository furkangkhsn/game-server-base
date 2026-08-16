//! The room actor: one per room/map, owning its world and its tick loop.
//!
//! A room runs the four-phase tick driven by the **global ticker** (a
//! `broadcast` channel, see [`crate::ticker`]):
//!
//! ```text
//! global ticker ──TickInfo (broadcast)──▶ room actor   (the only await)
//!                                            │
//!  Phase 0  │  CONTROL: pull join/leave/shutdown from the control channel
//!  Phase 1  │  READ:     pull actions from each connection's channel
//!  Phase 2  │  CONVERT:  actions → component writes (RoomLogic)
//!  Phase 3  │  SYSTEMS:  run the ordered game systems   (RoomLogic)
//!  Phase 4  │  BROADCAST: dirty entities → frames → conn channels
//!             └─────────────────────────────────────────────────────────┘
//! ```
//!
//! Everything is pulled with `try_recv` — the tick body is fully
//! synchronous. Per-connection action channels isolate users: one flooding
//! connection can only fill its own channel, never delay a tick or another
//! connection.
//!
//! Time handling: each room tracks the last tick it stepped at. The step
//! `dt` is the wall-clock difference, so ticks missed while busy are
//! absorbed into a single catch-up step and the simulation stays
//! frame-rate independent (the same real-time displacement at 15 Hz or
//! 100 Hz). `dt` is capped at `RoomConfig::max_catchup` periods so a
//! pathological stall produces temporary slow-motion instead of a giant
//! step. A room running slower than the global ticker simply steps on
//! every k-th global tick.
//!
//! The room actor owns **no** game types: the world is an opaque `W` and
//! all game behaviour is delegated to [`RoomLogic`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, warn};

use crate::channel::{FrameBatch, Inbox, Mailbox};
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::ticker::TickInfo;

/// A client action forwarded by the connection actor. The payload is still
/// encoded; the game crate decodes it against its own message types.
#[derive(Debug)]
pub struct Action {
    pub conn: ConnectionId,
    pub op: u16,
    pub payload: bytes::Bytes,
}

/// Static configuration for a room.
#[derive(Debug, Clone)]
pub struct RoomConfig {
    pub id: RoomId,
    /// Simulation rate in ticks per second. Must divide the global ticker
    /// rate: the room steps on every k-th global tick.
    pub tick_hz: f64,
    /// Capacity of the control channel (join/leave/shutdown).
    pub control_capacity: usize,
    /// Capacity of each connection's action channel.
    pub action_capacity: usize,
    /// High-water mark for buffered actions; beyond this, the *oldest*
    /// actions are dropped (a room behind real time must stay bounded).
    pub max_pending_actions: usize,
    /// Cap for catch-up `dt`, in periods: after a long stall the next step
    /// simulates at most this many periods (temporary slow-motion).
    pub max_catchup: u32,
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            id: RoomId(0),
            tick_hz: 30.0,
            control_capacity: 128,
            action_capacity: 256,
            max_pending_actions: 65536,
            max_catchup: 4,
        }
    }
}

impl RoomConfig {
    pub fn period(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.tick_hz)
    }
}

/// Control messages to a room. Low frequency; processed at the next tick
/// boundary (deterministic: joins and leaves take effect *on* a tick, never
/// mid-simulation; join/leave latency is at most one tick).
#[derive(Debug)]
pub enum RoomControl {
    /// A player joined: register its outbound channel, create its entity,
    /// and hand the connection actor the sender of the new per-connection
    /// action channel.
    Join {
        conn: ConnectionId,
        out: mpsc::Sender<FrameBatch>,
        reply: oneshot::Sender<(EntityId, Mailbox<Action>)>,
    },
    /// A player left: remove its entity and channel.
    ///
    /// Carries the entity this leave refers to: a *stale* leave (its
    /// connection re-joined in the meantime) is ignored, so it can never
    /// despawn the new entity.
    Leave {
        conn: ConnectionId,
        entity: EntityId,
    },
    /// Stop the room (drops the world).
    Shutdown,
}

/// Per-tick metadata handed to the game logic.
#[derive(Debug, Clone, Copy)]
pub struct TickCtx {
    pub room: RoomId,
    /// Global tick index (from the ticker; all rooms share one clock).
    pub tick: u64,
    /// Time since the previous step (covers any ticks missed in between).
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
    /// This connection's input, written by its connection actor as frames
    /// arrive; pulled non-blockingly at each step.
    actions: Inbox<Action>,
    entity: EntityId,
}

/// The room actor. Owns the world and the connection table; everything
/// mutable is local, so no synchronization is needed.
pub struct RoomActor<W> {
    config: RoomConfig,
    world: W,
    logic: Box<dyn RoomLogic<W>>,
    tick_rx: broadcast::Receiver<TickInfo>,
    control_rx: Inbox<RoomControl>,
    conns: HashMap<ConnectionId, RoomConn>,
    /// Number of global ticks between steps (1 = room rate == global rate).
    run_every: u64,
    /// Wall-clock instant of the last step (for dt).
    last_at: Option<Instant>,
    dropped_frames: u64,
}

impl<W> RoomActor<W> {
    pub fn new(
        config: RoomConfig,
        world: W,
        logic: Box<dyn RoomLogic<W>>,
        tick_rx: broadcast::Receiver<TickInfo>,
        control_rx: Inbox<RoomControl>,
        run_every: u64,
    ) -> Self {
        Self {
            config,
            world,
            logic,
            tick_rx,
            control_rx,
            conns: HashMap::new(),
            run_every: run_every.max(1),
            last_at: None,
            dropped_frames: 0,
        }
    }

    /// Run until the ticker channel closes or a control `Shutdown` is
    /// processed on a tick.
    pub async fn run(mut self) {
        debug!(
            room = %self.config.id,
            hz = self.config.tick_hz,
            run_every = self.run_every,
            "room actor started"
        );
        loop {
            let t = match self.tick_rx.recv().await {
                Ok(t) => t,
                // We fell behind by more than the broadcast buffer: skip
                // these; the wall-clock dt of the next step covers the gap
                // (bounded by the catch-up cap).
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    warn!(
                        room = %self.config.id,
                        missed,
                        "lagged behind global ticker; next step catches up via dt"
                    );
                    continue;
                }
                // Ticker aborted: global stop signal.
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if t.tick % self.run_every != 0 {
                continue; // this room runs slower: every k-th global tick
            }
            if !self.step(&t) {
                break;
            }
        }
        self.logic.on_shutdown();
        debug!(
            room = %self.config.id,
            dropped_frames = self.dropped_frames,
            "room actor stopped"
        );
    }

    /// One full step: control → read → convert → systems → broadcast.
    /// Synchronous. Returns `false` when the actor should stop.
    fn step(&mut self, t: &TickInfo) -> bool {
        // -- time: wall-clock since the last step; covers missed ticks
        //    (frame-rate independent), capped for pathological stalls.
        let dt = {
            let last = self.last_at.replace(t.at);
            match last {
                Some(last) => {
                    let elapsed = t.at.saturating_duration_since(last);
                    let cap = self.config.period() * self.config.max_catchup;
                    if elapsed > cap {
                        warn!(
                            room = %self.config.id,
                            ?elapsed,
                            ?cap,
                            "long stall: catch-up dt clamped (sim temporarily slower than real time)"
                        );
                        cap
                    } else {
                        elapsed
                    }
                }
                None => self.config.period(),
            }
        };
        let ctx = TickCtx {
            room: self.config.id,
            tick: t.tick,
            dt,
        };

        // -- Phase 0 — CONTROL (before actions: a fresh join's action
        //    channel is only registered once its Join has been processed).
        while let Ok(c) = self.control_rx.try_recv() {
            if !self.handle_control(c) {
                return false;
            }
        }

        // -- Phase 1 — READ: pull each connection's actions (non-blocking;
        //    per-connection isolation — one flooder only fills its own
        //    channel).
        let mut actions: Vec<Action> = Vec::new();
        for r in self.conns.values_mut() {
            while let Ok(a) = r.actions.try_recv() {
                actions.push(a);
            }
        }
        if actions.len() > self.config.max_pending_actions {
            let over = actions.len() - self.config.max_pending_actions;
            actions.drain(..over);
            warn!(
                room = %self.config.id,
                dropped = over,
                "action overflow; dropped oldest"
            );
        }

        // -- Phase 2 — CONVERT: actions → component writes (game logic).
        self.logic.ingest(&mut self.world, &ctx, &mut actions);

        // -- Phase 3 — SYSTEMS: run the ordered game systems.
        self.logic.update(&mut self.world, &ctx);

        // -- Phase 4 — BROADCAST: dirty entities → connection channels.
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
        true
    }

    fn handle_control(&mut self, c: RoomControl) -> bool {
        match c {
            RoomControl::Join { conn, out, reply } => {
                // A join supersedes any stale state this connection had
                // (e.g. a leave queued behind it in the control channel).
                if self.conns.remove(&conn).is_some() {
                    self.logic.on_leave(&mut self.world, conn);
                }
                let entity = self.logic.on_join(&mut self.world, conn);
                let (act_tx, act_rx) = mpsc::channel(self.config.action_capacity);
                self.conns.insert(
                    conn,
                    RoomConn {
                        out,
                        actions: act_rx,
                        entity,
                    },
                );
                let _ = reply.send((entity, act_tx));
                debug!(room = %self.config.id, %conn, entity, "player joined");
                true
            }
            RoomControl::Leave { conn, entity } => {
                // Stale-leave guard: only the entity this connection
                // currently owns.
                if self.conns.get(&conn).map(|c| c.entity) == Some(entity) {
                    self.conns.remove(&conn);
                    self.logic.on_leave(&mut self.world, conn);
                    debug!(room = %self.config.id, %conn, entity, "player left");
                }
                true
            }
            RoomControl::Shutdown => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::channel;
    use std::time::Duration;

    /// Test logic recording the dt of every step over a channel (no locks:
    /// this crate's lint forbids them even in tests).
    struct RecLogic {
        dts: mpsc::Sender<Duration>,
        ops: mpsc::Sender<u16>,
    }

    impl RoomLogic<()> for RecLogic {
        fn on_join(&mut self, _w: &mut (), _c: ConnectionId) -> EntityId {
            1
        }
        fn on_leave(&mut self, _w: &mut (), _c: ConnectionId) {}
        fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
            for a in actions.drain(..) {
                let _ = self.ops.try_send(a.op);
            }
        }
        fn update(&mut self, _w: &mut (), ctx: &TickCtx) {
            let _ = self.dts.try_send(ctx.dt);
        }
        fn broadcast(&mut self, _w: &mut (), _c: &TickCtx, _s: &mut OutSink<'_>) {}
    }

    struct Harness {
        tick_tx: broadcast::Sender<TickInfo>,
        control: Mailbox<RoomControl>,
        handle: tokio::task::JoinHandle<()>,
        t0: Instant,
        next_tick: u64,
        run_every: u64,
    }

    impl Harness {
        fn new(run_every: u64, config: RoomConfig, logic: RecLogic) -> Self {
            let (tick_tx, _first) = broadcast::channel(64);
            let tick_rx = tick_tx.subscribe();
            let (control, control_rx) = channel(config.control_capacity);
            let actor = RoomActor::new(config, (), Box::new(logic), tick_rx, control_rx, run_every);
            Self {
                tick_tx,
                control,
                handle: tokio::spawn(actor.run()),
                t0: Instant::now(),
                next_tick: 0,
                run_every: run_every.max(1),
            }
        }

        /// Send the next global tick with an exact synthetic timestamp:
        /// `at = t0 + n * period`, so dts are deterministic.
        fn tick(&mut self, period: Duration) {
            self.next_tick += 1;
            let at =
                self.t0 + Duration::from_secs_f64(self.next_tick as f64 * period.as_secs_f64());
            self.tick_tx
                .send(TickInfo {
                    tick: self.next_tick,
                    at,
                })
                .expect("room subscriber alive");
        }

        async fn join(
            &mut self,
            conn: ConnectionId,
            ticks_needed: u64,
        ) -> (EntityId, Mailbox<Action>) {
            let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
            let (reply_tx, reply_rx) = oneshot::channel::<(EntityId, Mailbox<Action>)>();
            self.control
                .send(RoomControl::Join {
                    conn,
                    out: out_tx,
                    reply: reply_tx,
                })
                .await
                .expect("control alive");
            // Control is processed on the room's next *step*; feed enough
            // ticks to guarantee one (run_every + slack).
            for _ in 0..ticks_needed {
                self.tick(Duration::from_secs_f64(1.0 / 30.0));
            }
            tokio::time::timeout(Duration::from_secs(2), reply_rx)
                .await
                .expect("timed out waiting for join reply")
                .expect("join reply dropped")
        }

        async fn shutdown(mut self) {
            self.control
                .send(RoomControl::Shutdown)
                .await
                .expect("control alive");
            // Control is processed on the room's next step: feed enough
            // ticks to guarantee one.
            for _ in 0..self.run_every {
                self.tick(Duration::from_secs_f64(1.0 / 30.0));
            }
            tokio::time::timeout(Duration::from_secs(2), &mut self.handle)
                .await
                .expect("room did not shut down")
                .expect("room task panicked");
        }
    }

    #[tokio::test]
    async fn room_steps_on_ticks_and_pulls_actions() {
        let (dt_tx, mut dts) = mpsc::channel(16);
        let (op_tx, mut ops) = mpsc::channel(16);
        let mut h = Harness::new(
            1,
            RoomConfig {
                id: RoomId(1),
                ..Default::default()
            },
            RecLogic {
                dts: dt_tx,
                ops: op_tx,
            },
        );
        let period = Duration::from_secs_f64(1.0 / 30.0);

        let (entity, actions) = h.join(ConnectionId(7), 1).await;
        assert_eq!(entity, 1);

        // Actions flow over the per-connection channel and are pulled at
        // the next step.
        actions
            .send(Action {
                conn: ConnectionId(7),
                op: 0x1001,
                payload: bytes::Bytes::new(),
            })
            .await
            .unwrap();
        h.tick(period);
        h.tick(period);

        // 3 steps so far (one from the join helper, two here): dts are the
        // exact nominal period (synthetic timestamps).
        for _ in 0..3 {
            let dt = tokio::time::timeout(Duration::from_secs(2), dts.recv())
                .await
                .expect("timed out")
                .expect("dts closed");
            assert!(
                dt.abs_diff(period) < Duration::from_micros(1),
                "dt {dt:?} != period {period:?}"
            );
        }
        let op = tokio::time::timeout(Duration::from_secs(2), ops.recv())
            .await
            .expect("timed out")
            .expect("ops closed");
        assert_eq!(op, 0x1001);

        h.shutdown().await;
    }

    #[tokio::test]
    async fn catchup_clamps_dt_after_long_gap() {
        let (dt_tx, mut dts) = mpsc::channel(16);
        let (op_tx, _ops) = mpsc::channel(16);
        let mut h = Harness::new(
            1,
            RoomConfig {
                id: RoomId(1),
                ..Default::default()
            }, // max_catchup = 4
            RecLogic {
                dts: dt_tx,
                ops: op_tx,
            },
        );
        let period = Duration::from_secs_f64(1.0 / 30.0);

        // Two normal steps, then a 2 s gap: the step's dt must be clamped
        // to 4 periods (frame-rate independence in steady state; bounded
        // slow-motion across the stall).
        h.tick(period);
        h.tick(period);
        h.next_tick += 1; // consume index 3 as "missed"
        let at = h.t0 + period * 4 + Duration::from_secs(2);
        h.tick_tx
            .send(TickInfo { tick: 4, at })
            .expect("subscriber alive");

        let first = dts.recv().await.expect("dts");
        let second = dts.recv().await.expect("dts");
        let third = dts.recv().await.expect("dts");
        assert!(first.abs_diff(period) < Duration::from_micros(1));
        assert!(second.abs_diff(period) < Duration::from_micros(1));
        assert!(
            third.abs_diff(period * 4) < Duration::from_micros(1),
            "clamped dt {third:?} != 4 * period {period:?}"
        );

        h.shutdown().await;
    }

    #[tokio::test]
    async fn slower_room_steps_on_every_kth_global_tick() {
        let (dt_tx, mut dts) = mpsc::channel(16);
        let (op_tx, _ops) = mpsc::channel(16);
        // Room at 15 Hz under a 60 Hz global ticker: run_every = 4.
        let mut h = Harness::new(
            4,
            RoomConfig {
                id: RoomId(1),
                tick_hz: 15.0,
                ..Default::default()
            },
            RecLogic {
                dts: dt_tx,
                ops: op_tx,
            },
        );
        let global_period = Duration::from_secs_f64(1.0 / 60.0);

        for _ in 0..8 {
            h.tick(global_period);
        }

        // Steps happened on ticks 4 and 8 only: 2 dts of 4 global periods.
        let room_period = Duration::from_secs_f64(1.0 / 15.0);
        for _ in 0..2 {
            let dt = tokio::time::timeout(Duration::from_secs(2), dts.recv())
                .await
                .expect("timed out")
                .expect("dts closed");
            assert!(
                dt.abs_diff(room_period) < Duration::from_micros(1),
                "dt {dt:?} != room period {room_period:?}"
            );
        }

        h.shutdown().await;
    }

    #[tokio::test]
    async fn lagged_receiver_catches_up_and_keeps_stepping() {
        let (dt_tx, mut dts) = mpsc::channel(16);
        let (op_tx, _ops) = mpsc::channel(16);
        // Buffer of 2: flooding it makes the receiver lag deterministically
        // *before* the room starts consuming.
        let (tick_tx, lagged_rx) = broadcast::channel(2);
        let (_control, control_rx) = channel(16);
        let t0 = Instant::now();
        let period = Duration::from_secs_f64(1.0 / 30.0);
        for i in 1..=10u64 {
            tick_tx
                .send(TickInfo {
                    tick: i,
                    at: t0 + Duration::from_secs_f64(i as f64 * period.as_secs_f64()),
                })
                .expect("channel open");
        }
        let actor = RoomActor::new(
            RoomConfig {
                id: RoomId(1),
                ..Default::default()
            },
            (),
            Box::new(RecLogic {
                dts: dt_tx,
                ops: op_tx,
            }),
            lagged_rx,
            control_rx,
            1,
        );
        let handle = tokio::spawn(actor.run());

        // The room skips the lagged ticks (Lagged → continue) and steps on
        // the two still-buffered ticks (9 and 10), then the sender is
        // dropped → Closed → clean exit.
        drop(tick_tx);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("room did not exit on closed ticker")
            .expect("room task panicked");
        let mut count = 0;
        while let Ok(dt) = dts.try_recv() {
            count += 1;
            assert!(dt <= period * 2);
        }
        assert_eq!(count, 2, "expected exactly the two buffered ticks");
    }

    #[tokio::test]
    async fn room_exits_when_ticker_closes() {
        let (dt_tx, _dts) = mpsc::channel(16);
        let (op_tx, _ops) = mpsc::channel(16);
        let (tick_tx, tick_rx) = broadcast::channel(4);
        let (_control, control_rx) = channel(16);
        let actor = RoomActor::new(
            RoomConfig {
                id: RoomId(1),
                ..Default::default()
            },
            (),
            Box::new(RecLogic {
                dts: dt_tx,
                ops: op_tx,
            }),
            tick_rx,
            control_rx,
            1,
        );
        let handle = tokio::spawn(actor.run());
        drop(tick_tx); // ticker aborted → broadcast closes
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("room did not exit on closed ticker")
            .expect("room task panicked");
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
