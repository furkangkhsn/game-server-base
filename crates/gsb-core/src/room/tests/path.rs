//! A member's path in the room (BACKLOG B103, `crate::path`): the marker
//! the connection actor puts on the member's action channel reaches the
//! tick context as `TickCtx::{path, budget}` — never the game's ingest,
//! never the input-idle clock — and goes with the session.

use super::*;
use crate::path::{PathPhase, PathState, path_action};

/// What the logic saw for one member in `update`: `(player, path,
/// budget, since_input)`.
type PathObs = (PlayerId, Option<PathState>, Option<usize>, Option<Duration>);

struct PathLogic {
    players: Vec<PlayerId>,
    obs: mpsc::Sender<PathObs>,
    /// Every opcode the game ingested.
    ops: mpsc::Sender<u16>,
    decision: Detach,
}

impl GameLogic<()> for PathLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7510
    }
    fn private_op(&self) -> u16 {
        0x7511
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
    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        let player = PlayerId(conn.0);
        self.players.push(player);
        Admission { player, entity: 1 }
    }
    fn on_leave(&mut self, _w: &mut (), player: PlayerId) {
        self.players.retain(|p| *p != player);
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        for x in a.drain(..) {
            let _ = self.ops.try_send(x.op);
        }
    }
    fn update(&mut self, _w: &mut (), ctx: &TickCtx) {
        for &p in &self.players {
            let _ = self
                .obs
                .try_send((p, ctx.path(p), ctx.budget(p), ctx.since_input(p)));
        }
    }
    fn on_disconnect(&mut self, _w: &mut (), _p: PlayerId, _identity: &str) -> Detach {
        self.decision
    }
}

impl RoomLogic<()> for PathLogic {}

struct Rig {
    actor: RoomActor<(), (), ()>,
    obs: mpsc::Receiver<PathObs>,
    ops: mpsc::Receiver<u16>,
    t0: Instant,
    _outs: Vec<mpsc::Receiver<FrameBatch>>,
}

impl Rig {
    fn new(decision: Detach) -> Self {
        let (obs_tx, obs) = mpsc::channel(4096);
        let (ops_tx, ops) = mpsc::channel(4096);
        let (_tick_tx, tick_rx) = broadcast::channel(4);
        let (_control, control_rx) = channel(16);
        let actor = RoomActor::new(
            RoomConfig {
                id: RoomId(61),
                ..Default::default()
            },
            (),
            Box::new(PathLogic {
                players: Vec::new(),
                obs: obs_tx,
                ops: ops_tx,
                decision,
            }),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
            None,
        );
        Self {
            actor,
            obs,
            ops,
            t0: Instant::now(),
            _outs: Vec::new(),
        }
    }

    fn join(&mut self, conn: ConnectionId) -> Mailbox<Action> {
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
        self._outs.push(out_rx);
        let (rtx, mut rrx) = oneshot::channel();
        self.actor.handle_control(RoomControl::Resume {
            conn,
            epoch: 1,
            identity: format!("id{}", conn.0),
            out: out_tx,
            reply: rtx,
            claims: None,
        });
        rrx.try_recv().expect("reply").expect("join accepted").1
    }

    fn step_at(&mut self, tick: u64, secs: u64) {
        self.actor.step_phases(&TickInfo {
            tick,
            at: self.t0 + Duration::from_secs(secs),
        });
    }

    /// The last observation for `player` (`None`: not observed).
    fn last(&mut self, player: PlayerId) -> Option<PathObs> {
        let mut last = None;
        while let Ok(o) = self.obs.try_recv() {
            if o.0 == player {
                last = Some(o);
            }
        }
        last
    }

    fn ingested(&mut self) -> Vec<u16> {
        let mut v = Vec::new();
        while let Ok(op) = self.ops.try_recv() {
            v.push(op);
        }
        v
    }
}

fn paced(rate: u32) -> PathState {
    PathState {
        phase: PathPhase::Paced,
        rate: Some(rate),
        rtt: Some(Duration::from_millis(90)),
        ..Default::default()
    }
}

