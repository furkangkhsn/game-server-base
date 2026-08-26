//! The one-owner invariant across a churn run, wire-identity
//! disjointness, and what must survive a crossing: in-flight input,
//! the player's identity, and the border view both sides share.

use super::*;

/// Required test 1 — migration never drops or duplicates an entity,
/// including a ping-pong across the boundary. The entity walks right
/// (shard 0 → 1), then left (1 → 0), then right again: at EVERY tick
/// index it is in exactly one shard, and its position advances by
/// exactly the tick's mode (no teleports, no gaps — a one-tick loss
/// would show up as a double step).
#[tokio::test]
async fn migration_never_drops_or_duplicates() {
    let mut h = Harness::new();
    let conn = ConnectionId(1);
    let (wire, actions, _out) = h.join(0, conn, 1).await;
    // join() consumed tick 1: the entity is in shard 0 at x = -9.
    assert_eq!(h.content[0].len(), 1);
    assert_eq!(h.content[0][0].0, wire);
    let mut prev_x = h.content[0][0].1;

    // Walk right from x = -9; the crossing into shard 1 happens when
    // the post-step position reaches x = 0.
    h.act(&actions, conn, 1000).await;
    // Flip direction once in shard 1 (it crosses back), then again.
    let mut flipped = false;
    for _ in 0..40 {
        let c = h.tick().await;
        let t = h.t;
        // Exactly one shard owns the entity at this tick index.
        let owners = owners_at(&c, &h.migrated, t, wire);
        assert_eq!(
            owners.len(),
            1,
            "tick {t}: wire {wire} owned by {owners:?} (must be exactly one)"
        );
        let Some((x, mode)) = c[owners[0]].iter().find_map(|e| {
            if e.0 == wire {
                Some((e.1, e.3))
            } else {
                None
            }
        }) else {
            panic!("tick {t}: owner {} lost the entity", owners[0]);
        };
        // Position continuity: the step applied this tick equals the
        // mode in force for the tick (ingest runs before the step in
        // the same tick, so the tick's own content mode is the one
        // applied).
        assert!(
            (x - prev_x - mode as f32).abs() < 1e-6,
            "tick {t}: position jumped {prev_x} -> {x} (mode {mode})"
        );
        prev_x = x;
        // Flip direction once the entity is in shard 1 (it will cross
        // back), then again once it is back in shard 0.
        if owners[0] == 1 && !flipped {
            h.act(&actions, conn, 1001).await;
            flipped = true;
        } else if owners[0] == 0 && flipped {
            h.act(&actions, conn, 1000).await;
        }
    }
    // The ping-pong actually happened (the entity crossed into shard 1
    // and back).
    assert!(flipped, "the entity never crossed into shard 1");
    assert!(
        h.migrated
            .iter()
            .any(|((_, s), v)| *s == 1 && v.contains(&wire)),
        "the entity never crossed back into shard 0"
    );
}

/// Required test 2 — wire identity: a migrated entity keeps its id;
/// the two shards' id spaces are disjoint (no cross-shard collision).
#[tokio::test]
async fn wire_identity_stable_and_disjoint() {
    let mut h = Harness::new();
    let (w0, actions0, _o0) = h.join(0, ConnectionId(1), 1).await; // x = -9, shard 0
    let (w1, _a1, _o1) = h.join(1, ConnectionId(10), 1).await; // x = 0, shard 1
    // Disjoint ranges: shard 0 below 2^20, shard 1 at/above it.
    assert!(w0 < SHARD_SERIAL_RANGE, "shard 0 minted out of range: {w0}");
    assert!(
        (SHARD_SERIAL_RANGE..2 * SHARD_SERIAL_RANGE).contains(&w1),
        "shard 1 minted out of range: {w1}"
    );
    assert_ne!(w0, w1);
    // Walk w0 into shard 1; it must arrive under the SAME id.
    h.act(&actions0, ConnectionId(1), 1000).await;
    let mut crossed_at = None;
    for _ in 0..30 {
        let c = h.tick().await;
        if c[1].iter().any(|(w, _, _, _)| *w == w0)
            && !reported_out(&h.migrated, h.t, 1, w0)
        {
            crossed_at = Some(h.t);
            break;
        }
    }
    let Some(t) = crossed_at else {
        panic!("w0 never crossed into shard 1");
    };
    // The crossing was reported by shard 0 at t-1 (it despawns at t).
    assert!(
        reported_out(&h.migrated, t, 0, w0),
        "shard 0 did not report the crossing of w0"
    );
    // Both entities coexist in shard 1 under their own ids (no
    // collision: w0 and w1 are distinct records in the same world).
    let c = h.content;
    let ids: Vec<u64> = c[1].iter().map(|(w, _, _, _)| *w).collect();
    assert!(ids.contains(&w0) && ids.contains(&w1), "ids: {ids:?}");
    assert_eq!(ids.len(), 2);
}

