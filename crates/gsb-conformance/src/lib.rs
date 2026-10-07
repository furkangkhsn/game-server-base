//! One scenario, every way of writing the game (not published).
//!
//! The engine is ECS-agnostic (`docs/SERVICES.md` §5.5, `docs/KIT-ECS.md`
//! §1): the same game written without an ECS, on bevy_ecs through
//! `gsb-bevy-ecs` and on hecs through `gsb-hecs` must give the same
//! results against the real engine. This crate holds what the styles
//! share — the scenario's wire contract, its fake service, the game's
//! rules over an abstract [`Style`], and the driver ([`play`]: a REAL
//! room actor in TEST mode, `gsb_core::determinism::SimRoom`) — and is
//! itself ECS-free; the styles that use an ECS live in `tests/` (the ECS
//! crates are dev-dependencies only).
//!
//! The scenario (SERVICES §6.1, "send gold to a friend"): alice sends
//! 100 gold to bob — the game reserves it and calls the bank, which
//! answers the client through `.answers`; alice's double click is
//! answered BUSY in the same tick; bob's 600 is INSUFFICIENT; the bank
//! (idempotent, by call key) charges once; the result releases the
//! reserve. Every tick's wallets, every client answer, the bank's
//! charges and the room's call counters form the [`Trace`] the styles
//! must agree on.

use std::collections::BTreeMap;
use std::sync::mpsc as smpsc;

use gsb_core::calls::{
    Ask, BootError, CallKey, CallSpec, Calls, Envelope, Mail, Owner, Request, Route,
};
use gsb_core::channel::Inbox;
use gsb_core::determinism::{Sim, SimClient, SimRoom};
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::metrics::CallCounts;
use gsb_core::room::{Action, GameLogic, RoomConfig, RoomLogic, TickCtx};
use gsb_core::rpc::RpcReply;
use prost::Message;

// ---- the wire contract (what `.proto` codegen would emit) -----------------

/// The client's request: send `amount` gold to `to`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct TransferGold {
    #[prost(string, tag = "1")]
    pub to: String,
    #[prost(uint64, tag = "2")]
    pub amount: u64,
}

/// Its answer: the payer's confirmed balance.
#[derive(Clone, PartialEq, prost::Message)]
pub struct TransferDone {
    #[prost(uint64, tag = "1")]
    pub balance: u64,
}

/// Its typed game error.
#[derive(Clone, PartialEq, prost::Message)]
pub struct GameError {
    #[prost(uint32, tag = "1")]
    pub code: u32,
}

impl gsb_core::calls::Call for TransferGold {
    type Response = TransferDone;
    type Error = GameError;
}
impl Request for TransferGold {
    const OP: u16 = 1001;
}

/// A transfer is already in flight for this owner.
pub const BUSY: u32 = 1;
/// The wallet cannot cover the transfer.
pub const INSUFFICIENT: u32 = 2;
/// The owner holds no wallet.
pub const NO_WALLET: u32 = 99;

// ---- the service -------------------------------------------------------------

/// The bank's one call (what `#[service]` would generate per method).
#[derive(Clone, Debug)]
pub struct Charge {
    /// The amount to take.
    pub amount: u64,
}
impl gsb_core::calls::Call for Charge {
    type Response = TransferDone;
    type Error = GameError;
}

/// The bank's mailbox message.
pub enum BankMsg {
    /// A charge.
    Charge(Ask<Charge>),
}

/// The bank's client (what `#[service]` would generate): a call spec per
/// method.
#[derive(Clone)]
pub struct BankClient {
    route: Route<BankMsg>,
}

impl BankClient {
    /// Charge `amount` from the caller.
    pub fn charge(&self, amount: u64) -> CallSpec<Charge> {
        CallSpec::new(&self.route, Charge { amount }, BankMsg::Charge)
    }
}

/// A fake bank [`play`] drains after every tick (its answers land at the
/// next tick's RESPONSES): ONE effect per call key (idempotent), every
/// payer starting at [`START`] gold.
pub struct Bank {
    inbox: Inbox<Mail<BankMsg>>,
    done: BTreeMap<CallKey, u64>,
    /// Effects applied.
    pub charges: u32,
    /// Calls received (re-issues included).
    pub calls: u32,
}

