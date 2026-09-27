//! The `team × sharded` composite on the REAL actors: a live registry
//! (its team hub relaying the shards' exports) over four shard actors on
//! a 2×2 grid, three teams (`docs/CROSS-SHARD.md` §8b — "Cephe" in
//! miniature). What each client's view holds is what its out channel
//! actually received, applied under the kit's client rules.

use gsb_core::shard::{TEAM_EXPORT_TTL_TICKS, TeamExport, TeamRecord};

use super::*;
use crate::codec::RecordCodec;

mod client;
mod dropped;
mod front;
mod kick;
mod migration;
mod rig;

use rig::Rig;

/// A record body as the shards' codec writes it.
fn body(wire: u64, x: i32, y: i32) -> bytes::Bytes {
    let mut buf = bytes::BytesMut::new();
    FixCodec.encode(wire, &WirePos { x, y }, &mut buf);
    buf.freeze()
}

/// Team 0 has a scout on shard 0 and a player on shard 3; a team-1
/// raider stands 10 from the scout on shard 0; a team-2 loner stands
/// alone on shard 1. The far team-0 player sees its scout (allies
/// map-wide) AND the raider (the scout sees it — on another shard); the
/// raider sees the scout (local vision) but not the far player; the
/// loner sees only itself, and nobody sees the loner. Run in both
/// snapshot modes.
async fn allies_map_wide_enemies_through_far_vision(delta: bool) {
    let mut rig = Rig::new(delta).await;
    let scout = rig.join(1, "0:-50:-50").await;
    let far = rig.join(2, "0:50:50").await;
    let raider = rig.join(3, "1:-40:-50").await;
    let loner = rig.join(4, "2:50:-50").await;
    rig.steps(4).await;
    assert_eq!(rig.members, [2, 1, 0, 1]);
    let wire = |i: usize| rig.clients[i].wire;
    let (s, f, r, l) = (wire(scout), wire(far), wire(raider), wire(loner));

    let far_view: Vec<bool> = [s, f, r, l]
        .iter()
        .map(|&w| rig.clients[far].sees(w))
        .collect();
    assert_eq!(far_view, [true, true, true, false], "ally + seen enemy");
    let raider_view: Vec<bool> = [s, f, r, l]
        .iter()
        .map(|&w| rig.clients[raider].sees(w))
        .collect();
    assert_eq!(raider_view, [true, false, true, false]);
    assert_eq!(rig.clients[loner].view.ids().collect::<Vec<_>>(), [l]);

    // Isolation over the whole run: the loner's view never held another
    // team's record; no team-0 view ever held the loner.
    for (_, ids) in &rig.clients[loner].history {
        assert!(ids.iter().all(|&w| w == l), "{ids:?}");
    }
    for c in [scout, far] {
        assert!(
            rig.clients[c]
                .history
                .iter()
                .all(|(_, ids)| !ids.contains(&l))
        );
    }
    assert!(rig.clients.iter().all(|c| c.doubled == 0));
}

#[tokio::test(start_paused = true)]
async fn allies_map_wide_and_enemies_through_far_vision_full() {
    allies_map_wide_enemies_through_far_vision(false).await;
}

#[tokio::test(start_paused = true)]
async fn allies_map_wide_and_enemies_through_far_vision_delta() {
    allies_map_wide_enemies_through_far_vision(true).await;
}

/// An enemy right across a seam from a scout: the scout's shard sees it
/// in its border strip and exports it under team 0; the enemy's own
/// shard hosts a team-0 viewer too, which shows the enemy ONCE, with
/// the enemy's own (fresh) record. A far team-0 viewer sees it as well.
#[tokio::test(start_paused = true)]
async fn an_enemy_across_a_seam_is_one_record() {
    let mut rig = Rig::new(true).await;
    let scout = rig.join(1, "0:-5:-50").await;
    let enemy = rig.join(2, "1:5:-50").await;
    let near = rig.join(3, "0:80:-80").await; // shard 1, 75+ from the enemy
    let far = rig.join(4, "0:50:50").await;
    rig.steps(4).await;
    let e = rig.clients[enemy].wire;
    for c in [scout, near, far] {
        assert!(rig.clients[c].sees(e), "client {c}");
    }
    assert_eq!(rig.clients[near].view.get(e), Some(&(5, -50)));
    // The enemy moves on its own shard: the near viewer gets its own
    // shard's record — the move shows at once, never a stale copy.
    rig.clients[enemy].move_to(6.0, -52.0);
    rig.step().await;
    assert_eq!(rig.clients[near].view.get(e), Some(&(6, -52)));
    assert!(rig.clients.iter().all(|c| c.doubled == 0));
}

