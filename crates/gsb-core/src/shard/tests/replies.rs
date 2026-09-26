//! RPC answers on a dropped batch (F14) — the room's contract on the
//! sharded actor: the answers a dropped batch carried ride the next
//! accepted one, exactly once and in order; a congested connection owes
//! at most its in-flight cap; a leaving session takes them along.

use super::*;
use crate::rpc::{RPC_REQ_OP, RequestDecision, RpcRequest};
use prost::Message;

// The storm bound (the congested connection's refusals).
mod bound;

/// Answered locally, in the tick that processes it.
const OP_LOCAL: u16 = 0x01;
/// Delegated to a worker whose future never resolves (stays in flight).
const OP_EXT: u16 = 0x03;
const OP_SNAP: u16 = 0x71A0;
const OP_PRIV: u16 = 0x71A1;

/// A group frame every step; the private frame lists the answers' ids
/// (`u64` LE each), only when there are any.
struct ReplyLogic {
    next_wire: u64,
}

impl GameLogic<TWorld> for ReplyLogic {
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
        _g: &Self::GroupKey,
        _borrowed: &[BorderRecord<TStrip>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        out.extend_from_slice(&[0xAA]);
        true
    }

    fn private(
        &mut self,
        _w: &mut TWorld,
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
        _w: &mut TWorld,
        _c: &TickCtx,
        req: &RpcRequest,
    ) -> Option<RequestDecision> {
        match req.op {
            OP_LOCAL => Some(RequestDecision::Reply(bytes::Bytes::new())),
            OP_EXT => Some(RequestDecision::External(Box::pin(std::future::pending()))),
            _ => None,
        }
    }

    fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
        self.next_wire += 1;
        w.ents.insert(self.next_wire, (-5.0, 0.0, 0));
        Admission {
            player: PlayerId(conn.0),
            entity: self.next_wire,
        }
    }
    fn on_leave(&mut self, _w: &mut TWorld, _player: PlayerId) {}
    fn ingest(&mut self, _w: &mut TWorld, _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut TWorld, _c: &TickCtx) {}
}

impl ShardLogic<TWorld> for ReplyLogic {
    type State = TState;

    fn index(&self) -> usize {
        0
    }
    fn shard_count(&self) -> usize {
        1
    }
    fn serial_capacity(&self) -> u64 {
        SHARD_SERIAL_CAPACITY
    }
    fn serial_used(&self) -> u64 {
        self.next_wire
    }
    fn neighbors(&self) -> &[usize] {
        &[]
    }
    fn collect_migrations(&mut self, _w: &mut TWorld, _nb: usize) -> Vec<Migrating<TState>> {
        Vec::new()
    }
    fn on_migrate_in(&mut self, _w: &mut TWorld, _wire: u64, _s: TState, _p: Option<PlayerId>) {}
    fn on_migrate_out(&mut self, _w: &mut TWorld, _wire: u64) {}
    fn collect_border(&self, _w: &TWorld) -> Vec<BorderRecord<TStrip>> {
        Vec::new()
    }
    fn own_wires(&self, w: &TWorld) -> Vec<u64> {
        w.ents.keys().copied().collect()
    }
}

type Shard = ShardActor<TWorld, (), TState, TStrip>;

/// A bare shard over [`ReplyLogic`]: `cap` in-flight requests per
/// connection, `pull` actions pulled per connection per tick.
fn shard(cap: usize, pull: usize) -> Shard {
    let (_tick_tx, tick_rx) = broadcast::channel(64);
    let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
    ShardActor::new(
        RoomConfig {
            id: RoomId(20),
            tick_hz: 30.0,
            keepalive_hz: 0.0,
            metrics_cadence_hz: 0.0,
            max_pending_requests_per_conn: cap,
            max_actions_per_conn_per_tick: pull,
            ..Default::default()
        },
        0,
        TWorld::default(),
        Box::new(ReplyLogic { next_wire: 0 }),
        tick_rx,
        rx,
        Vec::new(),
        1,
        metrics_null(),
        None,
    )
}