impl Bank {
    /// The bank and its client.
    pub fn new() -> (BankClient, Self) {
        let (route, inbox) = Route::new("bank", 16);
        (
            BankClient { route },
            Self {
                inbox,
                done: BTreeMap::new(),
                charges: 0,
                calls: 0,
            },
        )
    }

    fn step(&mut self) {
        while let Ok(Mail::Call(Envelope { cx, msg })) = self.inbox.try_recv() {
            let BankMsg::Charge(ask) = msg;
            self.calls += 1;
            let charges = &mut self.charges;
            let balance = *self.done.entry(cx.key()).or_insert_with(|| {
                *charges += 1;
                START - ask.call.amount
            });
            ask.answer(Ok(TransferDone { balance }));
        }
    }
}

// ---- the game ------------------------------------------------------------------

/// Gold every player starts with.
pub const START: u64 = 500;

/// A player's wallet (a component in the ECS styles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wallet {
    /// Confirmed gold.
    pub gold: u64,
    /// Gold reserved by transfers in flight.
    pub reserved: u64,
}

impl Wallet {
    /// A new player's wallet.
    pub const START: Self = Self {
        gold: START,
        reserved: 0,
    };
}

/// The reserve a charge carries back (its context).
#[derive(Debug, Clone, Copy)]
pub struct Reserve(pub u64);

/// The game's rule for one transfer request, on the wallet its owner
/// holds: `Ok(amount)` — reserved, issue the charge — or the game error
/// to answer now. Every style calls this: the styles differ in WHERE the
/// wallet lives, not in what the game decides.
pub fn decide(wallet: Option<&mut Wallet>, busy: bool, amount: u64) -> Result<u64, GameError> {
    let Some(w) = wallet else {
        return Err(GameError { code: NO_WALLET });
    };
    if busy {
        Err(GameError { code: BUSY })
    } else if w.gold - w.reserved < amount {
        Err(GameError { code: INSUFFICIENT })
    } else {
        w.reserved += amount;
        Ok(amount)
    }
}

/// The game's rule for one charge result: release the reserve, keep the
/// confirmed balance.
pub fn settle(wallet: &mut Wallet, reserve: Reserve, balance: Option<u64>) {
    wallet.reserved -= reserve.0;
    if let Some(b) = balance {
        wallet.gold = b;
    }
}

/// One way of writing the game: where its state lives and how its
/// systems reach the call table. `systems` takes the tick's results
/// first (`take_resolved::<Charge, Reserve>` → [`settle`]), then the
/// transfer requests (`take_incoming::<TransferGold>` → [`decide`] →
/// `calls.reply(..)`, or
/// `calls.call(bank.charge(..)).owner(r.owner).with(Reserve(..)).answers(r.reply)`).
pub trait Style: Send + 'static {
    /// The game's world (the room's `W`).
    type World: Send + 'static;
    /// A new player joins: its wallet holds [`Wallet::START`].
    fn join(&mut self, world: &mut Self::World, player: PlayerId);
    /// A player leaves.
    fn leave(&mut self, world: &mut Self::World, player: PlayerId);
    /// The owner the player's session is bound to (its entity in an ECS).
    fn owner(&self, world: &Self::World, player: PlayerId) -> Option<Owner>;
    /// Does `owner` still exist?
    fn alive(&self, world: &Self::World, owner: Owner) -> bool;
    /// The tick's systems.
    fn systems(&mut self, world: &mut Self::World, calls: &mut Calls, bank: &BankClient);
    /// Every player's wallet, by player.
    fn wallets(&mut self, world: &mut Self::World) -> Vec<(PlayerId, Wallet)>;
}

/// What the client got for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The transfer went through: the payer's balance.
    Done(u64),
    /// The game's typed error code.
    GameError(u32),
    /// An engine answer's reason.
    Engine(String),
}

/// Everything the scenario produced, ECS-agnostic.
#[derive(Debug, Clone, PartialEq)]
pub struct Trace {
    /// Every tick's wallets (after its systems).
    pub wallets: Vec<(u64, Vec<(PlayerId, Wallet)>)>,
    /// Every client answer: (tick it arrived after, conn, request id,
    /// answer).
    pub answers: Vec<(u64, u64, u64, Answer)>,
    /// The bank's effects.
    pub charges: u32,
    /// The calls the bank received.
    pub bank_calls: u32,
    /// The room's call counters at the end.
    pub counts: CallCounts,
}

