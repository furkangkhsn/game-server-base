//! A migrating player's in-flight calls ride the `Migrate` (BACKLOG
//! B152, `docs/SERVICES.md` §8), across two real shard actors stepped by
//! hand: the destination re-issues the call under its ORIGINAL key (an
//! idempotent bank charges once), hands the result to the game under
//! the player's owner THERE, and answers the client from there; the
//! source counts the call `calls_exported` and its late answer
//! `results_late`. A refused send puts the calls back exactly as they
//! were; a migration whose join died counts the answers it carried.

use std::collections::BTreeMap;
use std::sync::mpsc as smpsc;

use prost::Message;

use super::*;
use crate::calls::{Ask, CallError, CallKey, CallSpec, Calls, Envelope, Mail, Owner, Route};
use crate::rpc::{RPC_REQ_OP, RpcReply};

const OP_SNAP: u16 = 0x71C0;
const OP_PRIV: u16 = 0x71C1;

/// The client's request: buy for `price` gold.
#[derive(Clone, PartialEq, prost::Message)]
struct Buy {
    #[prost(uint64, tag = "1")]
    price: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
struct Bought {
    #[prost(uint64, tag = "1")]
    balance: u64,
}
impl crate::calls::Call for Buy {
    type Response = Bought;
    type Error = Bought;
}
impl crate::calls::Request for Buy {
    const OP: u16 = 1100;
}

/// The bank's one call.
#[derive(Clone, Debug)]
struct Charge {
    amount: u64,
}
impl crate::calls::Call for Charge {
    type Response = Bought;
    type Error = Bought;
}
enum BankMsg {
    Charge(Ask<Charge>),
}

/// The reserve a charge carries back.
struct Reserve(u64);

/// A fake bank, drained by the test: ONE effect per key (idempotent),
/// 500 gold to start.
struct Bank {
    inbox: crate::channel::Inbox<Mail<BankMsg>>,
    done: BTreeMap<CallKey, u64>,
    balance: u64,
    charges: u32,
    seen: Vec<CallKey>,
}

impl Bank {
    fn new() -> (Route<BankMsg>, Self) {
        let (route, inbox) = Route::new("bank", 16);
        (
            route,
            Self {
                inbox,
                done: BTreeMap::new(),
                balance: 500,
                charges: 0,
                seen: Vec::new(),
            },
        )
    }

    /// Answer everything in the mailbox.
    fn step(&mut self) {
        while let Ok(Mail::Call(Envelope { cx, msg })) = self.inbox.try_recv() {
            let BankMsg::Charge(ask) = msg;
            let key = cx.key();
            self.seen.push(key);
            let balance = match self.done.get(&key) {
                Some(b) => *b,
                None => {
                    self.balance -= ask.call.amount;
                    self.charges += 1;
                    self.done.insert(key, self.balance);
                    self.balance
                }
            };
            ask.answer(Ok(Bought { balance }));
        }
    }
}

/// What a shard's game saw resolve: (shard, owner, reserve, balance).
type Resolved = (usize, Owner, u64, Result<u64, CallError<Bought>>);

/// The two-shard logic: shard 0 owns x < 0, shard 1 x >= 0. A `Buy`
/// reserves, calls the bank (`.answers` the client) and starts the
/// player walking east — it crosses the seam in the same tick. The
/// owner of a player is SHARD-LOCAL (`1000 · (index + 1) + player`, an
/// entity-like id): the import must re-own the call at the destination.
struct CLogic {
    index: usize,
    next_serial: u64,
    player_ent: HashMap<PlayerId, u64>,
    ent_player: HashMap<u64, PlayerId>,
    bank: Route<BankMsg>,
    resolved: smpsc::Sender<Resolved>,
}

impl CLogic {
    fn owner_of(&self, p: PlayerId) -> Owner {
        Owner(1000 * (self.index as u64 + 1) + p.0)
    }
}

impl GameLogic<TWorld> for CLogic {
    type GroupKey = ();
    type Strip = TStrip;

