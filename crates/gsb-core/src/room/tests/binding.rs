//! The resume binding and the roster it must not disturb.

use super::*;

struct RebindLogic {
    held: std::collections::HashMap<String, PlayerId>,
    ents: std::collections::HashMap<PlayerId, EntityId>,
    next: u64,
}

// Faz 1 trait split: the shared contract lives on `GameLogic`; this
// logic uses no room-exclusive hook, so its `RoomLogic` impl is empty
// (both exclusive methods have defaults).
impl GameLogic<()> for RebindLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7400
    }
    fn private_op(&self) -> u16 {
        0x7401
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
    fn on_join(&mut self, _w: &mut (), _conn: ConnectionId) -> Admission {
        self.next += 1;
        let player = PlayerId(self.next);
        self.ents.insert(player, self.next);
        Admission {
            player,
            entity: self.next,
        }
    }
    fn on_leave(&mut self, _w: &mut (), player: PlayerId) {
        self.ents.remove(&player);
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
    fn on_disconnect(&mut self, _w: &mut (), player: PlayerId, identity: &str) -> Detach {
        if self.ents.contains_key(&player) {
            self.held.insert(identity.to_string(), player);
        }
        Detach::Hold {
            grace: None,
            to: ExpireTo::Despawn,
        }
    }
    fn resume_lookup(&self, _w: &(), identity: &str) -> ResumeFound {
        match self.held.get(identity) {
            Some(p) => ResumeFound::Held(*p),
            None => ResumeFound::Never,
        }
    }
    fn on_resume(
        &mut self,
        _w: &mut (),
        identity: &str,
        _conn: ConnectionId,
        _player: PlayerId,
        _entity: EntityId,
    ) {
        // The player-keyed tables keep their keys across the resume:
        // only the ledger entry is consumed.
        self.held.remove(identity);
    }
}

impl RoomLogic<()> for RebindLogic {}

#[test]
fn resume_rekeys_only_the_binding() {
    let cfg = RoomConfig {
        id: RoomId(31),
        ..Default::default()
    };
    let (_tick_tx, tick_rx) = broadcast::channel(4);
    let (_control, control_rx) = channel(16);
    let mut actor = RoomActor::new(
        cfg,
        (),
        Box::new(RebindLogic {
            held: std::collections::HashMap::new(),
            ents: std::collections::HashMap::new(),
            next: 0,
        }),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
        None,
    );
    // Three members join; remember each session's binding row.
    let mut ents = std::collections::HashMap::new();
    let mut pid_of = std::collections::HashMap::new();
    for c in 1..=3u64 {
        let (out_tx, _o) = mpsc::channel::<FrameBatch>(2);
        let (rtx, mut rrx) = oneshot::channel();
        actor.handle_control(RoomControl::Join {
            conn: ConnectionId(c),
            out: out_tx,
            reply: rtx,
        });
        let (e, _a) = rrx.try_recv().ok().unwrap().unwrap();
        ents.insert(ConnectionId(c), e);
        pid_of.insert(ConnectionId(c), actor.binding[&ConnectionId(c)]);
    }
    // Snapshot the resume-insensitive surface BEFORE the park.
    let roster_before = actor.roster.clone();
    let pos_before = actor.roster_pos.clone();
    let cursor_before = actor.read_cursor;
    // c2's transport dies; policy holds.
    actor.handle_control(RoomControl::Detach {
        conn: ConnectionId(2),
        entity: ents[&ConnectionId(2)],
        identity: "ana".into(),
    });
    assert!(actor.conns[&pid_of[&ConnectionId(2)]].detached, "parked");
    // The resume binds a fresh socket (c9) onto the parked row.
    let (out_tx, _o) = mpsc::channel::<FrameBatch>(2);
    let (rtx, mut rrx) = oneshot::channel();
    actor.handle_control(RoomControl::Resume {
        conn: ConnectionId(9),
        epoch: 1,
        identity: "ana".into(),
        out: out_tx,
        reply: rtx,
    });
    let reply = rrx.try_recv().expect("reply sent synchronously");
    let (entity, _actions) = reply.expect("resume accepted");
    assert_eq!(
        entity,
        ents[&ConnectionId(2)],
        "the SAME wire id comes back"
    );

    let pid = pid_of[&ConnectionId(2)];
    // The binding moved — and it is the ONLY table that did.
    assert!(
        !actor.binding.contains_key(&ConnectionId(2)),
        "old row gone"
    );
    assert_eq!(actor.binding[&ConnectionId(9)], pid, "new row, SAME player");
    assert_eq!(actor.conns[&pid].conn, ConnectionId(9), "row re-pointed");
    assert!(!actor.conns[&pid].detached, "rebound row live");
    assert_eq!(
        actor.conns[&pid].entity,
        ents[&ConnectionId(2)],
        "entity/wire id intact"
    );
    // conns kept its key...
    assert!(actor.conns.contains_key(&pid));
    // ...roster untouched (same ids, same order, same positions)...
    assert_eq!(actor.roster, roster_before, "roster not touched by resume");
    assert_eq!(actor.roster_pos, pos_before, "position map ditto");
    assert_eq!(actor.read_cursor, cursor_before, "rotation cursor ditto");
    assert_eq!(actor.roster.len(), 3, "no membership change happened");
    // The rebound tables still drain cleanly (mass-leave invariant):
    // leaves arrive under BOTH session ids over the lifetime — each
    // resolves through the CURRENT binding (c2's leaf is stale and
    // must be a no-op now that c9 owns the park).
    for c in [1u64, 3] {
        actor.handle_control(RoomControl::Leave {
            conn: ConnectionId(c),
            entity: ents[&ConnectionId(c)],
        });
    }
    actor.handle_control(RoomControl::Leave {
        conn: ConnectionId(9),
        entity: ents[&ConnectionId(2)],
    });
    assert!(
        actor.roster.is_empty() && actor.roster_pos.is_empty() && actor.conns.is_empty(),
        "drained"
    );
    assert!(actor.binding.is_empty(), "bindings torn down with the rows");
}

/// Regression lock for the roster-drift panic: `swap_remove` returns
/// the REMOVED element, and the fix must retarget the RELOCATED one.
/// The old code fixed the removed element's (just-deleted) entry, so
/// the first non-tail leave left the relocated connection's position
/// stale and a mass-leave run panicked inside `roster_remove` —
/// observed on every loadgen end-of-run (surfaced by the supervision
/// round's death-reaping, which turned the silent task death into a
/// visible warn + reaped room).
#[test]
fn roster_stays_synchronized_through_mass_leaves() {
    let cfg = RoomConfig {
        id: RoomId(1),
        tick_hz: 30.0,
        metrics_cadence_hz: 30.0,
        ..Default::default()
    };
    let (tick_tx, _first) = broadcast::channel(64);
    let (_control, control_rx) = channel(1024);
    let (dts_tx, _d) = mpsc::channel(1);
    let (ops_tx, _o) = mpsc::channel(1);
    let mut actor = RoomActor::new(
        cfg,
        (),
        Box::new(RecLogic {
            dts: dts_tx,
            ops: ops_tx,
        }),
        tick_tx.subscribe(),
        control_rx,
        1,
        null_metrics_tx(),
        None,
    );
    const N: u64 = 50;
    for c in 1..=N {
        let (out_tx, _o) = mpsc::channel(8);
        let (rtx, _rrx) = oneshot::channel();
        actor.handle_control(RoomControl::Join {
            conn: ConnectionId(c),
            out: out_tx,
            reply: rtx,
        });
    }
    assert_eq!(actor.roster.len(), N as usize);
    // Every connection leaves, in join order — the worst pattern for
    // the old code (every removal relocates someone whose position
    // entry then had to be fixed).
    for c in 1..=N {
        actor.handle_control(RoomControl::Leave {
            conn: ConnectionId(c),
            entity: 1,
        });
    }
    assert!(actor.roster.is_empty(), "roster drained");
    assert!(actor.roster_pos.is_empty(), "positions drained");
    assert!(actor.conns.is_empty(), "table drained");
    // And the room still accepts joins afterwards (the structures
    // are consistent, not merely empty).
    let (out_tx, _o) = mpsc::channel(8);
    let (rtx, rrx) = oneshot::channel();
    actor.handle_control(RoomControl::Join {
        conn: ConnectionId(N + 1),
        out: out_tx,
        reply: rtx,
    });
    assert!(
        tokio_sync_oneshot_peek(rrx).is_some(),
        "post-mass-leave join accepted"
    );
}
