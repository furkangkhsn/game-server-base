//! The call table in a real room actor (round q1, `docs/SERVICES.md`
//! Faz 1): the spike's scenarios (`spikes/s0-calls/scenario`) that apply
//! to Faz 1, driven step by step through `RoomActor::step` against fake
//! services whose mailboxes the test drains itself (so every answer
//! lands on a tick the test chooses).
//!
//! The game ([`Game`]) is the spike's plain (ECS-free) game: a wallet
//! per player, a transfer system with a synchronous reserve, a bank call
//! that `.answers` the client, and a `forward` rule for the friend list.

use super::*;
use crate::calls::{
    Ask, BootError, CallError, CallKey, CallSpec, Calls, Envelope, Mail, Owner, Route,
};
use crate::room::actor::RoomActor;
use crate::rpc::RPC_REQ_OP;
use prost::Message;
use std::collections::BTreeMap;
use std::sync::mpsc as smpsc;

mod closing;
mod forward;
mod idle;
mod ordering;
mod refusal;
mod transfer;

const OP_SNAP: u16 = 0x7060;
const OP_PRIV: u16 = 0x7061;
/// Answered by `handle_request` (the migration path, untouched).
const OP_LEGACY: u16 = 0x0002;

// ---- the wire contract (what `.proto` codegen would emit) -----------------

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct TransferGold {
    #[prost(string, tag = "1")]
    pub to: String,
    #[prost(uint64, tag = "2")]
    pub amount: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct TransferDone {
    #[prost(uint64, tag = "1")]
    pub balance: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct GameError {
    #[prost(uint32, tag = "1")]
    pub code: u32,
}
impl crate::calls::Call for TransferGold {
    type Response = TransferDone;
    type Error = GameError;
}
impl crate::calls::Request for TransferGold {
    const OP: u16 = 1001;
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ListFriends {
    #[prost(uint32, tag = "1")]
    pub page: u32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct FriendList {
    #[prost(string, repeated, tag = "1")]
    pub names: Vec<String>,
}
impl crate::calls::Call for ListFriends {
    type Response = FriendList;
    type Error = GameError;
}
impl crate::calls::Request for ListFriends {
    const OP: u16 = 1005;
}

/// A second request type (ordering).
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Ping {
    #[prost(uint32, tag = "1")]
    pub n: u32,
}
impl crate::calls::Call for Ping {
    type Response = Ping;
    type Error = GameError;
}
impl crate::calls::Request for Ping {
    const OP: u16 = 1007;
}

pub(super) const BUSY: u32 = 1;
pub(super) const INSUFFICIENT: u32 = 2;

// ---- the services ----------------------------------------------------------

/// The bank's one call (what `#[gsb::service]` would generate per method).
#[derive(Clone, Debug)]
pub(super) struct Transfer {
    pub to: String,
    pub amount: u64,
}
impl crate::calls::Call for Transfer {
    type Response = TransferDone;
    type Error = GameError;
}
pub(super) enum BankMsg {
    Transfer(Ask<Transfer>),
}

#[derive(Clone, Debug)]
pub(super) struct List {
    pub page: u32,
}
impl crate::calls::Call for List {
    type Response = FriendList;
    type Error = GameError;
}
pub(super) enum FriendsMsg {
    List(Ask<List>),
}

/// A fake bank: answers each call `delay` drains after it took it, ONE
/// effect per key (an idempotent service), 500 gold per payer.
pub(super) struct Bank {
    pub inbox: crate::channel::Inbox<Mail<BankMsg>>,
    pub delay: u32,
    held: Vec<(u32, crate::calls::CallCx, Ask<Transfer>)>,
    done: BTreeMap<CallKey, TransferDone>,
    balances: BTreeMap<String, u64>,
    /// Effects applied (charges), and every key the bank saw.
    pub charges: u32,
    pub seen: Vec<(CallKey, Option<String>)>,
}

impl Bank {
    pub(super) fn new(capacity: usize, delay: u32) -> (Route<BankMsg>, Self) {
        let (route, inbox) = Route::new("bank", capacity);
        (
            route,
            Self {
                inbox,
                delay,
                held: Vec::new(),
                done: BTreeMap::new(),
                balances: BTreeMap::new(),
                charges: 0,
                seen: Vec::new(),
            },
        )
    }

    /// One drain: take the mailbox, answer what is due.
    pub(super) fn step(&mut self) {
        while let Ok(Mail::Call(Envelope { cx, msg })) = self.inbox.try_recv() {
            let BankMsg::Transfer(ask) = msg;
            assert!(!ask.call.to.is_empty(), "a transfer names its payee");
            self.seen
                .push((cx.key(), cx.player().map(|p| p.as_str().to_owned())));
            self.held.push((self.delay, cx, ask));
        }
        let mut keep = Vec::new();
        for (left, cx, ask) in std::mem::take(&mut self.held) {
            if left > 1 {
                keep.push((left - 1, cx, ask));
                continue;
            }
            let key = cx.key();
            if let Some(d) = self.done.get(&key) {
                ask.answer(Ok(d.clone()));
                continue;
            }
            let payer = cx
                .player()
                .map(|p| p.as_str().to_owned())
                .unwrap_or_default();
            let bal = self.balances.entry(payer).or_insert(500);
            if *bal < ask.call.amount {
                ask.answer(Err(GameError { code: INSUFFICIENT }));
                continue;
            }
            *bal -= ask.call.amount;
            self.charges += 1;
            let d = TransferDone { balance: *bal };
            self.done.insert(key, d.clone());
            ask.answer(Ok(d));
        }
        self.held = keep;
    }
}

// ---- the game --------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Wallet {
    pub gold: u64,
    pub reserved: u64,
}

/// The reserve a transfer call carries back (its context).
pub(super) struct Reserve(pub u64);

/// What the game saw, reported to the test (the logic is boxed in the
/// actor).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Seen {
    /// A request taken: its op, its sender, its owner.
    Request(u16, String, Owner),
    /// A transfer resolved: owner, reserve, outcome (balance or error).
    Resolved(Owner, u64, Result<u64, CallError<GameError>>),
    /// A player's wallet after the tick's systems.
    Wallet(PlayerId, Wallet),
}

/// The knobs a scenario turns.
#[derive(Clone, Copy)]
pub(super) struct Mode {
    /// Take the transfer requests at all (`false`: `requests_untaken`).
    pub take: bool,
    /// Answer or attach what was taken (`false`: `requests_unanswered`).
    pub answer: bool,
    /// Take the resolved transfers (`false`: `responses_unhandled`).
    pub take_resolved: bool,
    /// Sessions get no owner (`call_owner` → `None`).
    pub unbound: bool,
    /// Every owner is gone (`owner_alive` → `Some(false)`).
    pub owners_gone: bool,
    /// Take the requests through `take_incoming_all` (canonical order).
    pub take_all: bool,
}

impl Default for Mode {
    fn default() -> Self {
        Self {
            take: true,
            answer: true,
            take_resolved: true,
            unbound: false,
            owners_gone: false,
            take_all: false,
        }
    }
}

pub(super) struct Game {
    pub bank: Route<BankMsg>,
    pub friends: Option<Route<FriendsMsg>>,
    pub mode: Mode,
    pub wallets: BTreeMap<PlayerId, Wallet>,
    pub seen: smpsc::Sender<Seen>,
    /// Claim `Ping` too (and accept `TransferGold` a second time when set:
    /// the boot collision).
    pub boot_twice: bool,
}

impl GameLogic<()> for Game {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        OP_SNAP
    }
    fn private_op(&self) -> u16 {
        OP_PRIV
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _b: &[crate::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    /// The answers, each a length-delimited `RpcResponse`.
    fn private(
        &mut self,
        _w: &mut (),
        _p: PlayerId,
        _g: &(),
        replies: &[crate::rpc::RpcReply],
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
    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        self.wallets.insert(
            PlayerId(c.0),
            Wallet {
                gold: 500,
                reserved: 0,
            },
        );
        Admission {
            player: PlayerId(c.0),
            entity: c.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), p: PlayerId) {
        self.wallets.remove(&p);
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {
        panic!("the actor calls update_with_calls");
    }

    fn handle_request(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        req: &crate::rpc::RpcRequest,
    ) -> Option<crate::rpc::RequestDecision> {
        (req.op == OP_LEGACY).then(|| crate::rpc::RequestDecision::Reply(b"legacy"[..].into()))
    }

    fn boot_calls(&mut self, calls: &mut Calls) -> Result<(), BootError> {
        calls.accept::<TransferGold>()?;
        calls.accept::<Ping>()?;
        if let Some(friends) = self.friends.clone() {
            calls.forward(move |r: ListFriends| {
                CallSpec::new(&friends, List { page: r.page }, FriendsMsg::List)
            })?;
        }
        if self.boot_twice {
            calls.accept::<TransferGold>()?;
        }
        Ok(())
    }

    fn call_owner(&self, _w: &(), player: PlayerId) -> Option<Owner> {
        (!self.mode.unbound).then_some(player.into())
    }

    fn owner_alive(&self, _w: &(), _o: Owner) -> Option<bool> {
        self.mode.owners_gone.then_some(false)
    }

    fn update_with_calls(&mut self, _w: &mut (), _c: &TickCtx, calls: &mut Calls) {
        // 1) Results first: release the reserve, write the confirmed
        //    balance.
        if self.mode.take_resolved {
            for r in calls.take_resolved::<Transfer, Reserve>() {
                let p = PlayerId(r.owner.0);
                if let Some(w) = self.wallets.get_mut(&p) {
                    w.reserved -= r.ctx.0;
                    if let Ok(done) = &r.outcome {
                        w.gold = done.balance;
                    }
                }
                let _ = self.seen.send(Seen::Resolved(
                    r.owner,
                    r.ctx.0,
                    r.outcome.map(|d| d.balance),
                ));
            }
        }
        // 2) Then the new requests.
        if self.mode.take_all {
            for any in calls.take_incoming_all() {
                match any.downcast::<TransferGold>() {
                    Ok(r) => {
                        let _ = self.seen.send(Seen::Request(
                            1001,
                            r.from.as_str().to_owned(),
                            r.owner,
                        ));
                        calls.reply(r.reply, Err(GameError { code: 0 }));
                    }
                    Err(any) => {
                        let r = any.downcast::<Ping>().ok().expect("only two types");
                        let _ = self.seen.send(Seen::Request(
                            1007,
                            r.from.as_str().to_owned(),
                            r.owner,
                        ));
                        let n = r.body.n;
                        calls.reply(r.reply, Ok(Ping { n }));
                    }
                }
            }
        } else if self.mode.take {
            for r in calls.take_incoming::<TransferGold>() {
                let _ = self
                    .seen
                    .send(Seen::Request(1001, r.from.as_str().to_owned(), r.owner));
                if !self.mode.answer {
                    continue; // the ReplyTo is dropped: unanswered
                }
                let Some(w) = self.wallets.get_mut(&PlayerId(r.owner.0)) else {
                    calls.reply(r.reply, Err(GameError { code: 99 }));
                    continue;
                };
                if calls.in_flight::<Transfer>(r.owner) {
                    calls.reply(r.reply, Err(GameError { code: BUSY }));
                } else if w.gold - w.reserved < r.body.amount {
                    calls.reply(r.reply, Err(GameError { code: INSUFFICIENT }));
                } else {
                    w.reserved += r.body.amount;
                    let spec = CallSpec::new(
                        &self.bank,
                        Transfer {
                            to: r.body.to,
                            amount: r.body.amount,
                        },
                        BankMsg::Transfer,
                    );
                    calls
                        .call(spec)
                        .owner(r.owner)
                        .with(Reserve(r.body.amount))
                        .answers(r.reply);
                }
            }
            for r in calls.take_incoming::<Ping>() {
                let _ = self
                    .seen
                    .send(Seen::Request(1007, r.from.as_str().to_owned(), r.owner));
                let n = r.body.n;
                calls.reply(r.reply, Ok(Ping { n }));
            }
        }
        for (p, w) in &self.wallets {
            let _ = self.seen.send(Seen::Wallet(*p, *w));
        }
    }
}

impl RoomLogic<()> for Game {}

// ---- the rig ----------------------------------------------------------------

/// A room over [`Game`], a bank, and what the game saw.
pub(super) struct Rig {
    pub room: RoomActor<(), (), ()>,
    pub bank: Bank,
    pub friends: Option<crate::channel::Inbox<Mail<FriendsMsg>>>,
    pub seen: smpsc::Receiver<Seen>,
    pub tick: u64,
}

pub(super) struct RigCfg {
    pub mode: Mode,
    pub bank_capacity: usize,
    pub bank_delay: u32,
    pub share: u32,
    pub friends: bool,
}

impl Default for RigCfg {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            bank_capacity: 16,
            bank_delay: 1,
            share: 64,
            friends: false,
        }
    }
}

impl Rig {
    pub(super) fn new(cfg: RigCfg) -> Self {
        let (route, bank) = Bank::new(cfg.bank_capacity, cfg.bank_delay);
        let (friends_route, friends) = if cfg.friends {
            let (r, inbox) = Route::new("friends", 16);
            (Some(r), Some(inbox))
        } else {
            (None, None)
        };
        let (tx, seen) = smpsc::channel();
        let game = Game {
            bank: route,
            friends: friends_route,
            mode: cfg.mode,
            wallets: BTreeMap::new(),
            seen: tx,
            boot_twice: false,
        };
        Self {
            room: room_with(game, cfg.share),
            bank,
            friends,
            seen,
            tick: 0,
        }
    }

    /// Join `conn` under `identity` (an identified join: the resume path
    /// falls back to a fresh join).
    pub(super) fn join(&mut self, conn: u64, identity: &str) -> Joined {
        let (out, rx) = mpsc::channel::<FrameBatch>(64);
        let (reply, mut replied) = oneshot::channel();
        self.room.handle_control(RoomControl::Resume {
            conn: ConnectionId(conn),
            epoch: 0,
            identity: identity.to_owned(),
            claims: None,
            out,
            reply,
        });
        let (_entity, actions) = replied
            .try_recv()
            .expect("reply sent synchronously")
            .expect("join accepted");
        Joined {
            conn,
            actions,
            out: rx,
        }
    }

    pub(super) fn leave(&mut self, conn: u64) {
        self.room.handle_control(RoomControl::Leave {
            conn: ConnectionId(conn),
            entity: conn,
        });
    }

    /// One room step, the services left alone.
    pub(super) fn step_room_only(&mut self) {
        self.tick += 1;
        let at = Instant::now() + Duration::from_secs_f64(self.tick as f64 / 30.0);
        assert!(self.room.step(&TickInfo {
            tick: self.tick,
            at
        }));
    }

    /// One room step, then the services drain (their answers land at the
    /// next step's RESPONSES).
    pub(super) fn step(&mut self) {
        self.step_room_only();
        self.bank.step();
    }

    /// Everything the game reported since the last call.
    pub(super) fn seen(&mut self) -> Vec<Seen> {
        self.seen.try_iter().collect()
    }

    /// The game's wallet of `p` as last reported.
    pub(super) fn wallet(&mut self, p: u64) -> Option<Wallet> {
        self.seen().into_iter().rev().find_map(|s| match s {
            Seen::Wallet(q, w) if q == PlayerId(p) => Some(w),
            _ => None,
        })
    }

    pub(super) fn counts(&self) -> crate::metrics::CallCounts {
        *self.room.calls.counts()
    }
}

pub(super) fn room_with(game: Game, share: u32) -> RoomActor<(), (), ()> {
    let (_tick_tx, tick_rx) = broadcast::channel(8);
    let (_control, control_rx) = channel(16);
    RoomActor::new(
        RoomConfig {
            id: RoomId(77),
            tick_hz: 30.0,
            keepalive_hz: 0.0,
            metrics_cadence_hz: 0.0,
            call_share: share,
            ..Default::default()
        },
        (),
        Box::new(game),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
        None,
    )
}

/// A joined session: its input and its output.
pub(super) struct Joined {
    pub conn: u64,
    pub actions: Mailbox<Action>,
    pub out: mpsc::Receiver<FrameBatch>,
}

/// One answer as the client decodes it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Answer {
    pub id: u64,
    pub ok: bool,
    pub op: u32,
    pub reason: String,
    pub payload: Vec<u8>,
    pub game_error: bool,
}

impl Joined {
    /// Send one correlated request.
    pub(super) fn request<R: prost::Message>(&self, id: u64, op: u16, body: &R) {
        self.raw(id, op, body.encode_to_vec());
    }

    pub(super) fn raw(&self, id: u64, op: u16, payload: Vec<u8>) {
        let env = gsb_protocol::base::RpcRequest {
            id,
            op: u32::from(op),
            payload,
        };
        self.actions
            .try_send(Action {
                conn: ConnectionId(self.conn),
                player: PlayerId(0),
                op: RPC_REQ_OP,
                payload: env.encode_to_vec().into(),
            })
            .expect("input has room");
    }

    /// Every answer delivered since the last call, in order.
    pub(super) fn answers(&mut self) -> Vec<Answer> {
        let mut out = Vec::new();
        while let Ok(batch) = self.out.try_recv() {
            for f in batch.iter().filter(|f| f.op == OP_PRIV) {
                let mut buf = &f.payload[..];
                while !buf.is_empty() {
                    let r = gsb_protocol::base::RpcResponse::decode_length_delimited(&mut buf)
                        .expect("a length-delimited response");
                    out.push(Answer {
                        id: r.id,
                        ok: r.ok,
                        op: r.op,
                        reason: r.reason,
                        payload: r.payload,
                        game_error: r.game_error,
                    });
                }
            }
        }
        out
    }
}

pub(super) fn transfer(to: &str, amount: u64) -> TransferGold {
    TransferGold {
        to: to.to_owned(),
        amount,
    }
}

pub(super) fn balance(a: &Answer) -> u64 {
    assert!(a.ok, "{a:?}");
    TransferDone::decode(&a.payload[..])
        .expect("TransferDone")
        .balance
}

pub(super) fn game_error(a: &Answer) -> u32 {
    assert!(
        !a.ok && a.game_error && a.reason.is_empty(),
        "a typed game error: {a:?}"
    );
    GameError::decode(&a.payload[..]).expect("GameError").code
}

/// A boot that claims one op twice is refused with the collision; the
/// room is built anyway (the first claim stays).
#[test]
fn an_op_claimed_twice_is_a_boot_error_and_the_first_claim_stays() {
    let (route, _bank) = Bank::new(4, 1);
    let (tx, _seen) = smpsc::channel();
    let mut game = Game {
        bank: route,
        friends: None,
        mode: Mode::default(),
        wallets: BTreeMap::new(),
        seen: tx,
        boot_twice: true,
    };
    let mut calls = Calls::default();
    let err = game.boot_calls(&mut calls).expect_err("collision");
    let BootError::OpCollision { op, first, second } = err else {
        panic!("expected a collision: {err:?}")
    };
    assert_eq!(op, 1001);
    assert!(first.ends_with("TransferGold") && second.ends_with("TransferGold"));
    assert!(calls.handles(1001));
    let _room = room_with(game, 64);
}

/// The migration path is untouched: an op the table does not claim still
/// reaches `handle_request`, answered in the same tick.
#[test]
fn an_unclaimed_op_still_reaches_handle_request() {
    let mut rig = Rig::new(RigCfg::default());
    let mut a = rig.join(1, "alice");
    a.raw(5, OP_LEGACY, Vec::new());
    a.raw(6, 0x0999, Vec::new());
    rig.step();
    let got = a.answers();
    assert_eq!(got.len(), 2);
    assert!(got[0].ok && got[0].payload == b"legacy" && got[0].id == 5);
    assert_eq!(got[1].reason, crate::rpc::no_handler_reason(0x0999));
    assert_eq!(rig.room.m.requests_local, 1);
    assert_eq!(rig.room.m.requests_rejected_no_handler, 1);
}
