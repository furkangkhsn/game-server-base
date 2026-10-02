//! The shard's half of F32: a broadcast resume that finds its identity
//! still LIVE on another connection of this shard takes that session
//! over (the old session's detach first, then the park), as the single
//! room does — the other shards answer "not here" as ever.

use super::*;
use crate::error::CoreError;
use crate::room::{Detach, ExpireTo, ResumeFound};

/// Parks every disconnect in a ledger keyed by identity, reporting each
/// policy call.
struct LedgerLogic {
    next_wire: u64,
    ledger: HashMap<String, PlayerId>,
    asked: mpsc::UnboundedSender<PlayerId>,
}

impl GameLogic<TWorld> for LedgerLogic {
    type GroupKey = ();
    type Strip = TStrip;

    fn snapshot_op(&self) -> u16 {
        0x7610
    }
    fn private_op(&self) -> u16 {
        0x7611
    }
    fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut TWorld,
        _c: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[BorderRecord<TStrip>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
        self.next_wire += 1;
        w.ents.insert(self.next_wire, (0.0, 0.0, 0));
        Admission {
            player: PlayerId(conn.0),
            entity: self.next_wire,
        }
    }
    fn on_leave(&mut self, _w: &mut TWorld, _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut TWorld, _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut TWorld, _c: &TickCtx) {}
    fn on_disconnect(&mut self, _w: &mut TWorld, player: PlayerId, identity: &str) -> Detach {
        let _ = self.asked.send(player);
        self.ledger.insert(identity.to_string(), player);
        Detach::Hold {
            grace: None,
            to: ExpireTo::Despawn,
        }
    }
    fn resume_lookup(&self, _w: &TWorld, identity: &str) -> ResumeFound {
        match self.ledger.get(identity) {
            Some(&player) => ResumeFound::Held(player),
            None => ResumeFound::Never,
        }
    }
    fn on_resume(
        &mut self,
        _w: &mut TWorld,
        identity: &str,
        _c: ConnectionId,
        _p: PlayerId,
        _e: EntityId,
    ) {
        self.ledger.remove(identity);
    }
}

impl ShardLogic<TWorld> for LedgerLogic {
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

/// The shard, and the players its policy was asked about, in order.
fn shard() -> (Shard, mpsc::UnboundedReceiver<PlayerId>) {
    let (_tick_tx, tick_rx) = broadcast::channel(16);
    let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
    let (asked, policy_calls) = mpsc::unbounded_channel();
    let a = ShardActor::new(
        RoomConfig {
            id: RoomId(62),
            metrics_cadence_hz: 0.0,
            ..Default::default()
        },
        0,
        TWorld::default(),
        Box::new(LedgerLogic {
            next_wire: 0,
            ledger: HashMap::new(),
            asked,
        }),
        tick_rx,
        rx,
        vec![],
        1,
        metrics_null(),
        None,
    );
    (a, policy_calls)
}

type Answer = Result<Option<(EntityId, Mailbox<Action>)>, CoreError>;

/// A broadcast resume for "ana" by `conn`: this shard's answer.
fn resume(a: &mut Shard, conn: u64, epoch: u64) -> Answer {
    let (out, _o) = mpsc::channel::<FrameBatch>(8);
    let (reply, mut answer) = oneshot::channel();
    assert!(a.handle_msg(
        ShardMsg::Resume {
            conn: ConnectionId(conn),
            epoch,
            identity: "ana".to_string(),
            out,
            reply,
        },
        1,
    ));
    answer.try_recv().expect("answered in the same call")
}

#[test]
fn a_resume_ahead_of_the_old_session_s_detach_takes_that_session_over() {
    let (mut a, mut asked) = shard();
    let (out, _o) = mpsc::channel::<FrameBatch>(8);
    let (reply, mut seat) = oneshot::channel();
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(1),
            epoch: 1,
            identity: "ana".to_string(),
            out,
            reply,
            claims: None,
        },
        1,
    ));
    let (entity, _actions) = seat.try_recv().expect("answered").expect("joined");
    let player = a.binding[&ConnectionId(1)];

    // c2's resume lands before c1's detach.
    let (got, _actions) = resume(&mut a, 2, 2)
        .expect("not stale")
        .expect("this shard holds the identity");
    assert_eq!(got, entity, "the SAME entity comes back");
    assert_eq!(a.m.resumes, 1);
    assert_eq!(a.conns.len(), 1, "one member for one identity");
    assert_eq!(a.binding.get(&ConnectionId(2)), Some(&player));
    assert_eq!(asked.try_recv(), Ok(player), "the old session's policy ran");
    assert!(asked.try_recv().is_err(), "once");

    // c1's detach, late: a no-op.
    assert!(a.handle_msg(
        ShardMsg::Detach {
            conn: ConnectionId(1),
            entity,
            identity: "ana".to_string(),
        },
        1,
    ));
    assert!(!a.conns[&player].detached, "the new session stays live");
    assert!(asked.try_recv().is_err(), "the policy is not asked again");

    // The ordinary path is untouched: a drop parks, a resume takes the
    // park without asking the policy a second time.
    assert!(a.handle_msg(
        ShardMsg::Detach {
            conn: ConnectionId(2),
            entity,
            identity: "ana".to_string(),
        },
        1,
    ));
    assert_eq!(asked.try_recv(), Ok(player));
    let (again, _actions) = resume(&mut a, 3, 3).expect("not stale").expect("here");
    assert_eq!(again, entity);
    assert!(asked.try_recv().is_err(), "a parked row is not taken over");

    // Another identity's resume still finds nothing here.
    let (out, _o) = mpsc::channel::<FrameBatch>(8);
    let (reply, mut answer) = oneshot::channel();
    assert!(a.handle_msg(
        ShardMsg::Resume {
            conn: ConnectionId(4),
            epoch: 4,
            identity: "bob".to_string(),
            out,
            reply,
        },
        1,
    ));
    assert!(matches!(answer.try_recv(), Ok(Ok(None))), "not here");
}