/// A member who leaves is gone from the far ally's view by the next
/// export — and stays gone past the TTL (no ghost).
#[tokio::test(start_paused = true)]
async fn a_member_who_leaves_leaves_no_ghost() {
    let mut rig = Rig::new(true).await;
    let gone = rig.join(1, "0:-50:-50").await;
    let far = rig.join(2, "0:50:50").await;
    rig.steps(3).await;
    let w = rig.clients[gone].wire;
    assert!(rig.clients[far].sees(w));
    rig.leave(gone).await;
    rig.steps(3).await;
    assert!(!rig.clients[far].sees(w), "gone with the next export");
    rig.steps(TEAM_EXPORT_TTL_TICKS as u32 + 2).await;
    assert!(!rig.clients[far].sees(w));
}

/// A shard that stops exporting: a record its (last) export named stays
/// on the receivers until the TTL — then goes. Here the silent source
/// is shard 2, which hosts nobody and so never exports on its own; the
/// hub relays its one (forged) export like any other.
#[tokio::test(start_paused = true)]
async fn a_silent_sources_records_expire_after_the_ttl() {
    let mut rig = Rig::new(true).await;
    let far = rig.join(1, "0:50:50").await;
    rig.steps(2).await;
    let ghost = gsb_core::shard::interleaved_id(2, rig::SHARDS, 77);
    let export = TeamExport {
        views: Vec::new(),
        records: vec![TeamRecord {
            team: 0,
            wire: ghost,
            bytes: body(ghost, -60, 60),
        }],
        over_budget: 0,
    };
    let at = rig.tick;
    rig.forge(2, export).await;
    rig.step().await;
    assert!(rig.clients[far].sees(ghost), "relayed and shown");
    while rig.tick < at + TEAM_EXPORT_TTL_TICKS - 1 {
        rig.step().await;
        assert!(rig.clients[far].sees(ghost), "tick {}", rig.tick);
    }
    rig.steps(2).await;
    assert!(
        !rig.clients[far].sees(ghost),
        "expired at tick {}",
        rig.tick
    );
}

/// A receiver holding a source's outdated set — what a dropped export
/// leaves behind — is healed by that source's next export: wholesale
/// replacement, one tick.
#[tokio::test(start_paused = true)]
async fn a_stale_set_heals_with_the_next_export() {
    let mut rig = Rig::new(true).await;
    let scout = rig.join(1, "0:-50:-50").await;
    let far = rig.join(2, "0:50:50").await;
    rig.steps(3).await;
    let s = rig.clients[scout].wire;
    let stale = TeamExport {
        views: vec![0],
        records: vec![TeamRecord {
            team: 0,
            wire: s,
            bytes: body(s, -1, -1),
        }],
        over_budget: 0,
    };
    rig.forge(0, stale).await;
    rig.step().await;
    assert_eq!(rig.clients[far].view.get(s), Some(&(-1, -1)), "stale");
    rig.step().await;
    assert_eq!(rig.clients[far].view.get(s), Some(&(-50, -50)), "healed");
}

/// The hub serves the room's CURRENT incarnation only: an export stamped
/// with another install generation (a dead incarnation's late message)
/// changes nothing.
#[tokio::test(start_paused = true)]
async fn an_export_of_another_incarnation_is_ignored() {
    let mut rig = Rig::new(true).await;
    let far = rig.join(1, "0:50:50").await;
    rig.steps(2).await;
    let ghost = gsb_core::shard::interleaved_id(2, rig::SHARDS, 5);
    let export = TeamExport {
        views: Vec::new(),
        records: vec![TeamRecord {
            team: 0,
            wire: ghost,
            bytes: body(ghost, -60, 60),
        }],
        over_budget: 0,
    };
    rig.forge_as(1, 2, export.clone()).await;
    rig.steps(2).await;
    assert!(!rig.clients[far].sees(ghost), "a dead incarnation's export");
    rig.forge(2, export).await;
    rig.step().await;
    assert!(
        rig.clients[far].sees(ghost),
        "the same export, this incarnation"
    );
}
