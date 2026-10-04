//! The scenario every test shares: a game contract (what `.proto`
//! codegen would emit), three `#[service]` traits, one fake backend that
//! implements all three, the game's components and helpers, the scripted
//! inputs, and the canonical state summary every flavour hashes.
#![allow(dead_code)]

pub mod bevy_game;
pub mod hecs_game;
pub mod plain_game;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use bevy_ecs::prelude::Component;
pub use s0_calls::*;

// ---- the contract: what `.proto` codegen emits --------------------------
//
// service Game {
//   rpc TransferGold(TransferGold) returns (TransferDone) { option (gsb.op) = 1001; option (gsb.error) = "GameError"; }
//   rpc PickUp(PickUp)             returns (PickedUp)     { option (gsb.op) = 1002; ... }
//   rpc Craft(CraftReq)            returns (Crafted)      { option (gsb.op) = 1003; ... }
//   rpc Equip(Equip)               returns (Equipped)     { option (gsb.op) = 1004; ... }
//   rpc ListFriends(ListFriends)   returns (FriendList)   { option (gsb.op) = 1005; ... }
// }

#[derive(Clone, PartialEq, prost::Message)]
pub struct TransferGold {
    #[prost(string, tag = "1")]
    pub to: String,
    #[prost(uint64, tag = "2")]
    pub amount: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct TransferDone {
    #[prost(uint64, tag = "1")]
    pub balance: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct GameError {
    #[prost(uint32, tag = "1")]
    pub code: u32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct PickUp {
    #[prost(uint64, tag = "1")]
    pub loot: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct PickedUp {
    #[prost(uint32, tag = "1")]
    pub item: u32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct CraftReq {
    #[prost(uint32, tag = "1")]
    pub recipe: u32,
    #[prost(uint32, repeated, tag = "2")]
    pub consume: Vec<u32>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct Crafted {
    #[prost(uint32, tag = "1")]
    pub item: u32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct Equip {
    #[prost(uint32, tag = "1")]
    pub item: u32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct Equipped {
    #[prost(uint32, tag = "1")]
    pub item: u32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct ListFriends {}
#[derive(Clone, PartialEq, prost::Message)]
pub struct FriendList {
    #[prost(string, repeated, tag = "1")]
    pub names: Vec<String>,
}

macro_rules! request {
    ($req:ty, $resp:ty, $op:expr) => {
        impl Call for $req {
            type Response = $resp;
            type Error = GameError;
        }
        impl Request for $req {
            const OP: u16 = $op;
        }
    };
}
request!(TransferGold, TransferDone, 1001);
request!(PickUp, PickedUp, 1002);
request!(CraftReq, Crafted, 1003);
request!(Equip, Equipped, 1004);
request!(ListFriends, FriendList, 1005);

pub const BUSY: u32 = 1;
pub const INSUFFICIENT: u32 = 2;
pub const CLAIMED: u32 = 3;
pub const NOT_LOADED: u32 = 4;
pub const MISSING: u32 = 5;
pub const NO_SUCH_LOOT: u32 = 6;
pub const UNKNOWN_RECIPE: u32 = 7;

pub fn err(code: u32) -> GameError {
    GameError { code }
}

// ---- the services (in-process: plain Rust types are fine) ---------------

/// In-process answer type: not a wire message.
#[derive(Clone, Debug, PartialEq)]
pub struct InventoryData {
    pub items: Vec<u32>,
    pub gold: u64,
}

#[service]
pub trait Bank {
    /// From `cx.player()` (verified) to `to`.
    async fn transfer(
        &self,
        cx: &CallCx,
        to: Identity,
        amount: u64,
    ) -> Result<TransferDone, GameError>;
}

#[service]
pub trait Inventory {
    async fn load(&self, cx: &CallCx, who: Identity) -> Result<InventoryData, GameError>;
    async fn grant(&self, cx: &CallCx, who: Identity, item: u32) -> Result<PickedUp, GameError>;
    /// For `cx.player()` (verified).
    async fn craft(
        &self,
        cx: &CallCx,
        recipe: u32,
        consume: Vec<u32>,
    ) -> Result<Crafted, GameError>;
    async fn save(
        &self,
        cx: &CallCx,
        who: Identity,
        items: Vec<u32>,
        equipped: u32,
    ) -> Result<(), GameError>;
}

#[service]
pub trait Friends {
    async fn list(&self, cx: &CallCx, req: ListFriends) -> Result<FriendList, GameError>;
}

// ---- the fake backend (all three traits; lock-free state) ---------------

pub const ROSTER: [&str; 3] = ["alice", "bob", "carol"];
const KINDS: usize = 8;
const MEMO: usize = 1024;

fn idx(who: &Identity) -> usize {
    ROSTER
        .iter()
        .position(|n| *n == who.as_str())
        .expect("roster player")
}

/// Key → result memo (insert-once, linear probing, CAS): the service's
/// idempotency store.
struct Memo {
    keys: Vec<AtomicU64>,
    vals: Vec<AtomicU64>,
}

impl Memo {
    fn new() -> Self {
        Self {
            keys: (0..MEMO).map(|_| AtomicU64::new(0)).collect(),
            vals: (0..MEMO).map(|_| AtomicU64::new(0)).collect(),
        }
    }
    fn h(k: CallKey) -> u64 {
        k.trace_id() | 1
    }
    fn get(&self, k: CallKey) -> Option<u64> {
        let h = Self::h(k);
        let mut i = (h as usize) % MEMO;
        loop {
            match self.keys[i].load(Ordering::Acquire) {
                0 => return None,
                x if x == h => return Some(self.vals[i].load(Ordering::Acquire)),
                _ => i = (i + 1) % MEMO,
            }
        }
    }
    fn put(&self, k: CallKey, v: u64) {
        let h = Self::h(k);
        let mut i = (h as usize) % MEMO;
        loop {
            match self.keys[i].compare_exchange(0, h, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    self.vals[i].store(v, Ordering::Release);
                    return;
                }
                Err(x) if x == h => return,
                Err(_) => i = (i + 1) % MEMO,
            }
        }
    }
}

pub struct Ledger {
    pub gold: [AtomicI64; 3],
    pub items: [[AtomicU32; KINDS]; 3],
    pub equipped: [AtomicU32; 3],
    pub charges: AtomicU64,
    pub grants: AtomicU64,
    pub crafts: AtomicU64,
    pub saves: AtomicU64,
    /// Every method entry (replay must leave this untouched).
    pub calls: AtomicU64,
    memo: Memo,
    /// Live only: max random latency per call, in ms.
    jitter_ms: u64,
    noise: AtomicU64,
}

#[derive(Clone)]
pub struct FakeBackend(pub Arc<Ledger>);

impl FakeBackend {
    /// alice: 500 gold, items [1, 1, 2]; bob: 100 gold, [2]; carol: 0, [].
    pub fn new() -> Self {
        Self::with_jitter(0)
    }
    pub fn with_jitter(jitter_ms: u64) -> Self {
        let l = Ledger {
            gold: [AtomicI64::new(500), AtomicI64::new(100), AtomicI64::new(0)],
            items: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU32::new(0))),
            equipped: std::array::from_fn(|_| AtomicU32::new(0)),
            charges: AtomicU64::new(0),
            grants: AtomicU64::new(0),
            crafts: AtomicU64::new(0),
            saves: AtomicU64::new(0),
            calls: AtomicU64::new(0),
            memo: Memo::new(),
            jitter_ms,
            noise: AtomicU64::new(0),
        };
        l.items[0][1].store(2, Ordering::Relaxed);
        l.items[0][2].store(1, Ordering::Relaxed);
        l.items[1][2].store(1, Ordering::Relaxed);
        Self(Arc::new(l))
    }

    async fn enter(&self) {
        self.0.calls.fetch_add(1, Ordering::Relaxed);
        if self.0.jitter_ms > 0 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let r = s0_calls::contract::mix(
                u64::from(nanos) ^ self.0.noise.fetch_add(1, Ordering::Relaxed),
            );
            tokio::time::sleep(Duration::from_millis(r % (self.0.jitter_ms + 1))).await;
        }
    }

    pub fn gold(&self, who: &str) -> i64 {
        self.0.gold[idx(&Identity::new(who))].load(Ordering::Relaxed)
    }
    pub fn count(&self, who: &str, kind: u32) -> u32 {
        self.0.items[idx(&Identity::new(who))][kind as usize].load(Ordering::Relaxed)
    }
    pub fn charges(&self) -> u64 {
        self.0.charges.load(Ordering::Relaxed)
    }
    pub fn calls(&self) -> u64 {
        self.0.calls.load(Ordering::Relaxed)
    }
}

impl Bank for FakeBackend {
    async fn transfer(
        &self,
        cx: &CallCx,
        to: Identity,
        amount: u64,
    ) -> Result<TransferDone, GameError> {
        self.enter().await;
        let from = cx.player().ok_or(err(NOT_LOADED))?;
        if let Some(balance) = self.0.memo.get(cx.key()) {
            return Ok(TransferDone { balance });
        }
        let (f, t) = (idx(from), idx(&to));
        let amount = amount as i64;
        if self.0.gold[f].load(Ordering::Acquire) < amount {
            return Err(err(INSUFFICIENT));
        }
        let left = self.0.gold[f].fetch_sub(amount, Ordering::AcqRel) - amount;
        self.0.gold[t].fetch_add(amount, Ordering::AcqRel);
        self.0.charges.fetch_add(1, Ordering::Relaxed);
        self.0.memo.put(cx.key(), left as u64);
        Ok(TransferDone {
            balance: left as u64,
        })
    }
}

impl Inventory for FakeBackend {
    async fn load(&self, _cx: &CallCx, who: Identity) -> Result<InventoryData, GameError> {
        self.enter().await;
        let i = idx(&who);
        let mut items = Vec::new();
        for (kind, n) in self.0.items[i].iter().enumerate() {
            for _ in 0..n.load(Ordering::Acquire) {
                items.push(kind as u32);
            }
        }
        Ok(InventoryData {
            items,
            gold: self.0.gold[i].load(Ordering::Acquire) as u64,
        })
    }

    async fn grant(&self, cx: &CallCx, who: Identity, item: u32) -> Result<PickedUp, GameError> {
        self.enter().await;
        if self.0.memo.get(cx.key()).is_none() {
            self.0.items[idx(&who)][item as usize].fetch_add(1, Ordering::AcqRel);
            self.0.grants.fetch_add(1, Ordering::Relaxed);
            self.0.memo.put(cx.key(), u64::from(item));
        }
        Ok(PickedUp { item })
    }

    async fn craft(
        &self,
        cx: &CallCx,
        recipe: u32,
        consume: Vec<u32>,
    ) -> Result<Crafted, GameError> {
        self.enter().await;
        // Recipe 1: two of kind 1 → one kind 3.
        if recipe != 1 || consume != [1, 1] {
            return Err(err(UNKNOWN_RECIPE));
        }
        if self.0.memo.get(cx.key()).is_some() {
            return Ok(Crafted { item: 3 });
        }
        let who = idx(cx.player().ok_or(err(NOT_LOADED))?);
        if self.0.items[who][1].load(Ordering::Acquire) < 2 {
            return Err(err(MISSING));
        }
        self.0.items[who][1].fetch_sub(2, Ordering::AcqRel);
        self.0.items[who][3].fetch_add(1, Ordering::AcqRel);
        self.0.crafts.fetch_add(1, Ordering::Relaxed);
        self.0.memo.put(cx.key(), 3);
        Ok(Crafted { item: 3 })
    }

    async fn save(
        &self,
        _cx: &CallCx,
        who: Identity,
        _items: Vec<u32>,
        equipped: u32,
    ) -> Result<(), GameError> {
        self.enter().await;
        self.0.equipped[idx(&who)].store(equipped, Ordering::Release);
        self.0.saves.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl Friends for FakeBackend {
    async fn list(&self, cx: &CallCx, _req: ListFriends) -> Result<FriendList, GameError> {
        self.enter().await;
        let me = cx
            .player()
            .map(|p| p.as_str().to_owned())
            .unwrap_or_default();
        Ok(FriendList {
            names: ROSTER
                .iter()
                .filter(|n| **n != me)
                .map(|n| (*n).to_owned())
                .collect(),
        })
    }
}

/// The three clients a room needs.
#[derive(Clone)]
pub struct Clients {
    pub bank: BankClient,
    pub inv: InventoryClient,
    pub friends: FriendsClient,
}

impl Clients {
    pub fn from(host: &impl Host) -> Self {
        Self {
            bank: host.client(),
            inv: host.client(),
            friends: host.client(),
        }
    }
}

/// Test mode with the scenario's timing: bank 3 ticks, inventory 2,
/// friends 1.
pub fn sim(be: &FakeBackend) -> Sim {
    let mut sim = Sim::new();
    sim.service(Delay::ticks(3, Bank::serve(be.clone())), 64);
    sim.service(Delay::ticks(2, Inventory::serve(be.clone())), 64);
    sim.service(Friends::serve(be.clone()), 64);
    sim
}

/// The boot-time registration every flavour shares.
pub fn register(calls: &mut Calls, c: &Clients) {
    calls.accept::<TransferGold>().unwrap();
    calls.accept::<PickUp>().unwrap();
    calls.accept::<CraftReq>().unwrap();
    calls.accept::<Equip>().unwrap();
    let friends = c.friends.clone();
    calls
        .forward(move |r: ListFriends| friends.list(r))
        .unwrap();
}

// ---- game state shared by the flavours (components work for both ECSs) --

#[derive(Component, Clone, Debug)]
pub struct Player {
    pub conn: ConnId,
    pub who: Identity,
}

#[derive(Component, Clone, Debug, Default)]
pub struct Wallet {
    pub gold: u64,
    pub reserved: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Item {
    pub kind: u32,
    pub locked: bool,
}

#[derive(Component, Clone, Debug, Default)]
pub struct Bag {
    pub loaded: bool,
    pub items: Vec<Item>,
    pub equipped: Option<u32>,
    pub dirty: bool,
}

impl Bag {
    pub fn fill(&mut self, kinds: &[u32]) {
        self.items = kinds
            .iter()
            .map(|k| Item {
                kind: *k,
                locked: false,
            })
            .collect();
        self.loaded = true;
    }
    /// Lock one unlocked item per listed kind; all or nothing.
    pub fn try_lock(&mut self, kinds: &[u32]) -> bool {
        for (n, k) in kinds.iter().enumerate() {
            match self.items.iter_mut().find(|i| i.kind == *k && !i.locked) {
                Some(i) => i.locked = true,
                None => {
                    self.unlock(&kinds[..n]);
                    return false;
                }
            }
        }
        true
    }
    pub fn unlock(&mut self, kinds: &[u32]) {
        for k in kinds {
            if let Some(i) = self.items.iter_mut().find(|i| i.kind == *k && i.locked) {
                i.locked = false;
            }
        }
    }
    pub fn consume_locked(&mut self, kinds: &[u32]) {
        for k in kinds {
            if let Some(p) = self.items.iter().position(|i| i.kind == *k && i.locked) {
                self.items.remove(p);
            }
        }
    }
    pub fn has_unlocked(&self, kind: u32) -> bool {
        self.items.iter().any(|i| i.kind == kind && !i.locked)
    }
    pub fn kinds(&self) -> Vec<u32> {
        self.items.iter().map(|i| i.kind).collect()
    }
}

/// Loot on the ground. `claimed` is the PENDING state of a pickup: it
/// lives on the loot entity, which is also the call's owner.
#[derive(Component, Clone, Debug)]
pub struct Loot {
    pub id: u64,
    pub item: u32,
    pub claimed: bool,
    pub expires: u64,
}

#[derive(Component)]
pub struct JustJoined;

/// Call contexts (stay in the room; come back with the result).
pub struct Reserve(pub u64);
pub struct Picker(pub Owner);
pub struct Locked(pub Vec<u32>);

/// The two loot piles spawned on tick 0 (pile 1's item from the room rng).
pub fn loot_piles(rng: &mut Rng) -> [Loot; 2] {
    [
        Loot {
            id: 1,
            item: 1 + rng.below(2) as u32,
            claimed: false,
            expires: 1000,
        },
        Loot {
            id: 2,
            item: 1,
            claimed: false,
            expires: 6,
        },
    ]
}

pub const AUTOSAVE_EVERY: u64 = 5;

// ---- canonical summary (identical across flavours) ----------------------

/// name, gold, reserved, loaded, items, equipped, dirty
pub type PlayerRow = (String, u64, u64, bool, Vec<Item>, Option<u32>, bool);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    pub players: Vec<PlayerRow>,
    pub loots: Vec<(u64, u32, bool)>,
}

impl Summary {
    pub fn player(&mut self, p: &Player, w: &Wallet, b: &Bag) {
        let mut items = b.items.clone();
        items.sort();
        self.players.push((
            p.who.as_str().to_owned(),
            w.gold,
            w.reserved,
            b.loaded,
            items,
            b.equipped,
            b.dirty,
        ));
    }
    pub fn loot(&mut self, l: &Loot) {
        self.loots.push((l.id, l.item, l.claimed));
    }
    pub fn hash(mut self, h: &mut StateHasher) {
        self.players.sort();
        self.loots.sort();
        for (n, g, r, l, items, e, d) in &self.players {
            h.str(n);
            h.u64(*g);
            h.u64(*r);
            h.u64(u64::from(*l));
            for i in items {
                h.u64(u64::from(i.kind) << 1 | u64::from(i.locked));
            }
            h.u64(e.map_or(0, |x| u64::from(x) + 1));
            h.u64(u64::from(*d));
        }
        for (id, item, c) in &self.loots {
            h.u64(*id);
            h.u64(u64::from(*item));
            h.u64(u64::from(*c));
        }
    }
}

// ---- the scripted client side --------------------------------------------

pub const ALICE: ConnId = ConnId(1);
pub const BOB: ConnId = ConnId(2);

pub fn rpc<R: Request>(id: u64, body: &R) -> wire::RpcRequest {
    wire::RpcRequest {
        id,
        op: u32::from(R::OP),
        payload: body.to_wire(),
    }
}

/// Inputs before tick `t` (see the timeline in the inventory tests).
pub fn script(t: u64, i: &Inputs) {
    match t {
        0 => {
            i.join(ALICE, "alice");
            i.join(BOB, "bob");
        }
        3 => {
            let tr = TransferGold {
                to: "bob".into(),
                amount: 100,
            };
            i.request(ALICE, rpc(10, &tr));
            i.request(ALICE, rpc(11, &tr)); // the double click
        }
        4 => {
            i.request(
                ALICE,
                rpc(
                    10,
                    &TransferGold {
                        to: "bob".into(),
                        amount: 100,
                    },
                ),
            ); // same wire id, still in flight
            i.request(ALICE, rpc(20, &PickUp { loot: 1 }));
            i.request(BOB, rpc(21, &PickUp { loot: 1 }));
        }
        5 => {
            i.request(
                ALICE,
                rpc(
                    30,
                    &CraftReq {
                        recipe: 1,
                        consume: vec![1, 1],
                    },
                ),
            );
            i.request(BOB, rpc(40, &Equip { item: 2 }));
            i.request(ALICE, rpc(50, &ListFriends {}));
            i.request(BOB, rpc(22, &PickUp { loot: 2 }));
        }
        _ => {}
    }
}

pub const SCENARIO_TICKS: u64 = 14;

/// Every client answer of a run, with the tick it left in.
pub type Answers = Vec<(u64, ConnId, wire::RpcResponse)>;

pub fn run_sim<L: s0_calls::room::RoomLogic>(
    room: &mut Room<L>,
    sim: &mut Sim,
    ticks: u64,
) -> Answers {
    let mut all = Vec::new();
    for t in 0..ticks {
        script(t, &room.inputs());
        for (c, r) in room.tick_sim(sim) {
            all.push((t, c, r));
        }
    }
    all
}

pub fn answer(all: &Answers, conn: ConnId, id: u64) -> Vec<&(u64, ConnId, wire::RpcResponse)> {
    all.iter()
        .filter(|(_, c, r)| *c == conn && r.id == id)
        .collect()
}

pub fn decode<T: WireMessage>(r: &wire::RpcResponse) -> T {
    T::from_wire(&r.payload).expect("payload decodes")
}

// ---- a tiny no-ECS game for focused tests ---------------------------------

/// State `S` plus one systems function; every owner is alive unless the
/// state's `dead` list says otherwise.
pub struct Scripted<S> {
    pub s: S,
    pub sys: fn(&mut S, &mut Calls, &mut TickCtx),
    pub dead: Vec<Owner>,
}

impl<S> s0_calls::ecs::plain::PlainGame for Scripted<S> {
    fn alive(&self, o: Owner) -> bool {
        !self.dead.contains(&o)
    }
    fn control(&mut self, _c: &Control) {}
    fn systems(&mut self, calls: &mut Calls, ctx: &mut TickCtx) {
        (self.sys)(&mut self.s, calls, ctx);
    }
    fn state_hash(&self, _h: &mut StateHasher) {}
}

pub type ScriptedRoom<S> = Room<s0_calls::ecs::plain::PlainRoom<Scripted<S>>>;

pub fn scripted<S>(
    cfg: RoomConfig,
    s: S,
    boot: impl FnOnce(&mut Calls),
    sys: fn(&mut S, &mut Calls, &mut TickCtx),
) -> ScriptedRoom<S> {
    Room::new(cfg, |mut calls| {
        boot(&mut calls);
        s0_calls::ecs::plain::PlainRoom {
            calls,
            game: Scripted {
                s,
                sys,
                dead: Vec::new(),
            },
        }
    })
}
