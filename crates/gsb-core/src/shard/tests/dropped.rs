//! The shard's drop signal (F11) — the room's contract on the sharded
//! actor: a batch the fan-out could not deliver is reported to the
//! logic (the player, whether the group snapshot rode it) right after
//! that player's `private`, and so is the first delivered batch after
//! dropped ones (the resume); no other delivered batch is.

use super::*;
use crate::room::{Detach, ExpireTo, ResumeFound};

mod closed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Private(u64, u64),
    Dropped(u64, u64, bool),
    Resumed(u64, u64),
}

/// One group; the snapshot emits on ODD steps, the private frame on
/// every step.
struct DropLogic {
    step: u64,
    next_wire: u64,
    seen: mpsc::Sender<Seen>,
}

impl GameLogic<TWorld> for DropLogic {
    type GroupKey = ();
    type Strip = TStrip;

    fn snapshot_op(&self) -> u16 {
        0x7190
    }
    fn private_op(&self) -> u16 {
        0x7191
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
        if self.step.is_multiple_of(2) {
            return false;
        }
        out.extend_from_slice(&self.step.to_le_bytes());
        true
    }

    fn private(
        &mut self,
        _w: &mut TWorld,
        player: PlayerId,
        _g: &(),
        _replies: &[crate::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.seen
            .try_send(Seen::Private(player.0, self.step))
            .expect("log has room");
        out.extend_from_slice(&[0xCD]);
        true
    }

    fn on_batch_dropped(&mut self, _w: &mut TWorld, player: PlayerId, snapshot: bool) {
        self.seen
            .try_send(Seen::Dropped(player.0, self.step, snapshot))
            .expect("log has room");
    }

    fn on_batch_resumed(&mut self, _w: &mut TWorld, player: PlayerId) {
        self.seen
            .try_send(Seen::Resumed(player.0, self.step))
            .expect("log has room");
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
    fn on_disconnect(&mut self, _w: &mut TWorld, _p: PlayerId, _identity: &str) -> Detach {
        Detach::Hold {
            grace: Some(Duration::from_secs(60)),
            to: ExpireTo::Despawn,
        }
    }
    fn resume_lookup(&self, _w: &TWorld, _identity: &str) -> ResumeFound {
        ResumeFound::Held(PlayerId(1))
    }
    fn ingest(&mut self, _w: &mut TWorld, _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut TWorld, _c: &TickCtx) {
        self.step += 1;
    }
}

impl ShardLogic<TWorld> for DropLogic {
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

/// A bare shard over [`DropLogic`] and its log.
fn shard() -> (ShardActor<TWorld, (), TState, TStrip>, mpsc::Receiver<Seen>) {
    let (seen_tx, seen) = mpsc::channel(256);
    let (_tick_tx, tick_rx) = broadcast::channel(64);
    let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
    let a = ShardActor::new(
        RoomConfig {
            id: RoomId(19),
            tick_hz: 30.0,
            keepalive_hz: 0.0,
            metrics_cadence_hz: 0.0,
            ..Default::default()
        },
        0,
        TWorld::default(),
        Box::new(DropLogic {
            step: 0,
            next_wire: 0,
            seen: seen_tx,
        }),
        tick_rx,
        rx,
        Vec::new(),
        1,
        metrics_null(),
        None,
    );
    (a, seen)
}

/// The room test's scenario on a shard: player 1's channel holds one
/// batch and is not read, player 2's has room. Steps 2 (no group frame)
/// and 3 (group frame) are dropped for player 1 and reported right
/// after its `private`; step 4, once the channel was read, is reported
/// as the resume; step 5 is not.
#[tokio::test]
async fn a_dropped_batch_is_reported_on_the_shard_too() {
    let (mut a, mut seen) = shard();
    let mut outs = Vec::new();
    for (conn, cap) in [(1, 1), (2, 64)] {
        let (out, out_rx) = mpsc::channel::<FrameBatch>(cap);
        let (reply, replied) = oneshot::channel();
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
        replied.await.expect("join reply").expect("join ok");
        outs.push(out_rx);
    }
    for t in 1..=3u64 {
        assert!(a.step(&tinfo(t)));
    }
    assert!(outs[0].try_recv().is_ok(), "step 1's batch was delivered");
    assert!(a.step(&tinfo(4)));
    assert_eq!(outs[0].try_recv().expect("step 4 delivered").len(), 1);
    assert!(a.step(&tinfo(5))); // delivered again: no second resume
    assert_eq!(outs[0].try_recv().expect("step 5 delivered").len(), 2);

    let mut log = Vec::new();
    while let Ok(s) = seen.try_recv() {
        log.push(s);
    }
    let dropped: Vec<Seen> = log
        .iter()
        .copied()
        .filter(|s| !matches!(s, Seen::Private(..)))
        .collect();
    assert_eq!(
        dropped,
        [
            Seen::Dropped(1, 2, false),
            Seen::Dropped(1, 3, true),
            Seen::Resumed(1, 4)
        ],
        "exactly the two undelivered batches: {log:?}"
    );
    for (i, s) in log.iter().enumerate() {
        if let Seen::Dropped(p, at, _) | Seen::Resumed(p, at) = *s {
            assert_eq!(log[i - 1], Seen::Private(p, at), "{log:?}");
        }
    }
    assert_eq!(a.m.dropped_frames, 2, "the counter agrees");
    let mut delivered = 0;
    while outs[1].try_recv().is_ok() {
        delivered += 1;
    }
    assert_eq!(delivered, 5, "player 2 got every step");
}

/// A resumed session is a fresh transport: no resume is reported on it
/// for a run of drops on the old one.
#[tokio::test]
async fn a_resumed_session_starts_without_a_run_on_the_shard() {
    let (mut a, mut seen) = shard();
    let (out, _slow) = mpsc::channel::<FrameBatch>(1);
    let (reply, replied) = oneshot::channel();
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(1),
            epoch: 1,
            identity: "one".into(),
            out,
            reply,
        },
        1,
    ));
    let (wire, _actions) = replied.await.expect("join reply").expect("join ok");
    assert!(a.step(&tinfo(1)));
    assert!(a.step(&tinfo(2))); // dropped: the run begins
    assert!(a.handle_msg(
        ShardMsg::Detach {
            conn: ConnectionId(1),
            entity: wire,
            identity: "one".into(),
        },
        2,
    ));
    let (out, mut fresh) = mpsc::channel::<FrameBatch>(64);
    let (reply, replied) = oneshot::channel();
    assert!(a.handle_msg(
        ShardMsg::Resume {
            conn: ConnectionId(2),
            epoch: 2,
            identity: "one".into(),
            out,
            reply,
        },
        2,
    ));
    assert!(replied.await.expect("resume reply").is_ok());
    assert!(a.step(&tinfo(3)));
    assert!(fresh.try_recv().is_ok(), "delivered on the new transport");
    let mut log = Vec::new();
    while let Ok(s) = seen.try_recv() {
        log.push(s);
    }
    assert!(log.contains(&Seen::Dropped(1, 2, false)), "{log:?}");
    assert!(
        !log.iter().any(|s| matches!(s, Seen::Resumed(..))),
        "no resume reported: {log:?}"
    );
}
