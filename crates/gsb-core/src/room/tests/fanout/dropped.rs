//! The drop signal (F11): a batch the fan-out could not deliver is
//! reported to the logic — the player, whether the group snapshot rode
//! it — in the same fan-out iteration as that player's `private` call,
//! and a delivered batch is never reported.

use super::*;
use crate::room::actor::RoomActor;

/// What the logic saw, in call order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// `private(player)` on `step`.
    Private(u64, u64),
    /// `on_batch_dropped(player, snapshot)` on `step`.
    Dropped(u64, u64, bool),
}

/// One group; the snapshot emits on ODD steps only, the private frame
/// on every step — so a dropped batch with and without the group frame
/// both occur.
struct DropLogic {
    step: u64,
    seen: mpsc::Sender<Seen>,
}

impl GameLogic<()> for DropLogic {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        0x7040
    }
    fn private_op(&self) -> u16 {
        0x7041
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
        if self.step.is_multiple_of(2) {
            return false;
        }
        out.extend_from_slice(&self.step.to_le_bytes());
        true
    }

    fn private(
        &mut self,
        _w: &mut (),
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

    fn on_batch_dropped(&mut self, _w: &mut (), player: PlayerId, snapshot: bool) {
        self.seen
            .try_send(Seen::Dropped(player.0, self.step, snapshot))
            .expect("log has room");
    }

    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(c.0),
            entity: c.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {
        self.step += 1;
    }
}

impl RoomLogic<()> for DropLogic {}

fn join(actor: &mut RoomActor<(), (), ()>, conn: u64, cap: usize) -> mpsc::Receiver<FrameBatch> {
    let (out, rx) = mpsc::channel::<FrameBatch>(cap);
    let (reply, mut replied) = oneshot::channel();
    actor.handle_control(RoomControl::Join {
        conn: ConnectionId(conn),
        out,
        reply,
    });
    replied
        .try_recv()
        .expect("reply sent synchronously")
        .expect("join accepted");
    rx
}

/// Player 1's channel holds ONE batch and is not read: step 1 fills
/// it, steps 2 (no group frame) and 3 (group frame) are dropped and
/// reported — each right after player 1's own `private` of that step —
/// and once the channel is read, step 4 is delivered and not reported.
/// Player 2 (room to spare) is never reported.
#[test]
fn a_dropped_batch_is_reported_right_after_that_players_private() {
    let (seen_tx, mut seen) = mpsc::channel(256);
    let (_tick_tx, tick_rx) = broadcast::channel(8);
    let (_control, control_rx) = channel(16);
    let mut actor = RoomActor::new(
        RoomConfig {
            id: RoomId(40),
            tick_hz: 30.0,
            keepalive_hz: 0.0,
            metrics_cadence_hz: 0.0,
            ..Default::default()
        },
        (),
        Box::new(DropLogic {
            step: 0,
            seen: seen_tx,
        }),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
        None,
    );
    let mut slow = join(&mut actor, 1, 1);
    let mut fine = join(&mut actor, 2, 64);
    let t0 = Instant::now();
    let step = |actor: &mut RoomActor<(), (), ()>, tick: u64| {
        let at = t0 + Duration::from_secs_f64(tick as f64 / 30.0);
        assert!(actor.step(&TickInfo { tick, at }));
    };
    for tick in 1..=3 {
        step(&mut actor, tick);
    }
    assert!(slow.try_recv().is_ok(), "step 1's batch was delivered");
    step(&mut actor, 4);

    let mut log = Vec::new();
    while let Ok(s) = seen.try_recv() {
        log.push(s);
    }
    let dropped: Vec<Seen> = log
        .iter()
        .copied()
        .filter(|s| matches!(s, Seen::Dropped(..)))
        .collect();
    assert_eq!(
        dropped,
        [Seen::Dropped(1, 2, false), Seen::Dropped(1, 3, true)],
        "exactly the two undelivered batches, with what rode them: {log:?}"
    );
    for (i, s) in log.iter().enumerate() {
        if let Seen::Dropped(p, at, _) = *s {
            assert_eq!(
                log[i - 1],
                Seen::Private(p, at),
                "reported right after that player's private: {log:?}"
            );
        }
    }
    assert_eq!(actor.sample().dropped_frames, 2, "the counter agrees");
    assert_eq!(slow.try_recv().expect("step 4 delivered").len(), 1);
    assert!(slow.try_recv().is_err());
    let mut delivered = 0;
    while fine.try_recv().is_ok() {
        delivered += 1;
    }
    assert_eq!(delivered, 4, "player 2 got every step");
}
