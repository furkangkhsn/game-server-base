//! Migration and the tables it touches: the epoch prune, the
//! tombstone TTL, the one-owner invariant across a churn run, and
//! the border visibility both sides of a seam must agree on.

use super::*;

mod ownership;

/// Table-prune lock 1 — a Leave removes the connection's `conn_epoch`
/// entry in BOTH arms: the entity-matched despawn AND the broadcast
/// leave this shard held no matching entity for. The re-join path is
/// asserted too (the prune is only safe because re-joins and
/// migrate-ins re-insert).
#[tokio::test]
async fn leave_prunes_the_epoch_entry() {
    let mut a = bare_shard(0);

    // Arm 1: the entity-matched despawn.
    let conn = ConnectionId(4);
    let entity = join_direct(&mut a, conn, 7, 10).await;
    assert_eq!(a.conn_epoch.get(&conn), Some(&7));
    assert!(a.handle_msg(
        ShardMsg::Leave {
            conn,
            entity,
            epoch: 7
        },
        &tctx(11)
    ));
    assert!(
        !a.conn_epoch.contains_key(&conn),
        "the matched leave must prune the epoch entry"
    );
    assert_eq!(
        a.conn_tombstone.get(&conn),
        Some(&(7, 11)),
        "and write its tombstone (epoch, write tick)"
    );

    // Arm 2: the registry broadcasts every leave to all shards; here
    // the leave carries an entity this shard does NOT hold for that
    // connection (the stale-leave guard keeps the connection row —
    // it belongs to a live join), yet its epoch entry is still
    // pruned: the leave proves that join is dead HERE.
    let other = ConnectionId(5);
    let other_entity = join_direct(&mut a, other, 3, 12).await;
    assert_eq!(a.conn_epoch.get(&other), Some(&3));
    assert!(a.handle_msg(
        ShardMsg::Leave {
            conn: other,
            entity: other_entity.wrapping_add(1),
            epoch: 3
        },
        &tctx(13)
    ));
    assert!(
        a.conns.contains_key(&PlayerId(other.0)),
        "the stale-leave guard keeps the live join's row"
    );
    assert!(
        !a.conn_epoch.contains_key(&other),
        "the unmatched arm still prunes the epoch entry"
    );

    // Re-join safety (the prune's documented counterpart): a fresh
    // join carries a strictly newer epoch and re-inserts.
    join_direct(&mut a, other, 4, 14).await;
    assert_eq!(
        a.conn_epoch.get(&other),
        Some(&4),
        "re-join re-inserts the epoch entry"
    );
}

/// Table-prune lock 2 — the tombstone gate keeps rejecting a stale
/// Migrate within the TTL window, then the tombstone expires at the
/// first sweep past TTL + cadence, after which the same migration is
/// accepted again (the observable accept-path of expiry; the
/// migrate-in re-insertion of `conn_epoch` is asserted as well).
#[tokio::test]
async fn stale_migrate_rejected_then_tombstone_expires() {
    let mut a = bare_shard(1);
    let conn = ConnectionId(6);
    let wire = join_direct(&mut a, conn, 2, 100).await;
    assert!(a.handle_msg(
        ShardMsg::Leave {
            conn,
            entity: wire,
            epoch: 2
        },
        &tctx(101)
    ));
    assert_eq!(
        a.conn_tombstone.get(&conn),
        Some(&(2, 101)),
        "the leave wrote the tombstone (epoch, write tick)"
    );

    // The ghost arrives WITHIN the TTL window. `at_tick < ctx.tick`
    // so the install gate is open — only the epoch gate can stop it.
    // Existing behavior preserved: rejected.
    assert!(a.handle_msg(ghost_migrate(conn, 2, wire, 99), &tctx(102)));
    assert!(
        !a.conns.contains_key(&PlayerId(conn.0)),
        "the ghost migrate must not install the dead join"
    );
    assert_eq!(
        a.conn_tombstone.get(&conn),
        Some(&(2, 101)),
        "rejection leaves the tombstone untouched"
    );

    // Advance the clock through CONTROL phases (which run the lazy
    // sweep). First sweep ever → runs immediately at tick 200:
    // tombstone age 99 < TTL, kept. At tick 712 (512 past the last
    // sweep) the next sweep fires: age 611 >= TTL → expired.
    assert!(a.step_phases(&tinfo(200)));
    assert_eq!(
        a.conn_tombstone.get(&conn),
        Some(&(2, 101)),
        "inside the TTL window the sweep keeps the guard"
    );
    assert!(a.step_phases(&tinfo(712)));
    assert!(
        !a.conn_tombstone.contains_key(&conn),
        "past TTL + a sweep boundary the tombstone expires"
    );

    // Observable accept-path: the same (stale) migration now passes
    // the gate — proving expiry opened it — and migrate-in
    // re-inserts the pruned epoch entry (see CHANGE 1's comment).
    assert!(a.handle_msg(ghost_migrate(conn, 2, wire, 700), &tctx(713)));
    assert!(
        a.conns.contains_key(&PlayerId(conn.0)),
        "with the tombstone expired the gate no longer rejects"
    );
    assert_eq!(
        a.conn_epoch.get(&conn),
        Some(&2),
        "migrate-in re-inserts the epoch entry"
    );
}

/// Whether shard `s` reported `wire` as migrating out at tick `t-1`
/// (the subtlety: that despawn happens in tick `t`'s phase 4 — AFTER
/// the content observation — so the raw content of tick `t` still
/// shows it there, and the protocol ownership is "raw content minus
/// last tick's reports").
fn reported_out(migrated: &HashMap<(u64, usize), Vec<u64>>, t: u64, s: usize, wire: u64) -> bool {
    migrated
        .get(&(t.saturating_sub(1), s))
        .map(|v| v.contains(&wire))
        .unwrap_or(false)
}

/// The shard(s) that own `wire` at tick index `t` per the protocol.
fn owners_at(
    content: &[Vec<(u64, f32, f32, i8)>; 2],
    migrated: &HashMap<(u64, usize), Vec<u64>>,
    t: u64,
    wire: u64,
) -> Vec<usize> {
    (0..2)
        .filter(|&s| {
            content[s].iter().any(|(w, _, _, _)| *w == wire) && !reported_out(migrated, t, s, wire)
        })
        .collect()
}
