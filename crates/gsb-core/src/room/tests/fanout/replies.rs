//! RPC answers on a dropped batch (F14): the answers a batch carried are
//! the core's own — they go back to the front of the connection's queue
//! and ride the next accepted batch, exactly once and in order; a
//! connection whose batches keep dropping is let owe no more than its
//! in-flight cap (the storm bound); a session that ends takes its
//! undelivered answers with it.

use super::*;
use crate::room::actor::RoomActor;
use crate::rpc::{RPC_REQ_OP, RequestDecision, RpcRequest};
use prost::Message;

// The storm bound (the congested connection's refusals).
mod bound;
// A session that ends takes its undelivered answers along.
mod session;

/// Answered room-locally, in the tick that processes it.
const OP_LOCAL: u16 = 0x01;
/// Delegated to a worker whose future never resolves (stays in flight).
const OP_EXT: u16 = 0x03;
const OP_SNAP: u16 = 0x7050;
const OP_PRIV: u16 = 0x7051;

/// A group frame on every step (so a batch exists — and can drop — on a
/// step that owes no answer); the private frame lists the answers' ids
/// (`u64` LE each), and only when there are any.
struct ReplyLogic;

impl GameLogic<()> for ReplyLogic {
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
        _borrowed: &[crate::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        out.extend_from_slice(&[0xAA]);
        true
    }

    fn private(
        &mut self,
        _w: &mut (),
        _p: PlayerId,
        _g: &(),
        replies: &[crate::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        for r in replies {
            out.extend_from_slice(&r.id.to_le_bytes());
        }
        !replies.is_empty()
    }

    fn handle_request(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        req: &RpcRequest,
    ) -> Option<RequestDecision> {
        match req.op {
            OP_LOCAL => Some(RequestDecision::Reply(bytes::Bytes::new())),
            OP_EXT => Some(RequestDecision::External(Box::pin(std::future::pending()))),
            _ => None,
        }
    }

    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(c.0),
            entity: c.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn on_disconnect(&mut self, _w: &mut (), _p: PlayerId, _identity: &str) -> Detach {
        Detach::Hold {
            grace: Some(Duration::from_secs(60)),
            to: ExpireTo::Despawn,
        }
    }
    fn resume_lookup(&self, _w: &(), _identity: &str) -> ResumeFound {
        ResumeFound::Held(PlayerId(1))
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}

impl RoomLogic<()> for ReplyLogic {}

/// A bare room over [`ReplyLogic`]: `cap` in-flight requests per
/// connection, `pull` actions pulled per connection per tick.
fn room(cap: usize, pull: usize) -> (RoomActor<(), (), ()>, Mailbox<RoomControl>) {
    let (_tick_tx, tick_rx) = broadcast::channel(8);
    let (control, control_rx) = channel(16);
    let actor = RoomActor::new(
        RoomConfig {
            id: RoomId(41),
            tick_hz: 30.0,
            keepalive_hz: 0.0,
            metrics_cadence_hz: 0.0,
            max_pending_requests_per_conn: cap,
            max_actions_per_conn_per_tick: pull,
            ..Default::default()
        },
        (),
        Box::new(ReplyLogic),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
        None,
    );
    (actor, control)
}

/// Join `conn` with an outbound channel of `cap` batches.
fn join(
    actor: &mut RoomActor<(), (), ()>,
    conn: u64,
    cap: usize,
) -> (Mailbox<Action>, mpsc::Receiver<FrameBatch>) {
    let (out, rx) = mpsc::channel::<FrameBatch>(cap);
    let (reply, mut replied) = oneshot::channel();
    actor.handle_control(RoomControl::Join {
        conn: ConnectionId(conn),
        out,
        reply,
    });
    let (_entity, actions) = replied
        .try_recv()
        .expect("reply sent synchronously")
        .expect("join accepted");
    (actions, rx)
}

/// Send one correlated request on `conn`'s input.
fn request(tx: &Mailbox<Action>, conn: u64, id: u64, op: u16) {
    let env = gsb_protocol::base::RpcRequest {
        id,
        op: op as u32,
        payload: Vec::new(),
    };
    send(tx, conn, env.encode_to_vec());
}

fn send(tx: &Mailbox<Action>, conn: u64, payload: Vec<u8>) {
    tx.try_send(Action {
        conn: ConnectionId(conn),
        player: PlayerId(0),
        op: RPC_REQ_OP,
        payload: payload.into(),
    })
    .expect("input has room");
}

fn step(actor: &mut RoomActor<(), (), ()>, tick: u64) {
    let at = Instant::now() + Duration::from_secs_f64(tick as f64 / 30.0);
    assert!(actor.step(&TickInfo { tick, at }));
}

/// Every batch waiting on `rx`, as the answer ids its private frame
/// carried (empty = the group frame only).
fn drain(rx: &mut mpsc::Receiver<FrameBatch>) -> Vec<Vec<u64>> {
    let mut batches = Vec::new();
    while let Ok(batch) = rx.try_recv() {
        let ids = batch
            .iter()
            .filter(|f| f.op == OP_PRIV)
            .flat_map(|f| f.payload.chunks(8))
            .map(|c| u64::from_le_bytes(c.try_into().expect("8-byte id")))
            .collect();
        batches.push(ids);
    }
    batches
}

/// The channel holds one batch and is not read: the answers of steps 2
/// and 3 ride dropped batches and arrive — once, in order, ahead of
/// step 4's own — in the first batch the channel accepts; step 5 owes
/// nothing and re-sends nothing.
#[test]
fn a_dropped_answer_rides_the_next_accepted_batch_once() {
    let (mut a, _control) = room(4, 16);
    let (tx, mut rx) = join(&mut a, 1, 1);
    request(&tx, 1, 1, OP_LOCAL);
    step(&mut a, 1);
    request(&tx, 1, 2, OP_LOCAL);
    step(&mut a, 2);
    request(&tx, 1, 3, OP_LOCAL);
    step(&mut a, 3);
    assert_eq!(drain(&mut rx), [vec![1]], "steps 2 and 3 were dropped");
    request(&tx, 1, 4, OP_LOCAL);
    step(&mut a, 4);
    assert_eq!(drain(&mut rx), [vec![2, 3, 4]]);
    step(&mut a, 5);
    assert_eq!(drain(&mut rx), [Vec::<u64>::new()], "nothing re-sent");
    assert_eq!(a.m.dropped_frames, 2);
    assert!(a.queued.is_empty());
}

/// A drop on a step that owes no answer carries none: the answer an
/// earlier batch delivered is never sent a second time.
#[test]
fn a_delivered_answer_is_not_resent_by_a_later_drop() {
    let (mut a, _control) = room(4, 16);
    let (tx, mut rx) = join(&mut a, 1, 1);
    request(&tx, 1, 1, OP_LOCAL);
    step(&mut a, 1);
    step(&mut a, 2); // the group frame alone: dropped
    step(&mut a, 3); // dropped
    assert_eq!(drain(&mut rx), [vec![1]]);
    step(&mut a, 4);
    assert_eq!(drain(&mut rx), [Vec::<u64>::new()], "no duplicate");
    assert_eq!(a.m.dropped_frames, 2);
}