    fn snapshot_op(&self) -> u16 {
        OP_SNAP
    }
    fn private_op(&self) -> u16 {
        OP_PRIV
    }
    fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut TWorld,
        _ctx: &TickCtx,
        _g: &(),
        _b: &[BorderRecord<TStrip>],
        _out: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn private(
        &mut self,
        _w: &mut TWorld,
        _p: PlayerId,
        _g: &(),
        replies: &[RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        for r in replies {
            let mut buf = Vec::new();
            gsb_protocol::base::RpcResponse::from(r)
                .encode_length_delimited(&mut buf)
                .expect("vec grows");
            out.extend_from_slice(&buf);
        }
        !replies.is_empty()
    }
    fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
        self.next_serial += 1;
        let wire = interleaved_id(self.index, 2, self.next_serial);
        w.ents.insert(wire, (-1.0, 0.0, 0));
        let player = PlayerId(conn.0);
        self.player_ent.insert(player, wire);
        self.ent_player.insert(wire, player);
        Admission {
            player,
            entity: wire,
        }
    }
    fn on_leave(&mut self, w: &mut TWorld, player: PlayerId) {
        if let Some(wire) = self.player_ent.remove(&player) {
            self.ent_player.remove(&wire);
            w.ents.remove(&wire);
        }
    }
    fn ingest(&mut self, _w: &mut TWorld, _ctx: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut TWorld, _ctx: &TickCtx) {}

    fn boot_calls(&mut self, calls: &mut Calls) -> Result<(), crate::calls::BootError> {
        calls.accept::<Buy>()
    }
    fn call_owner(&self, _w: &TWorld, player: PlayerId) -> Option<Owner> {
        Some(self.owner_of(player))
    }
    fn owner_alive(&self, _w: &TWorld, owner: Owner) -> Option<bool> {
        Some(self.player_ent.keys().any(|p| self.owner_of(*p) == owner))
    }
}

impl ShardLogic<TWorld> for CLogic {
    type State = TState;

    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        2
    }
    fn serial_capacity(&self) -> u64 {
        SHARD_SERIAL_CAPACITY
    }
    fn serial_used(&self) -> u64 {
        self.next_serial
    }
    fn neighbors(&self) -> &[usize] {
        if self.index == 0 { &[1] } else { &[0] }
    }
    fn collect_migrations(&mut self, w: &mut TWorld, neighbor: usize) -> Vec<Migrating<TState>> {
        w.ents
            .iter()
            .filter(|(_, (x, _, _))| TLogic::region_of(*x) == neighbor)
            .map(|(&wire, &(x, y, mode))| Migrating {
                wire,
                state: TState { x, y, mode },
                player: self.ent_player.get(&wire).copied(),
            })
            .collect()
    }
    fn on_migrate_in(&mut self, w: &mut TWorld, wire: u64, s: TState, player: Option<PlayerId>) {
        // Arrived: stop walking.
        w.ents.insert(wire, (s.x, s.y, 0));
        if let Some(p) = player {
            self.player_ent.insert(p, wire);
            self.ent_player.insert(wire, p);
        }
    }
    fn on_migrate_out(&mut self, w: &mut TWorld, wire: u64) {
        if let Some(p) = self.ent_player.remove(&wire) {
            self.player_ent.remove(&p);
        }
        w.ents.remove(&wire);
    }
    fn collect_border(&self, _w: &TWorld) -> Vec<BorderRecord<TStrip>> {
        Vec::new()
    }
    fn own_wires(&self, w: &TWorld) -> Vec<u64> {
        w.ents.keys().copied().collect()
    }

    fn update_seam_with_calls(
        &mut self,
        w: &mut TWorld,
        _ctx: &TickCtx,
        _seam: &mut CrossSeam<'_, TStrip>,
        calls: &mut Calls,
    ) {
        for r in calls.take_resolved::<Charge, Reserve>() {
            let _ =
                self.resolved
                    .send((self.index, r.owner, r.ctx.0, r.outcome.map(|b| b.balance)));
        }
        for r in calls.take_incoming::<Buy>() {
            let spec = CallSpec::new(
                &self.bank,
                Charge {
                    amount: r.body.price,
                },
                BankMsg::Charge,
            );
            calls
                .call(spec)
                .owner(r.owner)
                .with(Reserve(r.body.price))
                .answers(r.reply);
            if let Some(wire) = self.player_ent.get(&r.player)
                && let Some(e) = w.ents.get_mut(wire)
            {
                e.0 = 0.0; // across the seam: shard 1's region
            }
        }
    }
}

type CShard = ShardActor<TWorld, (), TState, TStrip>;