/// Required test 3 — in-flight input survives the migration: the
/// action is in the channel while the connection is in transit; it is
/// applied by the RECEIVING shard (the channel moved with the
/// connection).
#[tokio::test]
async fn in_flight_action_survives_migration() {
    let mut h = Harness::new();
    let conn = ConnectionId(1);
    let (wire, actions, _out) = h.join(0, conn, 1).await;
    // Walk right; cross into shard 1.
    h.act(&actions, conn, 1000).await;
    let mut crossed_at = None;
    for _ in 0..30 {
        h.tick().await;
        if reported_out(&h.migrated, h.t, 0, wire) {
            crossed_at = Some(h.t);
            break;
        }
    }
    let Some(t_cross) = crossed_at else {
        panic!("no crossing");
    };
    // IN FLIGHT now: shard 0 just sent the migration (tick t_cross);
    // the connection halves are in transit (or just installed in
    // shard 1). Queue a mode change — it lands in the same channel
    // object the Migrate message carried to shard 1, wherever that
    // object currently sits.
    h.act(&actions, conn, 1001).await;
    // The next tick: shard 1 pulls it in its READ phase and applies it
    // (mode -1) in its step. If the action had been lost, the mode
    // would still be +1.
    let c = h.tick().await;
    let t = h.t;
    let owners = owners_at(&c, &h.migrated, t, wire);
    assert_eq!(owners.len(), 1, "tick {t}: owners {owners:?}");
    let (x, mode) = c[owners[0]]
        .iter()
        .find(|(w, _, _, _)| *w == wire)
        .map(|e| (e.1, e.3))
        .expect("entity present");
    assert_eq!(
        mode, -1,
        "the in-flight action was not applied by the receiving shard \
         (tick {t}, crossing at {t_cross}): mode {mode}"
    );
    // The entity stepped LEFT (toward shard 0) on this tick — the
    // in-flight action's mode, not the pre-migration one (+1). The
    // action was queued after the receiving shard's READ for the
    // spawn tick, so it is applied exactly one tick later: from x = 1
    // (the spawn tick's own +1 step) to x = 0.
    assert!((x - 0.0).abs() < 1e-6, "position {x} (expected 0)");
    // The action was ingested exactly once (shard 0's READ for the
    // crossing tick already ran before the send; only shard 1 can
    // pull it now).
    let ops = h.ops_drained().await;
    let n = ops.iter().filter(|(p, op)| *p == PlayerId(conn.0) && *op == 1001).count();
    assert_eq!(n, 1, "ops: {ops:?}");
}

