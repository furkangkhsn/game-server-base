//! The shard's keep-alive: a silent group still gets its cached
//! snapshot re-sent on cadence.

use super::*;

/// A single-group logic that goes silent once its content is out:
/// `on_join` dirties the group, an emission cleans it — so after the
/// join tick EVERY step is unchanged, which is exactly the state the
/// keep-alive cadence exists to interrupt.
struct KaLogic {
    dirty: bool,
    next_wire: u64,
}

impl GameLogic<TWorld> for KaLogic {
    type GroupKey = ();
    type Strip = TStrip;

    fn snapshot_op(&self) -> u16 {
        0x7180
    }
    fn private_op(&self) -> u16 {
        0x7181
    }
    fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}

    fn snapshot(
        &mut self,
        w: &mut TWorld,
        _ctx: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[BorderRecord<TStrip>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        if !self.dirty {
            return false; // unchanged since the last emission
        }
        self.dirty = false;
        let mut recs: Vec<(u64, i32, i32)> = w
            .ents
            .iter()
            .map(|(wire, (x, y, _))| (*wire, *x as i32, *y as i32))
            .collect();
        recs.sort_unstable_by_key(|r| r.0);
        for (wire, x, y) in &recs {
            out.extend_from_slice(&wire.to_le_bytes());
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
        }
        true
    }

    fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
        self.next_wire += 1;
        let x = (conn.0 % 20) as f32 - 10.0;
        w.ents.insert(self.next_wire, (x, 0.0, 0));
        self.dirty = true; // membership changed ⇒ must emit
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

impl ShardLogic<TWorld> for KaLogic {
    type State = TState;

    fn index(&self) -> usize {
        0
    }
    fn shard_count(&self) -> usize {
        1
    }
    fn serial_base(&self) -> u64 {
        0
    }
    fn serial_range(&self) -> u64 {
        SHARD_SERIAL_RANGE
    }
    fn serial_used(&self) -> u64 {
        self.next_wire
    }
    fn neighbors(&self) -> &[usize] {
        &[]
    }
    fn collect_migrations(
        &mut self,
        _w: &mut TWorld,
        _nb: usize,
    ) -> Vec<Migrating<TState>> {
        Vec::new()
    }
    fn on_migrate_in(
        &mut self,
        _w: &mut TWorld,
        _wire: u64,
        _state: TState,
        _player: Option<PlayerId>,
    ) {
    }
    fn on_migrate_out(&mut self, _w: &mut TWorld, _wire: u64) {}
    fn collect_border(&self, _w: &TWorld) -> Vec<BorderRecord<TStrip>> {
        Vec::new()
    }
    fn own_wires(&self, w: &TWorld) -> Vec<u64> {
        w.ents.keys().copied().collect()
    }
}

/// The lock: `keepalive_hz = 10` under a 30 Hz shard → a re-send
/// every 3rd step. The group emits on the join step; steps 2..=8 are
/// silent EXCEPT steps 3 and 6, which ship the CACHED full snapshot
/// (byte-identical to the join emission) — and nothing else ever
/// reaches the wire. Mirrors the room-side semantics: same cadence
/// derivation, same cache, same resend counter.
#[tokio::test]
async fn sharded_keepalive_resends_cached_snapshot_to_silent_group() {
    let (_tick_tx, tick_rx) = broadcast::channel(64);
    let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
    let (n0, _n0_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let (n1, _n1_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let mut a = ShardActor::new(
        RoomConfig {
            id: RoomId(11),
            tick_hz: 30.0,
            keepalive_hz: 10.0,
            metrics_cadence_hz: 0.0,
            ..Default::default()
        },
        0,
        TWorld::default(),
        Box::new(KaLogic {
            dirty: false,
            next_wire: 0,
        }),
        tick_rx,
        rx,
        vec![n0, n1],
        1,
        metrics_null(),
        None, // no result sink
    );

    // The join (its own CONTROL message; the group is dirty).
    let (out_tx, mut out_rx) = mpsc::channel::<FrameBatch>(16);
    let (reply_tx, reply_rx) = oneshot::channel();
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(1),
            epoch: 1,
            out: out_tx,
            reply: reply_tx,
        },
        &tctx(1),
    ));
    let _wire = reply_rx.await.expect("join reply").expect("join ok");

    // Steps 2..=8 change nothing. Only the cadence steps (3 and 6)
    // may ship anything besides the join emission of step 1. Driven
    // through `step` (not `step_phases`) so the actor's own step
    // counter advances — the cadence is measured in ACTOR steps.
    assert!(a.step(&tinfo(1)), "step 1 runs");
    for t in 2..=8u64 {
        assert!(a.step(&tinfo(t)), "step {t} runs");
    }

    // Exactly three batches reached the member: the join emission plus
    // two keep-alive re-sends, all byte-identical (the CACHE, not a
    // fresh encode — the default hook re-sends `last`).
    let mut got = Vec::new();
    while let Ok(batch) = out_rx.try_recv() {
        got.push(batch);
    }
    assert_eq!(
        got.len(),
        3,
        "one emission (step 1) + two keep-alive re-sends (steps 3 and \
         6), nothing else: {got:?}"
    );
    let payloads: Vec<Vec<u8>> = got
        .iter()
        .map(|b| {
            assert_eq!(b.len(), 1, "one frame per batch");
            b[0].payload.to_vec()
        })
        .collect();
    for (i, p) in payloads.iter().enumerate() {
        assert_eq!(
            p, &payloads[0],
            "keep-alive re-send {i} must be the cached snapshot bytes"
        );
    }
    assert_eq!(
        payloads[0].len(),
        16,
        "the payload is one entity record (u64 wire LE + i32 x LE + \
         i32 y LE)"
    );
    // The mechanism counter agrees with the wire.
    assert_eq!(
        a.m.keepalive_resends, 2,
        "two unchanged-group re-sends (steps 3 and 6)"
    );
}

// -----------------------------------------------------------------
// Delta border exchange (CROSS-SHARD §6.4, the four-pin contract).
// The rig below wires two BARE (unspawned) actors with real bounded
// channels whose RECEIVING ends the test holds: every inter-shard
// message crosses the test's hands, so a loss can be simulated
// exactly (drop one message) and the resync traffic observed without
// any network or timing dependence.
// -----------------------------------------------------------------

// -----------------------------------------------------------------
// ShardLink seam structural locks (`docs/DISTRIBUTED.md` §3): the
// in-process link must be a TRANSPARENT wrapper — FIFO order
// preserved on drain, and a refused send hands the EXACT message back
// with no partial state (the migration rollback reads its payload out
// of the error alone).
// -----------------------------------------------------------------
