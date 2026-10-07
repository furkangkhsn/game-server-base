//! The test game and its services.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use prost::Message;

use crate::calls::{Ask, BootError, CallSpec, Calls, Envelope, Route};
use crate::determinism::StateHasher;
use crate::id::{ConnectionId, PlayerId};
use crate::room::{Action, Admission, GameLogic, RoomLogic, TickCtx};

pub(super) const OP_SNAP: u16 = 0x7160;
pub(super) const OP_PRIV: u16 = 0x7161;
pub(super) const NOT_LOADED: u32 = 1;
pub(super) const BUSY: u32 = 2;
pub(super) const POOR: u32 = 3;
/// A tip: an action that adds one gold (`ingest`).
pub(super) const OP_TIP: u16 = 0x7170;
/// A legacy request (`handle_request` → `External`): answered `pong`.
pub(super) const OP_PING: u16 = 0x0003;

// ---- the wire request ----------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Buy {
    #[prost(uint64, tag = "1")]
    pub price: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Bought {
    #[prost(uint64, tag = "1")]
    pub gold: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ShopError {
    #[prost(uint32, tag = "1")]
    pub code: u32,
}
impl crate::calls::Call for Buy {
    type Response = Bought;
    type Error = ShopError;
}
impl crate::calls::Request for Buy {
    const OP: u16 = 1101;
}

// ---- the services' calls ---------------------------------------------------------

/// The bank: charge `price` from `have` (a pure fake — no state to keep).
#[derive(Clone, Debug)]
pub(super) struct Charge {
    pub price: u64,
    pub have: u64,
}
impl crate::calls::Call for Charge {
    type Response = Bought;
    type Error = ShopError;
}
pub(super) enum BankMsg {
    Charge(Ask<Charge>),
}

/// The inventory: load a player's bag (its item count).
#[derive(Clone, Debug)]
pub(super) struct Load {
    pub who: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Bag {
    #[prost(uint64, tag = "1")]
    pub items: u64,
}
impl crate::calls::Call for Load {
    type Response = Bag;
    type Error = ShopError;
}
pub(super) enum InvMsg {
    Load(Ask<Load>),
}

/// The bank's answer to one message.
pub(super) fn bank_answer(env: Envelope<BankMsg>) {
    let BankMsg::Charge(ask) = env.msg;
    let c = ask.call.clone();
    if c.have < c.price {
        ask.answer(Err(ShopError { code: POOR }));
    } else {
        ask.answer(Ok(Bought {
            gold: c.have - c.price,
        }));
    }
}

/// The inventory's answer to one message.
pub(super) fn inv_answer(env: Envelope<InvMsg>) {
    let InvMsg::Load(ask) = env.msg;
    let items = ask.call.who * 10;
    ask.answer(Ok(Bag { items }));
}

// ---- the game ------------------------------------------------------------------

thread_local! {
    /// How many times this thread's `Shop`s were asked their hash.
    pub(super) static HASHED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(super) struct Player {
    pub gold: u64,
    pub reserved: u64,
    pub items: Option<u64>,
}

/// The world: the players, by id (ordered: the hash iterates it).
pub(super) type World = BTreeMap<PlayerId, Player>;

/// The reserve a charge carries back.
pub(super) struct Reserve(pub u64);

pub(super) struct Shop {
    pub bank: Route<BankMsg>,
    pub inv: Route<InvMsg>,
    /// The sum of every roll of the room's dice.
    pub rolls: u64,
}

impl GameLogic<World> for Shop {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        OP_SNAP
    }
    fn private_op(&self) -> u16 {
        OP_PRIV
    }
    fn group_of(&self, _w: &World, _p: PlayerId) {}
    fn snapshot(
        &mut self,
        _w: &mut World,
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
        _w: &mut World,
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
    fn on_join(&mut self, w: &mut World, c: ConnectionId) -> Admission {
        w.insert(
            PlayerId(c.0),
            Player {
                gold: 500,
                ..Player::default()
            },
        );
        Admission {
            player: PlayerId(c.0),
            entity: c.0,
        }
    }
    fn on_leave(&mut self, w: &mut World, p: PlayerId) {
        w.remove(&p);
    }
    fn ingest(&mut self, w: &mut World, _c: &TickCtx, a: &mut Vec<Action>) {
        for a in a.drain(..) {
            if let (OP_TIP, Some(p)) = (a.op, w.get_mut(&a.player)) {
                p.gold += 1;
            }
        }
    }
    fn handle_request(
        &mut self,
        _w: &mut World,
        _c: &TickCtx,
        req: &crate::rpc::RpcRequest,
    ) -> Option<crate::rpc::RequestDecision> {
        (req.op == OP_PING).then(|| {
            crate::rpc::RequestDecision::External(Box::pin(async {
                Ok(bytes::Bytes::from_static(b"pong"))
            }))
        })
    }
    fn update(&mut self, _w: &mut World, _c: &TickCtx) {
        unreachable!("the actor calls update_with_calls");
    }
    fn boot_calls(&mut self, calls: &mut Calls) -> Result<(), BootError> {
        calls.accept::<Buy>()?;
        #[cfg(feature = "record")]
        {
            calls.record_as::<Charge>();
            calls.record_as::<Load>();
        }
        Ok(())
    }
    fn owner_alive(&self, w: &World, o: crate::calls::Owner) -> Option<bool> {
        Some(w.contains_key(&PlayerId(o.0)))
    }
    fn update_with_calls(&mut self, w: &mut World, ctx: &TickCtx, calls: &mut Calls) {
        // The dice: one roll per tick from the room's generator.
        self.rolls += ctx.rng.below(6) + 1;
        for r in calls.take_resolved::<Load, ()>() {
            if let (Some(p), Ok(bag)) = (w.get_mut(&PlayerId(r.owner.0)), &r.outcome) {
                p.items = Some(bag.items);
            }
        }
        for r in calls.take_resolved::<Charge, Reserve>() {
            if let Some(p) = w.get_mut(&PlayerId(r.owner.0)) {
                p.reserved -= r.ctx.0;
                if let Ok(b) = &r.outcome {
                    p.gold = b.gold;
                }
            }
        }
        for r in calls.take_incoming::<Buy>() {
            let Some(p) = w.get_mut(&r.player) else {
                calls.reply(r.reply, Err(ShopError { code: 99 }));
                continue;
            };
            if p.items.is_none() {
                calls.reply(r.reply, Err(ShopError { code: NOT_LOADED }));
            } else if calls.in_flight::<Charge>(r.owner) {
                calls.reply(r.reply, Err(ShopError { code: BUSY }));
            } else {
                p.reserved += r.body.price;
                let spec = CallSpec::new(
                    &self.bank,
                    Charge {
                        price: r.body.price,
                        have: p.gold,
                    },
                    BankMsg::Charge,
                );
                calls
                    .call(spec)
                    .owner(r.owner)
                    .with(Reserve(r.body.price))
                    .answers(r.reply);
            }
        }
        for (id, p) in w.iter() {
            let owner = (*id).into();
            if p.items.is_none() && !calls.in_flight::<Load>(owner) {
                let spec = CallSpec::new(&self.inv, Load { who: id.0 }, InvMsg::Load);
                calls.call(spec).owner(owner);
            }
        }
    }
    fn state_hash(&mut self, w: &mut World) -> Option<u64> {
        HASHED.with(|n| n.set(n.get() + 1));
        let mut h = StateHasher::new();
        for (id, p) in w.iter() {
            id.0.hash(&mut h);
            p.hash(&mut h);
        }
        self.rolls.hash(&mut h);
        Some(h.finish())
    }
}

impl RoomLogic<World> for Shop {}

/// The answers a client got, decoded (`RpcResponse`s of the private
/// frames).
pub(super) fn answers(
    client: &mut crate::determinism::SimClient,
) -> Vec<gsb_protocol::base::RpcResponse> {
    let mut out = Vec::new();
    for f in client.frames().iter().filter(|f| f.op == OP_PRIV) {
        let mut buf = &f.payload[..];
        while !buf.is_empty() {
            out.push(
                gsb_protocol::base::RpcResponse::decode_length_delimited(&mut buf)
                    .expect("a length-delimited response"),
            );
        }
    }
    out
}

/// A typed answer's code (`Err`) or gold (`Ok`).
pub(super) fn verdict(r: &gsb_protocol::base::RpcResponse) -> Result<u64, u32> {
    if r.ok {
        Ok(Bought::decode(&r.payload[..]).expect("Bought").gold)
    } else {
        assert!(r.game_error, "a typed error: {r:?}");
        Err(ShopError::decode(&r.payload[..]).expect("ShopError").code)
    }
}