/// Faz 2 lock — player identity is stable ACROSS SHARD MIGRATION: the
/// same human keeps ONE [`PlayerId`] from before the crossing to
/// after it, and the receiving shard ingests its input under that id
/// (the identity rides the `PlayerMigration`, exactly like the wire
/// id rides the entity state). Combined with the room-side resume
/// locks this pins the contract "resume/migration move the SESSION,
/// never the player".
#[tokio::test]
async fn player_identity_is_stable_across_migration() {
    let mut h = Harness::new();
    let conn = ConnectionId(1);
    let pid = PlayerId(conn.0); // the test logic's minting policy
    let (wire, actions, _out) = h.join(0, conn, 1).await;
    // One action BEFORE the migration: ingested by shard 0 under pid.
    h.act(&actions, conn, 1001).await;
    h.tick().await;
    // Walk right; cross into shard 1.
    h.act(&actions, conn, 1000).await;
    let mut crossed_at = None;
    for _ in 0..30 {
        h.tick().await;
        if reported_out(&h.migrated, h.t, 0, wire) {
            crossed_at = Some(h.t);
            break;
        }
    }
    assert!(crossed_at.is_some(), "no crossing");
    // Let the receiving shard install the row and pull one more input.
    h.tick().await;
    h.act(&actions, conn, 1002).await;
    h.tick().await;

    // Every observed op — on EITHER side of the seam — belongs to the
    // SAME stable player.
    let ops = h.ops_drained().await;
    assert!(
        ops.contains(&(pid, 1001)) && ops.contains(&(pid, 1002)),
        "input observed before AND after the migration: {ops:?}"
    );
    assert!(
        ops.iter().all(|(p, _)| *p == pid),
        "every action carries the SAME player id across migration: {ops:?}"
    );
}

/// Required test 4 — the leave/migration race: a `Migrate` whose join
/// is already dead (the leave was processed first) is rejected by the
/// epoch gate; a fresh join of the same connection (newer epoch) is
/// still accepted.
#[tokio::test]
async fn ghost_migrate_after_leave_is_rejected() {
    let mut h = Harness::new();
    let conn = ConnectionId(3);
    let (wire, _actions, _out) = h.join(0, conn, 1).await; // x = -7, shard 0, epoch 1
    let _ = h.tick().await; // steady
    // The leave (the registry's broadcast; epoch 1 = this join).
    h.leave(conn, wire, 1).await;
    let c = h.tick().await;
    // The entity is gone from both shards.
    assert!(
        c.iter().all(|v| v.iter().all(|(w, _, _, _)| *w != wire)),
        "the leave did not despawn the entity: {c:?}"
    );
    // The GHOST: a Migrate of the dead join (epoch 1) reaches shard 1
    // (simulating an in-flight migration that lost the race).
    let (ghost_out, _ghost_out_rx) = mpsc::channel::<FrameBatch>(8);
    let (_ghost_act_tx, ghost_act_rx) = mpsc::channel::<Action>(8);
    h.shard_txs[1]
        .send(ShardMsg::Migrate {
            from: 0,
            at_tick: h.t,
            wire,
            state: TState {
                x: -7.0,
                y: 0.0,
                mode: 0,
            },
            player: Some(PlayerMigration {
                player: PlayerId(conn.0),
                conn,
                epoch: 1,
                entity: wire,
                out: ghost_out,
                actions: ghost_act_rx,
                detached: false,
                detach_deadline: None,
                expire_to: crate::room::ExpireTo::Despawn,
                bot_fed: false,
                session_epoch: 0,
            }),
        })
        .await
        .expect("shard channel open");
    let c = h.tick().await;
    assert!(
        c.iter().all(|v| v.iter().all(|(w, _, _, _)| *w != wire)),
        "the ghost migrate resurrected the entity (the epoch gate \
         failed): {c:?}"
    );
    // A FRESH join of the same connection (epoch 2) must still be
    // accepted (the gate rejects only the dead join's epoch).
    let (wire2, _a2, _o2) = h.join(1, conn, 2).await;
    let c = h.content;
    assert!(
        c[1].iter().any(|(w, _, _, _)| *w == wire2),
        "the fresh join (epoch 2) was not accepted: {c:?}"
    );
    assert_ne!(wire, wire2, "fresh joins mint fresh identities");
}