/// [`GameLogic`] over a [`Style`]: everything a room needs that the
/// styles do not differ in.
struct StyleLogic<S> {
    style: S,
    bank: BankClient,
    wallets: smpsc::Sender<(u64, Vec<(PlayerId, Wallet)>)>,
}

const OP_SNAP: u16 = 0x7E00;
const OP_PRIV: u16 = 0x7E01;

impl<S: Style> GameLogic<S::World> for StyleLogic<S> {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        OP_SNAP
    }
    fn private_op(&self) -> u16 {
        OP_PRIV
    }
    fn group_of(&self, _w: &S::World, _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut S::World,
        _c: &TickCtx,
        _g: &(),
        _b: &[gsb_core::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    /// The answers, each a length-delimited `RpcResponse`.
    fn private(
        &mut self,
        _w: &mut S::World,
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
    fn on_join(&mut self, w: &mut S::World, conn: ConnectionId) -> gsb_core::Admission {
        let player = PlayerId(conn.0);
        self.style.join(w, player);
        gsb_core::Admission {
            player,
            entity: conn.0,
        }
    }
    fn on_leave(&mut self, w: &mut S::World, player: PlayerId) {
        self.style.leave(w, player);
    }
    fn ingest(&mut self, _w: &mut S::World, _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut S::World, _c: &TickCtx) {
        unreachable!("the room calls update_with_calls");
    }
    fn boot_calls(&mut self, calls: &mut Calls) -> Result<(), BootError> {
        calls.accept::<TransferGold>()
    }
    fn call_owner(&self, w: &S::World, player: PlayerId) -> Option<Owner> {
        self.style.owner(w, player)
    }
    fn owner_alive(&self, w: &S::World, owner: Owner) -> Option<bool> {
        Some(self.style.alive(w, owner))
    }
    fn update_with_calls(&mut self, w: &mut S::World, ctx: &TickCtx, calls: &mut Calls) {
        self.style.systems(w, calls, &self.bank);
        let _ = self.wallets.send((ctx.tick, self.style.wallets(w)));
    }
}

impl<S: Style> RoomLogic<S::World> for StyleLogic<S> {}

fn transfer(c: &SimClient, id: u64, to: &str, amount: u64) {
    c.request(
        id,
        &TransferGold {
            to: to.to_owned(),
            amount,
        },
    );
}

fn answers(c: &mut SimClient, tick: u64, into: &mut Vec<(u64, u64, u64, Answer)>) {
    for f in c.frames().iter().filter(|f| f.op == OP_PRIV) {
        let mut buf = &f.payload[..];
        while !buf.is_empty() {
            let r = gsb_protocol::base::RpcResponse::decode_length_delimited(&mut buf)
                .expect("a length-delimited response");
            let answer = if r.ok {
                Answer::Done(TransferDone::decode(&r.payload[..]).expect("done").balance)
            } else if r.game_error {
                Answer::GameError(GameError::decode(&r.payload[..]).expect("error").code)
            } else {
                Answer::Engine(r.reason)
            };
            into.push((tick, c.conn.0, r.id, answer));
        }
    }
}

/// Play the scenario against a real room actor (TEST mode) running
/// `style` over `world`, and return what it produced.
pub fn play<S: Style>(style: S, world: S::World) -> Trace {
    let (client, mut bank) = Bank::new();
    let (wallets_tx, wallets) = smpsc::channel();
    let logic = StyleLogic {
        style,
        bank: client,
        wallets: wallets_tx,
    };
    let config = RoomConfig {
        id: RoomId(5),
        seed: Some(5),
        ..Default::default()
    };
    let mut room = SimRoom::new(config, world, Box::new(logic));
    let mut sim = Sim::new();
    let mut alice = room.join(1, "alice");
    let mut bob = room.join(2, "bob");
    let mut got = Vec::new();
    for t in 1..=5 {
        if t == 2 {
            transfer(&alice, 10, "bob", 100);
            transfer(&alice, 11, "bob", 100); // the double click: BUSY
            transfer(&bob, 20, "alice", 600); // more than bob has
        }
        assert!(room.tick(&mut sim));
        bank.step();
        answers(&mut alice, t, &mut got);
        answers(&mut bob, t, &mut got);
    }
    Trace {
        wallets: wallets.try_iter().collect(),
        answers: got,
        charges: bank.charges,
        bank_calls: bank.calls,
        counts: room.counts(),
    }
}
