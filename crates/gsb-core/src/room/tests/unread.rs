//! The session ends that take an action channel along (BACKLOG B36):
//! each one counts the RPC requests still unread in it, once, and
//! nothing else. The leave itself — the case the loadgen measured — is
//! locked end to end in `tests/rpc/unread.rs`; these are the other ways
//! a channel dies, driven synchronously through `handle_control` (no
//! clock, no runtime).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

// READ's binding translation drops, counted by kind (B54).
mod unbound;
// What the room still holds when it stops, counted (B62).
mod stop;

/// A logic that parks (or despawns) every disconnect and resumes the
/// parked player of any identity.
struct ParkLogic {
    decision: Detach,
    /// The parked player (`0` = none), for `resume_lookup`.
    parked: Arc<AtomicU64>,
}

impl GameLogic<()> for ParkLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7a00
    }
    fn private_op(&self) -> u16 {
        0x7a01
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _borrowed: &[crate::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
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
    fn on_disconnect(&mut self, _w: &mut (), p: PlayerId, _identity: &str) -> Detach {
        self.parked.store(p.0, Ordering::Relaxed);
        self.decision
    }
    fn resume_lookup(&self, _w: &(), _identity: &str) -> ResumeFound {
        match self.parked.load(Ordering::Relaxed) {
            0 => ResumeFound::Never,
            p => ResumeFound::Held(PlayerId(p)),
        }
    }
}

impl RoomLogic<()> for ParkLogic {}

fn room(decision: Detach) -> RoomActor<(), (), ()> {
    let (_tick_tx, tick_rx) = broadcast::channel(4);
    let (_control, control_rx) = channel(16);
    let logic = ParkLogic {
        decision,
        parked: Arc::default(),
    };
    RoomActor::new(
        RoomConfig {
            id: RoomId(36),
            ..Default::default()
        },
        (),
        Box::new(logic),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
        None,
    )
}

/// Join `conn` (identity `who`, empty = anonymous) and return its
/// action mailbox.
fn join(r: &mut RoomActor<(), (), ()>, conn: u64, who: &str) -> Mailbox<Action> {
    let (out, _out_rx) = mpsc::channel::<FrameBatch>(8);
    let (reply, reply_rx) = oneshot::channel();
    r.handle_control(RoomControl::Resume {
        conn: ConnectionId(conn),
        epoch: 0,
        identity: who.to_string(),
        out,
        reply,
    });
    tokio_sync_oneshot_peek(reply_rx)
        .expect("sync reply")
        .expect("joined")
        .1
}

/// Two requests and one plain action into `conn`'s channel.
fn send_two_requests(tx: &Mailbox<Action>, conn: u64) {
    for op in [crate::rpc::RPC_REQ_OP, 0x2001, crate::rpc::RPC_REQ_OP] {
        tx.try_send(Action {
            conn: ConnectionId(conn),
            player: PlayerId(0),
            op,
            payload: bytes::Bytes::new(),
        })
        .expect("room in the channel");
    }
}

fn detach(r: &mut RoomActor<(), (), ()>, conn: u64) {
    r.handle_control(RoomControl::Detach {
        conn: ConnectionId(conn),
        entity: conn,
        identity: "p".to_string(),
    });
}

/// A park keeps its dead channel unread (READ skips a parked row); the
/// resume that swaps in a fresh one counts what the old one held.
#[test]
fn a_resume_counts_what_the_parked_session_left_unread() {
    let mut r = room(Detach::Hold {
        grace: None,
        to: ExpireTo::Despawn,
    });
    let tx = join(&mut r, 1, "p");
    send_two_requests(&tx, 1);
    drop(tx); // the transport died with its connection actor
    detach(&mut r, 1);
    assert_eq!(r.m.requests_dropped_unread, 0, "parked, not yet ended");

    let _fresh = join(&mut r, 2, "p");
    assert_eq!(r.m.resumes, 1, "the resume took the park over");
    assert_eq!(
        r.m.requests_dropped_unread, 2,
        "the two requests the dead channel held, not the plain action"
    );
    assert_eq!(
        r.m.actions_dropped_unread, 1,
        "the plain action, apart (B54)"
    );
}

/// A disconnect the policy despawns ends the row, and its channel's
/// unread requests with it.
#[test]
fn a_despawning_disconnect_counts_the_unread_requests() {
    let mut r = room(Detach::Despawn);
    let tx = join(&mut r, 1, "");
    send_two_requests(&tx, 1);
    drop(tx);
    detach(&mut r, 1);
    assert!(r.conns.is_empty(), "despawned");
    assert_eq!(r.m.requests_dropped_unread, 2);
    assert_eq!(
        r.m.actions_dropped_unread, 1,
        "the plain action, apart (B54)"
    );
    assert_eq!(
        r.sample().actions_dropped_unread,
        1,
        "the sample carries it"
    );
}

/// A rejoin on the same connection supersedes its stale row: the old
/// session's unread requests are counted once, and the fresh channel
/// starts empty.
#[test]
fn a_superseding_rejoin_counts_the_old_sessions_unread_requests() {
    let mut r = room(Detach::Despawn);
    let old = join(&mut r, 1, "");
    send_two_requests(&old, 1);
    let _new = join(&mut r, 1, "");
    assert_eq!(r.conns.len(), 1, "one row for the connection");
    assert_eq!(r.m.requests_dropped_unread, 2);
    assert_eq!(
        r.m.actions_dropped_unread, 1,
        "the plain action, apart (B54)"
    );
    assert!(old.is_closed(), "the old channel is gone");
}