/// Required test 5 — boundary visibility: each shard's snapshot
/// includes the neighbor's boundary entities (the borrowed set), so
/// a player at the boundary sees across the line.
#[tokio::test]
async fn boundary_entities_are_visible_to_both_sides() {
    let mut h = Harness::new();
    // conn 1 at x = -9 (shard 0), conn 11 at x = +1 (shard 1 — already
    // on the border: |x| <= 1).
    let (w0, actions0, mut out0) = h.join(0, ConnectionId(1), 1).await;
    let (w1, _a1, mut out1) = h.join(1, ConnectionId(11), 1).await;
    // Walk conn 1 toward the boundary (it reaches x = -1, exported by
    // shard 0, in a few ticks).
    h.act(&actions0, ConnectionId(1), 1000).await;

    /// Parse one snapshot frame into (wire, x, y) records.
    fn parse_records(payload: &bytes::Bytes) -> Vec<(u64, i32, i32)> {
        assert_eq!(payload.len() % 16, 0, "record-aligned payload");
        (0..payload.len() / 16)
            .map(|i| {
                let b = &payload[i * 16..i * 16 + 16];
                let wire = u64::from_le_bytes(b[0..8].try_into().unwrap());
                let x = i32::from_le_bytes(b[8..12].try_into().unwrap());
                let y = i32::from_le_bytes(b[12..16].try_into().unwrap());
                (wire, x, y)
            })
            .collect()
    }
    async fn read_snapshot(
        rx: &mut mpsc::Receiver<FrameBatch>,
    ) -> Option<Vec<(u64, i32, i32)>> {
        let batch = rx.recv().await.expect("snapshot stream alive");
        batch
            .into_iter()
            .find(|f| f.op == 0x7100)
            .map(|f| parse_records(&f.payload))
    }
    // Until each shard's snapshot shows BOTH w0 (shard 0's entity) and
    // w1 (shard 1's entity) — the own record under its own id and the
    // borrowed record under the neighbor's id (disjoint ranges: no
    // collision in the union view).
    let mut seen0: Option<Vec<(u64, i32, i32)>> = None;
    let mut seen1: Option<Vec<(u64, i32, i32)>> = None;
    for _ in 0..60 {
        if seen0.is_none()
            && let Some(r) = read_snapshot(&mut out0).await
            && r.iter().any(|(w, _, _)| *w == w0)
            && r.iter().any(|(w, _, _)| *w == w1)
        {
            seen0 = Some(r);
        }
        if seen1.is_none()
            && let Some(r) = read_snapshot(&mut out1).await
            && r.iter().any(|(w, _, _)| *w == w1)
            && r.iter().any(|(w, _, _)| *w == w0)
        {
            seen1 = Some(r);
        }
        if seen0.is_some() && seen1.is_some() {
            break;
        }
        let _ = h.tick().await;
    }
    let Some(r0) = seen0 else {
        panic!("shard 0's snapshot never included the borrowed entity w1");
    };
    let Some(r1) = seen1 else {
        panic!("shard 1's snapshot never included the borrowed entity w0");
    };
    // Each view has exactly the two records, under distinct wires.
    assert_eq!(r0.len(), 2, "shard 0 view: {r0:?}");
    assert_eq!(r1.len(), 2, "shard 1 view: {r1:?}");
    let wires0: Vec<u64> = r0.iter().map(|r| r.0).collect();
    let wires1: Vec<u64> = r1.iter().map(|r| r.0).collect();
    assert!(wires0.contains(&w0) && wires0.contains(&w1));
    assert!(wires1.contains(&w0) && wires1.contains(&w1));
}

// -----------------------------------------------------------------
// Faz 1 keep-alive promotion (behavior lock): a SILENT group on a
// SHARDED room receives its cached full snapshot on the keep-alive
// cadence — the shard-side mirror of the room actor's
// `unchanged_group_is_silent_until_keepalive`. Drives a bare
// (unspawned) [`ShardActor`] synchronously, like the table-prune
// locks above: the assertions read only the connection's wire bytes
// and the actor's own counters.
// -----------------------------------------------------------------
