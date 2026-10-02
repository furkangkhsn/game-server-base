//! The path budget's gate on the fan-out (BACKLOG B103,
//! `GameLogic::ship_snapshot`): only a member whose path is limited is
//! asked, with its group frame's size and its budget; a frame the logic
//! withholds leaves that member's batch (its private frame still goes)
//! and is counted; a logic that does not opt in ships as always.

use super::*;
use crate::path::{PathPhase, PathState, path_action};

const SNAP: u16 = 0x7520;
const PRIV: u16 = 0x7521;
/// The group frame's size: every tick changes the group.
const FRAME: usize = 1_200;

/// What the gate was asked: `(player, bytes, budget)`.
type Asked = (PlayerId, usize, usize);

struct Gated {
    /// What the gate answers.
    answer: bool,
    asked: mpsc::Sender<Asked>,
}

impl GameLogic<()> for Gated {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        SNAP
    }
    fn private_op(&self) -> u16 {
        PRIV
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _b: &[crate::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        out.extend_from_slice(&[7; FRAME]);
        true
    }
    fn private(
        &mut self,
        _w: &mut (),
        player: PlayerId,
        _g: &(),
        _r: &[crate::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        out.extend_from_slice(&[player.0 as u8]);
        true
    }
    fn ship_snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        player: PlayerId,
        _g: &(),
        bytes: usize,
        budget: usize,
    ) -> bool {
        let _ = self.asked.try_send((player, bytes, budget));
        self.answer
    }
    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(conn.0),
            entity: conn.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}

impl RoomLogic<()> for Gated {}

struct Rig {
    actor: RoomActor<(), (), ()>,
    asked: mpsc::Receiver<Asked>,
    t0: Instant,
}

impl Rig {
    fn new(answer: bool) -> Self {
        let (asked_tx, asked) = mpsc::channel(64);
        let (_tick_tx, tick_rx) = broadcast::channel(4);
        let (_control, control_rx) = channel(16);
        let actor = RoomActor::new(
            RoomConfig {
                id: RoomId(62),
                keepalive_hz: 0.0,
                ..Default::default()
            },
            (),
            Box::new(Gated {
                answer,
                asked: asked_tx,
            }),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
            None,
        );
        Self {
            actor,
            asked,
            t0: Instant::now(),
        }
    }

    fn join(&mut self, conn: u64) -> (Mailbox<Action>, mpsc::Receiver<FrameBatch>) {
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
        let (rtx, mut rrx) = oneshot::channel();
        self.actor.handle_control(RoomControl::Join {
            conn: ConnectionId(conn),
            out: out_tx,
            reply: rtx,
        });
        let (_e, actions) = rrx.try_recv().expect("reply").expect("joined");
        (actions, out_rx)
    }

    fn step(&mut self, tick: u64) {
        self.actor.step_phases(&TickInfo {
            tick,
            at: self.t0 + Duration::from_millis(33 * tick),
        });
    }

    fn asked(&mut self) -> Vec<Asked> {
        let mut v = Vec::new();
        while let Ok(a) = self.asked.try_recv() {
            v.push(a);
        }
        v
    }
}

fn paced(rate: u32) -> PathState {
    PathState {
        phase: PathPhase::Paced,
        rate: Some(rate),
        ..Default::default()
    }
}

/// The ops of every batch queued for one member.
fn ops(rx: &mut mpsc::Receiver<FrameBatch>) -> Vec<Vec<u16>> {
    let mut v = Vec::new();
    while let Ok(b) = rx.try_recv() {
        v.push(b.iter().map(|f| f.op).collect());
    }
    v
}

#[test]
fn a_limited_member_is_asked_and_a_withheld_frame_leaves_only_its_batch() {
    let mut r = Rig::new(false);
    let (limited, mut limited_out) = r.join(1);
    let (_open, mut open_out) = r.join(2);
    let (known_open, mut known_open_out) = r.join(3);
    limited
        .try_send(path_action(ConnectionId(1), &paced(30_000)))
        .expect("room");
    known_open
        .try_send(path_action(ConnectionId(3), &PathState::default()))
        .expect("room");
    r.step(1);
    // Only the limited member was asked, with the frame's size and its
    // budget (30 000 B/s over the 30 Hz tick).
    assert_eq!(r.asked(), vec![(PlayerId(1), FRAME, 999)]);
    assert_eq!(
        ops(&mut limited_out),
        vec![vec![PRIV]],
        "withheld: private only"
    );
    assert_eq!(
        ops(&mut open_out),
        vec![vec![SNAP, PRIV]],
        "unknown path: as always"
    );
    assert_eq!(
        ops(&mut known_open_out),
        vec![vec![SNAP, PRIV]],
        "a path that keeps up: as always"
    );
    assert_eq!(r.actor.m.snapshots_withheld, 1);
    assert_eq!(r.actor.sample().snapshots_withheld, 1);
    // Not a failed send, and not shipped traffic.
    assert_eq!(r.actor.m.dropped_frames + r.actor.m.sends_closed, 0);
    assert_eq!(
        r.actor.m.shipped_frames, 5,
        "two full batches and one private"
    );
}

#[test]
fn a_frame_the_logic_ships_goes_and_is_not_counted() {
    let mut r = Rig::new(true);
    let (limited, mut out) = r.join(1);
    limited
        .try_send(path_action(ConnectionId(1), &paced(30_000)))
        .expect("room");
    r.step(1);
    r.step(2);
    assert_eq!(r.asked().len(), 2, "asked once per tick");
    assert_eq!(ops(&mut out), vec![vec![SNAP, PRIV], vec![SNAP, PRIV]]);
    assert_eq!(r.actor.m.snapshots_withheld, 0);
}

/// The trait's default ships: a limited member of a logic that does not
/// opt in gets every frame, nothing is withheld.
#[test]
fn the_default_hook_ships_every_frame() {
    struct Default;
    impl GameLogic<()> for Default {
        type GroupKey = ();
        type Strip = ();
        fn snapshot_op(&self) -> u16 {
            SNAP
        }
        fn private_op(&self) -> u16 {
            PRIV
        }
        fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
        fn snapshot(
            &mut self,
            _w: &mut (),
            _c: &TickCtx,
            _g: &(),
            _b: &[crate::shard::BorderRecord<()>],
            out: &mut bytes::BytesMut,
        ) -> bool {
            out.extend_from_slice(&[1; FRAME]);
            true
        }
        fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
            Admission {
                player: PlayerId(conn.0),
                entity: conn.0,
            }
        }
        fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
        fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
            a.clear();
        }
        fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
    }
    impl RoomLogic<()> for Default {}
    let mut r = Rig::new(true);
    r.actor.logic = Box::new(Default);
    let (limited, mut out) = r.join(1);
    limited
        .try_send(path_action(ConnectionId(1), &paced(1_000)))
        .expect("room");
    r.step(1);
    assert_eq!(ops(&mut out), vec![vec![SNAP]]);
    assert_eq!(r.actor.m.snapshots_withheld, 0);
}