/// Two shards wired to each other, one bank, what the games resolved.
struct Pair {
    s: [CShard; 2],
    txs: [Mailbox<ShardMsg<TState, TStrip>>; 2],
    bank: Bank,
    resolved: smpsc::Receiver<Resolved>,
    tick: u64,
}

impl Pair {
    /// `cap1`: shard 1's inbox capacity (1 + a filler = a refused send).
    fn new(cap1: usize) -> Self {
        let (route, bank) = Bank::new();
        let (tx, resolved) = smpsc::channel();
        let (tx0, rx0) = channel::<ShardMsg<TState, TStrip>>(16);
        let (tx1, rx1) = channel::<ShardMsg<TState, TStrip>>(cap1);
        let (dummy, _dummy_rx) = channel::<ShardMsg<TState, TStrip>>(1);
        let shard = |index: usize, rx, links| {
            let (_tick_tx, tick_rx) = broadcast::channel(4);
            ShardActor::new(
                RoomConfig {
                    id: RoomId(31),
                    keepalive_hz: 0.0,
                    metrics_cadence_hz: 0.0,
                    ..Default::default()
                },
                index,
                TWorld::default(),
                Box::new(CLogic {
                    index,
                    next_serial: 0,
                    player_ent: HashMap::new(),
                    ent_player: HashMap::new(),
                    bank: route.clone(),
                    resolved: tx.clone(),
                }),
                tick_rx,
                rx,
                links,
                1,
                metrics_null(),
                None,
            )
        };
        let s0 = shard(0, rx0, vec![dummy.clone(), tx1.clone()]);
        let s1 = shard(1, rx1, vec![tx0.clone(), dummy]);
        Self {
            s: [s0, s1],
            txs: [tx0, tx1],
            bank,
            resolved,
            tick: 0,
        }
    }

    /// Join conn 1 on shard 0: its input and its output.
    fn join(&mut self) -> (u64, Mailbox<Action>, mpsc::Receiver<FrameBatch>) {
        let (out, rx) = mpsc::channel::<FrameBatch>(64);
        let (reply, mut replied) = oneshot::channel();
        assert!(self.s[0].handle_msg(
            ShardMsg::Join {
                conn: ConnectionId(1),
                epoch: 1,
                identity: String::new(),
                out,
                reply,
                claims: None,
            },
            0,
        ));
        let (wire, actions) = replied.try_recv().expect("sync reply").expect("join ok");
        (wire, actions, rx)
    }

    /// One tick of both shards (0 first), then the bank's drain.
    fn step(&mut self) {
        self.tick += 1;
        for s in &mut self.s {
            assert!(s.step(&tinfo(self.tick)));
        }
        self.bank.step();
    }

    fn resolved(&self) -> Vec<Resolved> {
        self.resolved.try_iter().collect()
    }
}

fn buy(actions: &Mailbox<Action>, id: u64, price: u64) {
    let env = gsb_protocol::base::RpcRequest {
        id,
        op: u32::from(<Buy as crate::calls::Request>::OP),
        payload: Buy { price }.encode_to_vec(),
    };
    actions
        .try_send(Action {
            conn: ConnectionId(1),
            player: PlayerId(0),
            op: RPC_REQ_OP,
            payload: env.encode_to_vec().into(),
        })
        .expect("input has room");
}

/// Every answer the client got: (id, ok, balance).
fn answers(out: &mut mpsc::Receiver<FrameBatch>) -> Vec<(u64, bool, u64)> {
    let mut got = Vec::new();
    while let Ok(batch) = out.try_recv() {
        for f in batch.iter().filter(|f| f.op == OP_PRIV) {
            let mut buf = &f.payload[..];
            while !buf.is_empty() {
                let r = gsb_protocol::base::RpcResponse::decode_length_delimited(&mut buf)
                    .expect("a response");
                let balance = Bought::decode(&r.payload[..]).map_or(0, |b| b.balance);
                got.push((r.id, r.ok, balance));
            }
        }
    }
    got
}

