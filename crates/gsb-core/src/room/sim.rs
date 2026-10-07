//! TEST mode's room (`docs/SERVICES.md` §10.3): a REAL room actor,
//! stepped by the test at a fixed `dt` (one period per step, exactly),
//! seeded, its state hashed every step, with scripted clients — and
//! [`Sim`](crate::determinism::Sim)'s fake services stepped right after
//! each tick. No runtime, no ticker, no registry: the same input gives
//! the same ticks, the same answers and the same hashes, every run.
//! ECS-agnostic: any `W` and any [`RoomLogic`].
//!
//! ```ignore
//! let mut sim = Sim::new();
//! let bank = sim.service("bank", 64, Delay::ticks(3, fake_bank));
//! let mut room = SimRoom::new(RoomConfig { seed: Some(1), ..Default::default() }, (), Box::new(MyGame::new(bank)));
//! let alice = room.join(1, "alice");
//! alice.request(10, &Transfer { .. });
//! for _ in 0..10 { room.tick(&mut sim); }
//! ```

use std::fmt::Debug;
use std::hash::Hash;
use std::time::{Duration, Instant};

use bytes::Bytes;
use prost::Message;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::calls::Request;
use crate::channel::{FrameBatch, Mailbox};
use crate::determinism::Sim;
use crate::id::{ConnectionId, EntityId, PlayerId};
use crate::metrics::CallCounts;
use crate::room::actor::RoomActor;
use crate::room::{Action, RoomConfig, RoomControl, RoomLogic};
use crate::ticker::TickInfo;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// The step's state hash: the logic's ([`GameLogic::state_hash`](crate::room::GameLogic::state_hash))
    /// folded with the generator's state and the room clock; `None` when
    /// the logic hashes nothing.
    pub(in crate::room) fn state_hash(&mut self) -> Option<u64> {
        let game = self.logic.state_hash(&mut self.world);
        crate::determinism::fold(game, self.rng.state(), self.clock)
    }
}

/// A room in TEST mode (see the module docs).
pub struct SimRoom<W, G = (), Sp = ()> {
    actor: RoomActor<W, G, Sp>,
    t0: Instant,
    tick: u64,
    hashes: Vec<Option<u64>>,
}

/// A scripted client of a [`SimRoom`]: what it sends goes into its
/// member channel (read at the next step's READ), what the room ships it
/// waits in its outbound channel ([`Self::frames`]).
pub struct SimClient {
    /// The client's transport session.
    pub conn: ConnectionId,
    /// The entity its join was answered with.
    pub entity: EntityId,
    /// Its identity (the resume key).
    pub identity: String,
    actions: Mailbox<Action>,
    out: mpsc::Receiver<FrameBatch>,
}

impl SimClient {
    /// Send one correlated request (`id != 0`) of a typed request.
    pub fn request<R: Request>(&self, id: u64, body: &R) {
        self.raw_request(id, R::OP, body.to_wire());
    }

    /// Send one correlated request with a raw payload.
    pub fn raw_request(&self, id: u64, op: u16, payload: Vec<u8>) {
        let env = gsb_protocol::base::RpcRequest {
            id,
            op: u32::from(op),
            payload,
        };
        self.action(crate::rpc::RPC_REQ_OP, env.encode_to_vec().into());
    }

    /// Send one fire-and-forget action. Panics when the client's channel
    /// is full (a script that outruns its room's per-tick budget).
    pub fn action(&self, op: u16, payload: Bytes) {
        self.actions
            .try_send(Action {
                conn: self.conn,
                player: PlayerId(0),
                op,
                payload,
            })
            .expect("the member channel has room");
    }

    /// Every frame the room shipped this client since the last call, in
    /// order (decode the answers with the game's own private format).
    pub fn frames(&mut self) -> Vec<gsb_protocol::FrameBody> {
        let mut out = Vec::new();
        while let Ok(batch) = self.out.try_recv() {
            out.extend(batch);
        }
        out
    }
}