#[test]
fn a_path_marker_reaches_the_tick_context_and_not_the_game_or_the_idle_clock() {
    let mut r = Rig::new(Detach::Despawn);
    let actions = r.join(ConnectionId(1));
    let p = PlayerId(1);
    r.step_at(1, 1);
    let (_, path, budget, _) = r.last(p).expect("observed");
    assert_eq!((path, budget), (None, None), "nothing measured yet");
    actions
        .try_send(path_action(ConnectionId(1), &paced(30_000)))
        .expect("room for the marker");
    r.step_at(2, 9);
    let (_, path, budget, idle) = r.last(p).expect("observed");
    assert_eq!(path, Some(paced(30_000)));
    let period = RoomConfig::default().period();
    assert_eq!(budget, paced(30_000).budget(period));
    assert_eq!(budget, Some(999), "30 000 B/s over a 30 Hz tick");
    assert!(
        idle >= Some(Duration::from_secs(8)),
        "the marker is not input: the member stays input-idle, got {idle:?}"
    );
    // A news-less tick keeps the state; an open path has no budget.
    r.step_at(3, 10);
    assert_eq!(r.last(p).expect("observed").1, Some(paced(30_000)));
    actions
        .try_send(path_action(ConnectionId(1), &PathState::default()))
        .expect("room");
    r.step_at(4, 11);
    let (_, path, budget, _) = r.last(p).expect("observed");
    assert_eq!((path, budget), (Some(PathState::default()), None));
}

/// The game never sees a marker: its ingest gets the input around it,
/// in order, and nothing else.
#[test]
fn the_game_ingests_the_input_around_a_marker_and_never_the_marker() {
    let mut r = Rig::new(Detach::Despawn);
    let actions = r.join(ConnectionId(2));
    let input = |op| Action {
        conn: ConnectionId(2),
        player: PlayerId(0),
        op,
        payload: bytes::Bytes::new(),
    };
    let game = gsb_protocol::op::GAME_BAND_START;
    actions.try_send(input(game)).expect("room");
    actions
        .try_send(path_action(ConnectionId(2), &paced(8_000)))
        .expect("room");
    actions.try_send(input(game + 1)).expect("room");
    r.step_at(1, 1);
    assert_eq!(r.ingested(), vec![game, game + 1]);
    assert_eq!(r.actor.paths.get(PlayerId(2)), Some(paced(8_000)));
}

/// A parked member's transport is dead: its path is unknown (no budget)
/// from the detach on; the resumed session starts unknown too, until its
/// own connection sends news.
#[test]
fn a_parked_member_has_no_path_and_a_resume_starts_unknown() {
    let mut r = Rig::new(Detach::Hold {
        grace: Some(Duration::from_secs(60)),
        to: ExpireTo::Despawn,
    });
    let p = PlayerId(3);
    let actions = r.join(ConnectionId(3));
    actions
        .try_send(path_action(ConnectionId(3), &paced(12_000)))
        .expect("room");
    r.step_at(1, 1);
    assert_eq!(r.actor.paths.get(p), Some(paced(12_000)));
    r.actor.handle_control(RoomControl::Detach {
        conn: ConnectionId(3),
        entity: 1,
        identity: "id3".into(),
    });
    assert!(r.actor.conns[&p].detached, "parked");
    assert_eq!(r.actor.paths.get(p), None, "the dead transport's path goes");
    r.step_at(2, 2);
    let (_, path, budget, _) = r.last(p).expect("observed while parked");
    assert_eq!((path, budget), (None, None));
    // A marker that was still in flight on the old channel cannot bring
    // it back: a parked row is not pulled, and the resume swaps the
    // channel and drops what it held.
    actions
        .try_send(path_action(ConnectionId(3), &paced(50_000)))
        .expect("the parked row's channel is still open");
    let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
    r._outs.push(out_rx);
    let (rtx, mut rrx) = oneshot::channel();
    r.actor.handle_control(RoomControl::Resume {
        conn: ConnectionId(33),
        epoch: 2,
        identity: "id3".into(),
        out: out_tx,
        reply: rtx,
        claims: None,
    });
    let _new = rrx.try_recv().expect("reply").expect("resumed");
    r.step_at(3, 3);
    assert_eq!(r.last(p).expect("observed").1, None, "unknown until news");
}

/// A member that leaves takes its path along; a marker still unread in
/// its channel is not input it sent, and is not counted as such.
#[test]
fn a_leave_forgets_the_path_and_an_unread_marker_is_no_lost_input() {
    let mut r = Rig::new(Detach::Despawn);
    let p = PlayerId(4);
    let actions = r.join(ConnectionId(4));
    actions
        .try_send(path_action(ConnectionId(4), &paced(9_000)))
        .expect("room");
    r.step_at(1, 1);
    assert_eq!(r.actor.paths.get(p), Some(paced(9_000)));
    // News lands, then the leave runs before any READ sees it.
    actions
        .try_send(path_action(ConnectionId(4), &paced(4_000)))
        .expect("room");
    r.actor.handle_control(RoomControl::Leave {
        conn: ConnectionId(4),
        entity: 1,
    });
    assert_eq!(r.actor.paths.get(p), None);
    assert!(r.actor.paths.is_empty());
    assert_eq!(r.actor.m.actions_dropped_unread, 0, "a marker is not input");
    assert_eq!(r.actor.m.requests_dropped_unread, 0);
}
