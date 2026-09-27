//! The per-cause disconnect policy (BACKLOG F27) through EVERY kit room,
//! the sharded wrappers included: an override answers its cause and only
//! it, in the same room as the room-wide rule; the default answers every
//! cause alike, as before.

use gsb_core::room::ResumeFound;

use super::*;
use crate::testing::fix_lent_pos;

use DisconnectCause::{ConnectionClosed, IdleInput, Kicked};

/// Join one player per cause and end each membership with its cause, in
/// order: the answer, and whether the ledger holds the identity after.
fn ends(mut room: impl GameLogic<World>, causes: &[DisconnectCause]) -> Vec<(Detach, bool)> {
    let mut world = World::new();
    let mut got = Vec::new();
    for (i, &cause) in causes.iter().enumerate() {
        let identity = format!("p{i}");
        let admission = room.on_join(&mut world, ConnectionId(i as u64 + 1));
        let answer = room.on_disconnect_with(&mut world, admission.player, &identity, cause);
        let held = matches!(
            room.resume_lookup(&world, &identity),
            ResumeFound::Held(p) if p == admission.player
        );
        got.push((answer, held));
    }
    got
}

fn check_causes<R: Policy>(name: &str, make: impl Fn() -> R) {
    let d = Duration::from_secs(5);
    let hold = |grace, to| (Detach::Hold { grace, to }, true);
    let default = hold(Some(crate::DEFAULT_DISCONNECT_GRACE), ExpireTo::AiHandover);
    let gone = (Detach::Despawn, false);
    let all = [ConnectionClosed, IdleInput, Kicked];
    let cases = [
        (
            ends(make(), &all),
            vec![default, default, default],
            "no override: every cause gets the room-wide rule",
        ),
        (
            ends(
                make().policy_for(Kicked, Some(Duration::ZERO), ExpireTo::Despawn),
                &all,
            ),
            vec![default, default, gone],
            "kicked → despawn, the others parked, in one room",
        ),
        (
            ends(
                make()
                    .policy_for(IdleInput, Some(d), ExpireTo::Despawn)
                    .policy(None, ExpireTo::AiHandover),
                &all,
            ),
            vec![
                hold(None, ExpireTo::AiHandover),
                hold(Some(d), ExpireTo::Despawn),
                hold(None, ExpireTo::AiHandover),
            ],
            "the room-wide builder leaves an override alone",
        ),
        (
            ends(
                make()
                    .policy_for(Kicked, Some(Duration::ZERO), ExpireTo::Despawn)
                    .policy_for(Kicked, Some(d), ExpireTo::Despawn),
                &[Kicked],
            ),
            vec![hold(Some(d), ExpireTo::Despawn)],
            "a second override of a cause replaces the first",
        ),
        (
            ends(
                make()
                    .policy(Some(Duration::ZERO), ExpireTo::Despawn)
                    .policy_for(ConnectionClosed, None, ExpireTo::AiHandover),
                &all,
            ),
            vec![hold(None, ExpireTo::AiHandover), gone, gone],
            "a despawning room that parks only the dropped",
        ),
    ];
    for (got, want, what) in cases {
        assert_eq!(got, want, "{name}: {what}");
    }
    // The cause-less hook (no core caller any more) keeps the room-wide
    // rule whatever the overrides say.
    assert_eq!(
        disconnect(make().policy_for(ConnectionClosed, Some(Duration::ZERO), ExpireTo::Despawn)),
        default.0,
        "{name}: on_disconnect is the room-wide rule"
    );
}

#[test]
fn every_room_answers_each_cause_with_its_policy() {
    check_causes("open", || OpenRoom::with_game(game()));
    check_causes("aoi", || AoiRoom::with_game(game(), Grid2::new(20.0)));
    check_causes("team", || {
        TeamRoom::with_game(game(), VisionGrid2::new(10.0))
    });
    check_causes("pvs", || SectorRoom::with_game(game(), fixture_map()));
    check_causes("sharded", sharded);
    check_causes("sharded × spatial", || {
        ShardedSpatialRoom::with_shard(sharded(), Grid2::new(20.0))
    });
    check_causes("sharded × team", || {
        ShardedTeamRoom::with_shard(sharded(), VisionGrid2::new(10.0), fix_lent_pos)
    });
}