impl<W, G, Sp> SimRoom<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// A room over `world` and `logic`. TEST mode is seeded: a config
    /// without a seed gets seed 0.
    pub fn new(
        mut config: RoomConfig,
        world: W,
        logic: Box<dyn RoomLogic<W, GroupKey = G, Strip = Sp>>,
    ) -> Self {
        config.seed.get_or_insert(0);
        let (_tick_tx, tick_rx) = broadcast::channel(1);
        let (_control, control_rx) = crate::channel::channel(1);
        let (metrics, _gone) = mpsc::channel(1);
        let actor = RoomActor::new(config, world, logic, tick_rx, control_rx, 1, metrics, None);
        Self {
            actor,
            t0: Instant::now(),
            tick: 0,
            hashes: Vec::new(),
        }
    }

    /// Record the room's input from now on (feature `record`; see
    /// [`RoomActor::with_record`]).
    #[cfg(feature = "record")]
    pub fn record(&mut self, recorder: crate::determinism::Recorder) {
        self.actor.start_record(recorder);
    }

    /// Join `conn` as `identity` (an identified join; processed at once,
    /// as the next step's CONTROL would).
    pub fn join(&mut self, conn: u64, identity: &str) -> SimClient {
        let (out, rx) = mpsc::channel::<FrameBatch>(1024);
        let (reply, mut replied) = oneshot::channel();
        self.actor.handle_control(RoomControl::Resume {
            conn: ConnectionId(conn),
            epoch: 0,
            identity: identity.to_owned(),
            claims: None,
            out,
            reply,
        });
        let (entity, actions) = replied
            .try_recv()
            .expect("a join is answered at once")
            .expect("join accepted");
        SimClient {
            conn: ConnectionId(conn),
            entity,
            identity: identity.to_owned(),
            actions,
            out: rx,
        }
    }

    /// The client leaves (processed at once).
    pub fn leave(&mut self, client: &SimClient) {
        self.actor.handle_control(RoomControl::Leave {
            conn: client.conn,
            entity: client.entity,
        });
    }

    /// The client's transport dies (the logic's detach policy decides).
    pub fn disconnect(&mut self, client: &SimClient) {
        self.actor.handle_control(RoomControl::Detach {
            conn: client.conn,
            entity: client.entity,
            identity: client.identity.clone(),
        });
    }

    /// One fixed-`dt` step (`dt` = the room's period, exactly), its hash
    /// kept, then the services run ([`Sim::step`]: answers due land at
    /// the next step's RESPONSES). `false` when the room stopped.
    pub fn tick(&mut self, sim: &mut Sim) -> bool {
        let period = self.actor.config.period().as_nanos() as u64;
        let at = self.t0 + Duration::from_nanos(period.saturating_mul(self.tick + 1));
        let keep = self.tick_at(at);
        sim.step(self.tick);
        keep
    }

    /// One step stamped `at` (a live-timing run: `dt` is the stamps'
    /// difference, as in a live room). Its hash is kept.
    pub fn tick_at(&mut self, at: Instant) -> bool {
        self.tick += 1;
        let keep = self.actor.step(&TickInfo {
            tick: self.tick,
            at,
        });
        let h = self.actor.state_hash();
        self.hashes.push(h);
        keep
    }

    /// The steps taken.
    pub fn tick_no(&self) -> u64 {
        self.tick
    }

    /// Every step's state hash, in order.
    pub fn hashes(&self) -> &[Option<u64>] {
        &self.hashes
    }

    /// The room's seed.
    pub fn seed(&self) -> u64 {
        self.actor.seed
    }

    /// The room's call table counters.
    pub fn counts(&self) -> CallCounts {
        *self.actor.calls.counts()
    }

    /// The room's world.
    pub fn world(&self) -> &W {
        &self.actor.world
    }
}
