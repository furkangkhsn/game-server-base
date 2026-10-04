//! A minimal room actor with the s0b phase order (SERVICES.md §3):
//!
//! ```text
//! CONTROL → RESPONSES → READ → (EVENTS, later) → CONVERT → SYSTEMS → BROADCAST
//! ```
//!
//! (CONVERT is the game's own first systems here: it takes the tick's
//! requests from the table.) The room is the determinism boundary:
//! everything that ENTERS a tick — control, responses, reads, dt — can be
//! recorded in canonical order and fed back in REPLAY mode, where no
//! service is called.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::calls::{Calls, CallsConfig, Counters, Recorded};
use crate::contract::{ActorId, ConnId, Identity, Owner, mix};
use crate::sim::Sim;
use crate::wire::{RpcRequest, RpcResponse};

/// The fixed step of test mode (and of the spike's scripted runs).
pub const FIXED_DT: f32 = 1.0 / 30.0;

/// The room's seeded generator (SplitMix64). The only randomness logic
/// may use: its seed is in the record, so a replay draws the same values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix(self.0)
    }
    /// Uniform-ish in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

/// What every system may read: the tick, the room's clock, the room's
/// rng. No wall clock anywhere in logic.
#[derive(Debug, Clone)]
pub struct TickCtx {
    tick: u64,
    time: f64,
    dt: f32,
    rng: Rng,
}

impl TickCtx {
    pub fn new(seed: u64) -> Self {
        Self {
            tick: 0,
            time: 0.0,
            dt: 0.0,
            rng: Rng::new(seed),
        }
    }
    pub fn tick(&self) -> u64 {
        self.tick
    }
    /// Room time: the sum of recorded `dt`s.
    pub fn time(&self) -> f64 {
        self.time
    }
    pub fn dt(&self) -> f32 {
        self.dt
    }
    pub fn rng(&mut self) -> &mut Rng {
        &mut self.rng
    }
}

/// The per-tick state hash the game fills (FNV-1a 64: stable, simple).
pub struct StateHasher(u64);

impl Default for StateHasher {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl StateHasher {
    pub fn bytes(&mut self, b: &[u8]) {
        for x in b {
            self.0 ^= u64::from(*x);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    pub fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }
    pub fn str(&mut self, s: &str) {
        self.u64(s.len() as u64);
        self.bytes(s.as_bytes());
    }
    pub fn finish(&self) -> u64 {
        self.0
    }
}

/// CONTROL input (joins and leaves).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    Join { conn: ConnId, identity: Identity },
    Leave { conn: ConnId },
}

/// Where a game keeps its world and its table. The engine ships three
/// (bevy, hecs, plain); a game implements the small per-flavour trait.
pub trait RoomLogic {
    /// Lend the table, and an "is this owner alive" test, to the engine.
    fn with_calls<T>(&mut self, f: impl FnOnce(&mut Calls, &dyn Fn(Owner) -> bool) -> T) -> T;
    fn control(&mut self, c: &Control, ctx: &mut TickCtx);
    fn systems(&mut self, ctx: &mut TickCtx);
    fn state_hash(&mut self, h: &mut StateHasher);
}

/// Everything that entered one tick, in canonical order.
pub struct TickInput {
    pub dt: f32,
    pub control: Vec<Control>,
    pub responses: Vec<Recorded>,
    pub reads: Vec<(ConnId, RpcRequest)>,
}

/// A room's input log (opt-in). In memory in the spike; the engine's
/// would be a file (open question: the on-disk form of in-process
/// outcomes).
pub struct InputLog {
    pub room: u64,
    pub seed: u64,
    pub ticks: Vec<TickInput>,
    /// The recorded run's per-tick hashes (to compare a replay with).
    pub hashes: Vec<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct RoomConfig {
    pub id: u64,
    pub seed: u64,
    /// In-flight share per service.
    pub share: u32,
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            id: 1,
            seed: 7,
            share: 64,
        }
    }
}

/// Senders into a room (a connection actor's view).
#[derive(Clone)]
pub struct Inputs {
    control: mpsc::Sender<Control>,
    requests: mpsc::Sender<(ConnId, RpcRequest)>,
}