/// Join `conn` with an outbound channel of `cap` batches: its wire, its
/// input, its outbound receiver.
fn join(
    a: &mut Shard,
    conn: u64,
    cap: usize,
) -> (u64, Mailbox<Action>, mpsc::Receiver<FrameBatch>) {
    let (out, rx) = mpsc::channel::<FrameBatch>(cap);
    let (reply, mut replied) = oneshot::channel();
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(conn),
            epoch: 1,
            identity: String::new(),
            out,
            reply,
        },
        1,
    ));
    let (wire, actions) = replied.try_recv().expect("sync reply").expect("join ok");
    (wire, actions, rx)
}

/// Send one request-op action: a correlated request, or (`id` = 0) an
/// envelope that cannot correlate.
fn request(tx: &Mailbox<Action>, conn: u64, id: u64, op: u16) {
    let env = gsb_protocol::base::RpcRequest {
        id,
        op: op as u32,
        payload: Vec::new(),
    };
    tx.try_send(Action {
        conn: ConnectionId(conn),
        player: PlayerId(0),
        op: RPC_REQ_OP,
        payload: env.encode_to_vec().into(),
    })
    .expect("input has room");
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

/// The room's scenario: steps 2 and 3 drop their answers, the first
/// accepted batch carries them — once, ahead of step 4's — and step 5
/// re-sends nothing.
#[tokio::test]
async fn a_dropped_answer_rides_the_next_accepted_batch_on_the_shard() {
    let mut a = shard(4, 16);
    let (_wire, tx, mut rx) = join(&mut a, 1, 1);
    for t in 1..=3 {
        request(&tx, 1, t, OP_LOCAL);
        assert!(a.step(&tinfo(t)));
    }
    assert_eq!(drain(&mut rx), [vec![1]], "steps 2 and 3 were dropped");
    request(&tx, 1, 4, OP_LOCAL);
    assert!(a.step(&tinfo(4)));
    assert_eq!(drain(&mut rx), [vec![2, 3, 4]]);
    assert!(a.step(&tinfo(5)));
    assert_eq!(drain(&mut rx), [Vec::<u64>::new()], "nothing re-sent");
    assert_eq!(a.m.dropped_frames, 2);
}

/// A drop on a step that owes no answer carries none: a delivered
/// answer is never sent a second time.
#[tokio::test]
async fn a_delivered_answer_is_not_resent_by_a_later_drop_on_the_shard() {
    let mut a = shard(4, 16);
    let (_wire, tx, mut rx) = join(&mut a, 1, 1);
    request(&tx, 1, 1, OP_LOCAL);
    for t in 1..=3 {
        assert!(a.step(&tinfo(t))); // 2 and 3: the group frame, dropped
    }
    assert_eq!(drain(&mut rx), [vec![1]]);
    assert!(a.step(&tinfo(4)));
    assert_eq!(drain(&mut rx), [Vec::<u64>::new()], "no duplicate");
}

/// A leaving connection takes its undelivered answers along.
#[tokio::test]
async fn a_leaving_connection_takes_its_undelivered_answers_along_on_the_shard() {
    let mut a = shard(4, 16);
    let (wire, tx, mut rx) = join(&mut a, 1, 1);
    request(&tx, 1, 1, OP_LOCAL);
    assert!(a.step(&tinfo(1)));
    request(&tx, 1, 2, OP_LOCAL);
    assert!(a.step(&tinfo(2)));
    assert!(a.queued.contains_key(&ConnectionId(1)), "2 is owed");
    assert!(a.handle_msg(
        ShardMsg::Leave {
            conn: ConnectionId(1),
            entity: wire,
            epoch: 1,
        },
        2,
    ));
    assert!(a.step(&tinfo(3)));
    assert!(a.queued.is_empty(), "nothing leaks");
    assert_eq!(drain(&mut rx), [vec![1]]);
}