#[tokio::test]
async fn a_migrating_players_call_is_reissued_at_the_destination_under_its_key() {
    let mut pair = Pair::new(16);
    let (_wire, actions, mut out) = pair.join();
    buy(&actions, 7, 100);
    // Tick 1: shard 0 takes the Buy, calls the bank (FLUSH), and the
    // player crosses — the call rides the Migrate. The bank charges.
    pair.step();
    assert_eq!(pair.bank.charges, 1);
    let c0 = *pair.s[0].calls.counts();
    assert_eq!((c0.calls_sent, c0.calls_exported), (1, 1));
    assert_eq!(pair.s[0].calls.pending_len(), 0, "nothing left behind");
    // Tick 2: shard 0's answer is late; shard 1 installs the player and
    // re-issues the call under the same key — the bank dedupes it.
    pair.step();
    assert_eq!(pair.s[0].calls.counts().results_late, 1);
    assert_eq!(pair.s[1].calls.counts().calls_imported, 1);
    assert_eq!(pair.bank.seen.len(), 2, "the call went out twice");
    assert_eq!(pair.bank.seen[0], pair.bank.seen[1], "under ONE key");
    assert_eq!(pair.bank.charges, 1, "one effect");
    assert!(pair.resolved().is_empty(), "never in the issuing tick");
    // Tick 3: shard 1's RESPONSES — the game gets the result under its
    // OWN owner for the player, the client its answer, once.
    pair.step();
    assert_eq!(pair.resolved(), [(1, Owner(2001), 100, Ok(400))]);
    assert_eq!(answers(&mut out), [(7, true, 400)]);

    let c0 = *pair.s[0].calls.counts();
    assert_eq!(
        (c0.results_ok, c0.answers_sent, c0.responses_owner_gone),
        (0, 0, 0)
    );
    assert_eq!(pair.s[0].m.requests_abandoned, 0, "the answer travelled");
    let c1 = *pair.s[1].calls.counts();
    assert_eq!(
        (
            c1.calls_sent,
            c1.results_ok,
            c1.answers_sent,
            c1.responses_delivered
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(pair.s[1].m.migrations_in, 1);
}

/// A refused send (shard 1's inbox is full) rolls the player back — its
/// calls included: nothing is counted exported, and the answer arrives
/// at shard 0 as if nothing had happened.
#[tokio::test]
async fn a_refused_migration_keeps_the_calls_where_they_were() {
    let mut pair = Pair::new(1);
    pair.txs[1]
        .try_send(ShardMsg::ResyncRequest { from: 0 })
        .expect("the filler fits");
    let (_wire, actions, mut out) = pair.join();
    buy(&actions, 7, 100);
    // Shard 1 is not stepped: its inbox stays full.
    pair.tick += 1;
    assert!(pair.s[0].step(&tinfo(pair.tick)));
    pair.bank.step();
    assert_eq!(pair.s[0].m.migrations_failed, 1);
    assert_eq!(pair.s[0].calls.counts().calls_exported, 0);
    assert_eq!(pair.s[0].calls.pending_len(), 1, "the call is back");
    pair.tick += 1;
    assert!(pair.s[0].step(&tinfo(pair.tick)));
    assert_eq!(pair.resolved(), [(0, Owner(1001), 100, Ok(400))]);
    assert_eq!(answers(&mut out), [(7, true, 400)]);
    let c0 = *pair.s[0].calls.counts();
    assert_eq!((c0.results_ok, c0.answers_sent, c0.results_late), (1, 1, 0));
}

/// A migration whose join died before it landed (the leave reached
/// shard 1 first): the answers its calls owed the dead session are
/// counted abandoned there; the call's own answer is late at shard 0.
#[tokio::test]
async fn a_dead_joins_migration_counts_the_answers_it_carried() {
    let mut pair = Pair::new(16);
    let (wire, actions, _out) = pair.join();
    buy(&actions, 7, 100);
    pair.tick += 1;
    assert!(pair.s[0].step(&tinfo(pair.tick)));
    pair.txs[1]
        .try_send(ShardMsg::Leave {
            conn: ConnectionId(1),
            entity: wire,
            epoch: 1,
        })
        .expect("room");
    assert!(pair.s[1].step(&tinfo(pair.tick))); // Migrate deferred, Leave tombstones
    pair.bank.step();
    pair.step();
    assert_eq!(
        pair.s[1].m.migrations_in, 0,
        "the dead join's row never lands"
    );
    assert_eq!(pair.s[1].m.requests_abandoned, 1, "its answer, counted");
    assert_eq!(pair.s[1].calls.counts().calls_imported, 0);
    assert_eq!(pair.s[0].calls.counts().results_late, 1);
    assert_eq!(pair.bank.charges, 1);
}
