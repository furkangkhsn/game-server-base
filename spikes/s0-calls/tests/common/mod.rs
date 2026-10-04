//! A tiny game for the spike tests: a shop room that buys through an
//! economy service with reserve/commit, plus that service's ledger.
#![allow(dead_code)]

use std::collections::HashMap;

use prost::Message;
use tokio::sync::mpsc;

use s0_calls::calls::{CallError, CallKey, Done, Outcome};
use s0_calls::fake::Scheduler;
use s0_calls::push::PushIn;
use s0_calls::request::{ClientCall, ConnId, Event, Request, RequestTable};
use s0_calls::room::{Action, Inbound, Logic, TickCtx};
use s0_calls::service::{Call, ServiceRef};
use s0_calls::wire::{self, Frame, RpcResponse};

// ---- the shared contract (server and client use the same types) ----

#[derive(Clone, PartialEq, prost::Message)]
pub struct BuyItem {
    #[prost(string, tag = "1")]
    pub item: String,
    #[prost(uint64, tag = "2")]
    pub price: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct BuyResult {
    #[prost(uint64, tag = "1")]
    pub balance: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct EconomyError {
    #[prost(uint32, tag = "1")]
    pub code: u32,
}

impl Request for BuyItem {
    const OP: u16 = 1001;
    type Response = BuyResult;
    type Error = EconomyError;
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrantGold {
    #[prost(uint64, tag = "1")]
    pub amount: u64,
}

impl Request for GrantGold {
    const OP: u16 = 1002;
    type Response = BuyResult;
    type Error = EconomyError;
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct BalanceChanged {
    #[prost(uint64, tag = "1")]
    pub balance: u64,
}

impl Event for BalanceChanged {
    const OP: u16 = 2001;
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FriendOnline {
    #[prost(string, tag = "1")]
    pub name: String,
}

impl Event for FriendOnline {
    const OP: u16 = 2002;
}

pub mod code {
    pub const INSUFFICIENT: u32 = 1;
    pub const BUSY: u32 = 3;
    pub const UNAVAILABLE: u32 = 4;
}

// ---- the economy service (game code) ----

pub enum EconomyMsg {
    Buy(Call<BuyItem>),
    Grant(Call<GrantGold>),
}

impl From<Call<BuyItem>> for EconomyMsg {
    fn from(c: Call<BuyItem>) -> Self {
        Self::Buy(c)
    }
}

impl From<Call<GrantGold>> for EconomyMsg {
    fn from(c: Call<GrantGold>) -> Self {
        Self::Grant(c)
    }
}

/// The service's authority: the balance, the charges it made, and the
/// result it gave each key (the idempotency table).
#[derive(Debug, Default)]
pub struct Ledger {
    pub balance: u64,
    pub charges: u64,
    pub keys_seen: u64,
    seen: HashMap<CallKey, Result<BuyResult, EconomyError>>,
}

impl Ledger {
    pub fn with_balance(balance: u64) -> Self {
        Self {
            balance,
            ..Self::default()
        }
    }

    fn once(
        &mut self,
        key: CallKey,
        f: impl FnOnce(&mut Self) -> Result<BuyResult, EconomyError>,
    ) -> Result<BuyResult, EconomyError> {
        if let Some(r) = self.seen.get(&key) {
            return r.clone();
        }
        self.keys_seen += 1;
        let r = f(self);
        self.seen.insert(key, r.clone());
        r
    }

    /// The service body: decide now (charge), answer `delay` ticks later.
    pub fn handle(&mut self, delay: u64, msg: EconomyMsg, s: &mut Scheduler<'_>) {
        match msg {
            EconomyMsg::Buy(call) => {
                let price = call.req.price;
                let r = self.once(call.key, |l| {
                    if l.balance >= price {
                        l.balance -= price;
                        l.charges += 1;
                        Ok(BuyResult { balance: l.balance })
                    } else {
                        Err(EconomyError {
                            code: code::INSUFFICIENT,
                        })
                    }
                });
                let reply = call.reply;
                s.after(delay, move || {
                    let _ = reply.send(r);
                });
            }
            EconomyMsg::Grant(call) => {
                let amount = call.req.amount;
                let r = self.once(call.key, |l| {
                    l.balance += amount;
                    Ok(BuyResult { balance: l.balance })
                });
                let reply = call.reply;
                s.after(delay, move || {
                    let _ = reply.send(r);
                });
            }
        }
    }
}

// ---- the shop room (game code) ----

pub enum ShopReq {
    Buy(ClientCall<BuyItem>, BuyItem),
}

pub enum ShopDone {
    /// A client's purchase: answer the client when it settles.
    Bought {
        client: ClientCall<BuyItem>,
        price: u64,
        outcome: Outcome<BuyItem>,
    },
    /// A purchase re-issued after a migration (no client to answer: the
    /// session-scoped request did not travel).
    Settled {
        price: u64,
        outcome: Outcome<BuyItem>,
    },
    Granted(Outcome<GrantGold>),
}

pub struct Shop {
    pub econ: ServiceRef<EconomyMsg>,
    /// The room's cache of the balance (the ledger is the authority).
    pub balance: u64,
    /// Reserved by purchases in flight (written synchronously at call).
    pub reserved: u64,
    pub trace: Vec<(u64, &'static str)>,
    pub keys: Vec<CallKey>,
    pub grant_on_update: Option<u64>,
    /// Purchases to re-issue under their original key (a migrate-in).
    pub reissue: Vec<(CallKey, BuyItem)>,
    pub settled: Vec<Result<u64, CallError<EconomyError>>>,
    pub undecodable_pushes: u64,
}

impl Shop {
    pub fn new(econ: ServiceRef<EconomyMsg>, balance: u64) -> Self {
        Self {
            econ,
            balance,
            reserved: 0,
            trace: Vec::new(),
            keys: Vec::new(),
            grant_on_update: None,
            reissue: Vec::new(),
            settled: Vec::new(),
            undecodable_pushes: 0,
        }
    }

    pub fn trace_at(&self, tick: u64) -> Vec<&'static str> {
        self.trace
            .iter()
            .filter(|(t, _)| *t == tick)
            .map(|(_, s)| *s)
            .collect()
    }

    fn buy(&mut self, ctx: &TickCtx<'_, ShopDone>, client: ClientCall<BuyItem>, buy: BuyItem) {
        let price = buy.price;
        if self.balance.saturating_sub(self.reserved) < price {
            ctx.reply(
                client,
                Err(EconomyError {
                    code: code::INSUFFICIENT,
                }),
            );
            return;
        }
        // Reserve BEFORE the call leaves: a second request in a later tick
        // sees the money as spoken for.
        self.reserved += price;
        match ctx.call(&self.econ, buy, move |outcome| ShopDone::Bought {
            client,
            price,
            outcome,
        }) {
            Ok(key) => self.keys.push(key),
            Err(refused) => {
                let done = refused.fail();
                self.settle(ctx, done);
            }
        }
    }

    fn settle(&mut self, ctx: &TickCtx<'_, ShopDone>, done: ShopDone) {
        match done {
            ShopDone::Bought {
                client,
                price,
                outcome,
            } => {
                self.reserved -= price;
                match outcome {
                    Ok(r) => {
                        self.balance = r.balance;
                        ctx.reply(client, Ok(r));
                    }
                    Err(CallError::Service(e)) => ctx.reply(client, Err(e)),
                    Err(CallError::Refused(_)) => {
                        ctx.reply(client, Err(EconomyError { code: code::BUSY }))
                    }
                    Err(CallError::TimedOut | CallError::Abandoned) => ctx.reply(
                        client,
                        Err(EconomyError {
                            code: code::UNAVAILABLE,
                        }),
                    ),
                }
            }
            ShopDone::Settled { price, outcome } => {
                self.reserved -= price;
                match outcome {
                    Ok(r) => {
                        self.balance = r.balance;
                        self.settled.push(Ok(r.balance));
                    }
                    Err(e) => self.settled.push(Err(e)),
                }
            }
            ShopDone::Granted(Ok(r)) => self.balance = r.balance,
            ShopDone::Granted(Err(_)) => {}
        }
    }
}

impl Logic for Shop {
    type Done = ShopDone;
    type Req = ShopReq;

    fn on_done(&mut self, ctx: &TickCtx<'_, ShopDone>, done: &mut Vec<Done<ShopDone>>) {
        self.trace.push((ctx.tick, "done"));
        for d in done.drain(..) {
            self.settle(ctx, d.msg);
        }
    }

    fn on_events(&mut self, ctx: &TickCtx<'_, ShopDone>, events: &mut Vec<PushIn>) {
        self.trace.push((ctx.tick, "events"));
        for e in events.drain(..) {
            match e.decode::<BalanceChanged>() {
                Some(b) => self.balance = b.balance,
                None => self.undecodable_pushes += 1,
            }
        }
    }

    fn ingest(&mut self, ctx: &TickCtx<'_, ShopDone>, actions: &mut Vec<Action>) {
        self.trace.push((ctx.tick, "ingest"));
        actions.clear();
    }

    fn on_request(&mut self, ctx: &TickCtx<'_, ShopDone>, req: ShopReq) {
        self.trace.push((ctx.tick, "request"));
        match req {
            ShopReq::Buy(client, buy) => self.buy(ctx, client, buy),
        }
    }

    fn update(&mut self, ctx: &TickCtx<'_, ShopDone>) {
        self.trace.push((ctx.tick, "update"));
        if let Some(amount) = self.grant_on_update.take() {
            let _ = ctx.call(&self.econ, GrantGold { amount }, ShopDone::Granted);
        }
        for (key, buy) in std::mem::take(&mut self.reissue) {
            let price = buy.price;
            self.reserved += price;
            if let Err(refused) = ctx.call_keyed(&self.econ, key, buy, move |outcome| {
                ShopDone::Settled { price, outcome }
            }) {
                let done = refused.fail();
                self.settle(ctx, done);
            }
        }
    }
}

pub fn shop_table() -> RequestTable<ShopReq> {
    let mut t = RequestTable::default();
    t.add::<BuyItem>(ShopReq::Buy).expect("first registration");
    t
}

// ---- client-side helpers ----

/// What the connection actor forwards for an `RPC_REQ` frame.
pub fn rpc_action<R: Request>(conn: ConnId, id: u64, req: &R) -> Inbound {
    Inbound::Action(Action {
        conn,
        op: wire::op::RPC_REQ,
        payload: wire::RpcRequest {
            id,
            op: u32::from(R::OP),
            payload: req.encode_to_vec(),
            target: wire::target::ROOM,
        }
        .encode_to_vec(),
    })
}

pub fn plain_action(conn: ConnId) -> Inbound {
    Inbound::Action(Action {
        conn,
        op: 1000,
        payload: Vec::new(),
    })
}

/// Every correlated answer received so far, in order.
pub fn responses(rx: &mut mpsc::Receiver<Frame>) -> Vec<RpcResponse> {
    let mut out = Vec::new();
    while let Ok(f) = rx.try_recv() {
        match f {
            Frame::Private { responses } => out.extend(responses),
            Frame::RpcResp(r) => out.push(r),
            Frame::Push(_) => {}
        }
    }
    out
}

pub fn decode_err(r: &RpcResponse) -> EconomyError {
    assert!(!r.ok && r.game_error);
    EconomyError::decode(&r.payload[..]).expect("typed error")
}

pub fn decode_ok(r: &RpcResponse) -> BuyResult {
    assert!(r.ok);
    BuyResult::decode(&r.payload[..]).expect("typed response")
}