impl Inputs {
    pub fn join(&self, conn: ConnId, who: &str) {
        let _ = self.control.try_send(Control::Join {
            conn,
            identity: Identity::new(who),
        });
    }
    pub fn leave(&self, conn: ConnId) {
        let _ = self.control.try_send(Control::Leave { conn });
    }
    pub fn request(&self, conn: ConnId, req: RpcRequest) {
        let _ = self.requests.try_send((conn, req));
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RoomStats {
    pub ticks: u64,
    pub read_polls: u64,
    pub requests_unknown_conn: u64,
}

/// The room actor (synchronous tick; everything pulled with `try_recv`).
pub struct Room<L> {
    cfg: RoomConfig,
    logic: L,
    ctx: TickCtx,
    members: BTreeMap<ConnId, Identity>,
    inputs: Inputs,
    control_rx: mpsc::Receiver<Control>,
    requests_rx: mpsc::Receiver<(ConnId, RpcRequest)>,
    record: Option<InputLog>,
    replay: Option<Arc<InputLog>>,
    hashing: bool,
    pub hashes: Vec<u64>,
    pub stats: RoomStats,
}

impl<L: RoomLogic> Room<L> {
    /// A live room. `build` gets the room's table (register `accept`s and
    /// `forward`s there) and returns the game.
    pub fn new(cfg: RoomConfig, build: impl FnOnce(Calls) -> L) -> Self {
        let calls = Calls::new(CallsConfig {
            actor: ActorId(cfg.id),
            origin: mix(cfg.seed ^ mix(cfg.id)),
            share: cfg.share,
            completions: 1024,
        });
        let (ctx_tx, control_rx) = mpsc::channel(1024);
        let (req_tx, requests_rx) = mpsc::channel(1024);
        Self {
            cfg,
            logic: build(calls),
            ctx: TickCtx::new(cfg.seed),
            members: BTreeMap::new(),
            inputs: Inputs {
                control: ctx_tx,
                requests: req_tx,
            },
            control_rx,
            requests_rx,
            record: None,
            replay: None,
            hashing: false,
            hashes: Vec::new(),
            stats: RoomStats::default(),
        }
    }

    /// A replaying room: same config as the recording, inputs from the
    /// log, services never called.
    pub fn replay(log: Arc<InputLog>, share: u32, build: impl FnOnce(Calls) -> L) -> Self {
        let cfg = RoomConfig {
            id: log.room,
            seed: log.seed,
            share,
        };
        let mut room = Self::new(cfg, build);
        room.logic.with_calls(|c, _| c.set_replay(true));
        room.replay = Some(log);
        room.hashing = true;
        room
    }

    /// Record every tick's input (and hash) from now on.
    pub fn record(&mut self) {
        self.logic.with_calls(|c, _| c.set_recording(true));
        self.hashing = true;
        self.record = Some(InputLog {
            room: self.cfg.id,
            seed: self.cfg.seed,
            ticks: Vec::new(),
            hashes: Vec::new(),
        });
    }

    /// Fill `hashes` every tick (test mode comparisons).
    pub fn hash_every_tick(&mut self) {
        self.hashing = true;
    }

    pub fn take_log(&mut self) -> Option<InputLog> {
        self.logic.with_calls(|c, _| c.set_recording(false));
        self.record.take()
    }

    pub fn inputs(&self) -> Inputs {
        self.inputs.clone()
    }

    pub fn logic(&self) -> &L {
        &self.logic
    }

    pub fn logic_mut(&mut self) -> &mut L {
        &mut self.logic
    }

    pub fn counters(&mut self) -> Counters {
        self.logic.with_calls(|c, _| c.counters)
    }

    pub fn tick_no(&self) -> u64 {
        self.stats.ticks
    }

    /// Ticks left in the replay log.
    pub fn replay_left(&self) -> usize {
        match &self.replay {
            Some(log) => log.ticks.len().saturating_sub(self.stats.ticks as usize),
            None => 0,
        }
    }

    /// One tick. Returns BROADCAST's client answers.
    pub fn tick(&mut self, dt: f32) -> Vec<(ConnId, RpcResponse)> {
        let t = self.stats.ticks;
        let log = self.replay.clone();
        let input = log.as_ref().map(|l| &l.ticks[t as usize]);

        // Gather this tick's inputs (live: pulled; replay: recorded).
        let dt = input.map_or(dt, |i| i.dt);
        let control: Vec<Control> = match input {
            Some(i) => i.control.clone(),
            None => {
                let mut v = Vec::new();
                while let Ok(c) = self.control_rx.try_recv() {
                    v.push(c);
                }
                v
            }
        };
        self.ctx.tick = t;
        self.ctx.dt = dt;
        self.ctx.time += f64::from(dt);

        // CONTROL
        for c in &control {
            match c {
                Control::Join { conn, identity } => {
                    self.members.insert(*conn, identity.clone());
                }
                Control::Leave { conn } => {
                    self.members.remove(conn);
                }
            }
            self.logic.control(c, &mut self.ctx);
        }

        // RESPONSES
        let replay_resp = input.map(|i| i.responses.as_slice());
        self.logic
            .with_calls(|calls, alive| calls.responses(replay_resp, alive));

        // READ
        let reads: Vec<(ConnId, RpcRequest)> = match input {
            Some(i) => i.reads.clone(),
            None => {
                let mut v = Vec::new();
                self.stats.read_polls += 1;
                while let Ok(r) = self.requests_rx.try_recv() {
                    v.push(r);
                    self.stats.read_polls += 1;
                }
                v
            }
        };
        if !reads.is_empty() {
            let members = &self.members;
            let stats = &mut self.stats;
            self.logic.with_calls(|calls, _| {
                for (conn, req) in &reads {
                    match members.get(conn) {
                        Some(who) => calls.read(*conn, who, req.clone()),
                        None => stats.requests_unknown_conn += 1,
                    }
                }
            });
        }

        // (EVENTS: next round.) CONVERT + SYSTEMS
        self.logic.systems(&mut self.ctx);

        // Calls issued this tick leave now, in issue order.
        self.logic.with_calls(|calls, _| calls.flush());

        // BROADCAST
        let out = self.logic.with_calls(|calls, _| calls.end_tick());

        if self.hashing {
            let mut h = StateHasher::default();
            self.logic.state_hash(&mut h);
            h.u64(self.ctx.rng.0);
            h.u64(self.ctx.time.to_bits());
            self.hashes.push(h.finish());
        }
        if let Some(rec) = &mut self.record {
            let responses = self.logic.with_calls(|c, _| c.take_recorded());
            rec.ticks.push(TickInput {
                dt,
                control,
                responses,
                reads,
            });
            if let Some(h) = self.hashes.last() {
                rec.hashes.push(*h);
            }
        }
        self.stats.ticks += 1;
        out
    }

    /// Test mode: one fixed-dt tick, then the services run.
    pub fn tick_sim(&mut self, sim: &mut Sim) -> Vec<(ConnId, RpcResponse)> {
        let out = self.tick(FIXED_DT);
        sim.step(self.stats.ticks - 1);
        out
    }

    /// Replay: run every recorded tick.
    pub fn replay_all(&mut self) -> Vec<(ConnId, RpcResponse)> {
        let mut out = Vec::new();
        while self.replay_left() > 0 {
            out.extend(self.tick(0.0));
        }
        out
    }
}
