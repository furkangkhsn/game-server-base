//! The verdict behind a closed connection (BACKLOG F28) through EVERY kit
//! room: an override for `ConnectionClosedBy(verdict)` answers that
//! verdict alone; a verdict without one takes the `ConnectionClosed`
//! override (what every closed connection got before F28), and with
//! neither the room-wide rule — the default is unchanged.

use gsb_core::conn::ServerClose;
use gsb_core::room::ResumeFound;

use super::*;
use crate::testing::fix_lent_pos;

use DisconnectCause::{ConnectionClosed, ConnectionClosedBy};

const BUDGET: DisconnectCause = ConnectionClosedBy(ServerClose::ViolationBudget);
const IDLE: DisconnectCause = ConnectionClosedBy(ServerClose::IdleTimeout);

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

fn check_verdicts<R: Policy>(name: &str, make: impl Fn() -> R) {
    let d = Duration::from_secs(5);
    let hold = |grace, to| (Detach::Hold { grace, to }, true);
    let default = hold(Some(crate::DEFAULT_DISCONNECT_GRACE), ExpireTo::AiHandover);
    let gone = (Detach::Despawn, false);
    let all = [ConnectionClosed, BUDGET, IDLE];
    let cases = [
        (
            ends(make(), &all),
            vec![default, default, default],
            "no override: every closed connection gets the room-wide rule",
        ),
        (
            ends(
                make().policy_for(BUDGET, Some(Duration::ZERO), ExpireTo::Despawn),
                &all,
            ),
            vec![default, gone, default],
            "a cheater's close despawns, the other closes park",
        ),
        (
            ends(
                make().policy_for(ConnectionClosed, Some(d), ExpireTo::Despawn),
                &all,
            ),
            vec![hold(Some(d), ExpireTo::Despawn); 3],
            "the ConnectionClosed override answers every verdict without its own",
        ),
        (
            ends(
                make()
                    .policy_for(ConnectionClosed, Some(d), ExpireTo::Despawn)
                    .policy_for(BUDGET, Some(Duration::ZERO), ExpireTo::Despawn),
                &all,
            ),
            vec![
                hold(Some(d), ExpireTo::Despawn),
                gone,
                hold(Some(d), ExpireTo::Despawn),
            ],
            "a verdict's own override wins over ConnectionClosed's",
        ),
    ];
    for (got, want, what) in cases {
        assert_eq!(got, want, "{name}: {what}");
    }
}

#[test]
fn every_room_selects_on_the_verdict_behind_a_close() {
    check_verdicts("open", || OpenRoom::with_game(game()));
    check_verdicts("aoi", || AoiRoom::with_game(game(), Grid2::new(20.0)));
    check_verdicts("team", || {
        TeamRoom::with_game(game(), VisionGrid2::new(10.0))
    });
    check_verdicts("pvs", || SectorRoom::with_game(game(), fixture_map()));
    check_verdicts("sharded", sharded);
    check_verdicts("sharded × spatial", || {
        ShardedSpatialRoom::with_shard(sharded(), Grid2::new(20.0))
    });
    check_verdicts("sharded × team", || {
        ShardedTeamRoom::with_shard(sharded(), VisionGrid2::new(10.0), fix_lent_pos)
    });
}
