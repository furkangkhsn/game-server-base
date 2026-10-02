//! The member gauge across a migration (F18): which shard's sample
//! counts a migrating player, tick index by tick index.
//!
//! The contract (`docs/CROSS-SHARD.md` §4d, `docs/DESIGN.md` §12): the
//! source stops counting the player at the commit tick `h` (phase 4
//! moves its row into the `Migrate`), the destination counts it from the
//! install, `h + 1` at the earliest (the `at_tick` gate). So two samples
//! taken on the same tick — or one tick apart — never count it twice;
//! in between it is in flight and neither counts it. Samples two or
//! more ticks apart CAN count it twice: that is a torn report, which a
//! consumer summing the shards' rows must not read as one instant.

use super::*;

/// Shards 0 and 1, unspawned (the test drives `step` itself), each
/// linked into the other's real inbox — a `Migrate` shard 0 commits is
/// in shard 1's CONTROL drain, as in a running room.
fn linked_pair() -> [ShardActor<TWorld, (), TState, TStrip>; 2] {
    let (tx0, rx0) = channel::<ShardMsg<TState, TStrip>>(16);
    let (tx1, rx1) = channel::<ShardMsg<TState, TStrip>>(16);
    // The self slot of each links vec (never targeted: TLogic's
    // neighbors are the other shard only).
    let (unused, _unused_rx) = channel::<ShardMsg<TState, TStrip>>(1);
    let shard = |index, inbox, links| {
        let (_tick_tx, tick_rx) = broadcast::channel(64);
        ShardActor::new(
            RoomConfig {
                id: RoomId(21),
                keepalive_hz: 0.0,
                metrics_cadence_hz: 0.0,
                ..Default::default()
            },
            index,
            TWorld::default(),
            Box::new(rig_logic(index)),
            tick_rx,
            inbox,
            links,
            1,
            metrics_null(),
            None,
        )
    };
    [
        shard(0, rx0, vec![unused.clone(), tx1]),
        shard(1, rx1, vec![tx0, unused]),
    ]
}

/// The members each shard's sample reports after its step of every tick
/// `1..=LAST`, with shard `first` stepping first on each tick. A player
/// joins shard 0 on tick 1 standing in shard 1's region (x = 0), so it
/// crosses on its very next step: the commit tick is 2.
async fn members_per_tick(first: usize) -> [[u32; LAST + 1]; 2] {
    let mut s = linked_pair();
    let (reply, joined) = oneshot::channel();
    let (out, _out_rx) = mpsc::channel::<FrameBatch>(64);
    let join = ShardMsg::Join {
        conn: ConnectionId(10),
        epoch: 1,
        identity: String::new(),
        out,
        reply,
        claims: None,
    };
    assert!(s[0].handle_msg(join, 1));
    let (_wire, _actions) = joined.await.expect("reply").expect("joined");
    let mut members = [[0; LAST + 1]; 2];
    for shard in 0..2 {
        members[shard][1] = s[shard].sample().members;
    }
    for t in 2..=LAST as u64 {
        for shard in [first, 1 - first] {
            assert!(s[shard].step(&tinfo(t)), "shard {shard} keeps running");
            members[shard][t as usize] = s[shard].sample().members;
        }
    }
    members
}

const LAST: usize = 6;
/// The tick shard 0 commits the migration on.
const COMMIT: usize = 2;

#[tokio::test]
async fn a_migrating_player_is_counted_once_at_most_per_tick() {
    for first in [0, 1] {
        let [src, dst] = members_per_tick(first).await;
        // The exact hand-over: the source counts it through the tick
        // before the commit, the destination from the tick after.
        for t in 1..=LAST {
            assert_eq!(src[t], u32::from(t < COMMIT), "source, tick {t}");
            assert_eq!(dst[t], u32::from(t > COMMIT), "destination, tick {t}");
        }
        assert_eq!(
            src[COMMIT] + dst[COMMIT],
            0,
            "on the commit tick the player is in flight: counted by neither"
        );
        // Never twice for samples up to one tick apart (two shards'
        // ticker subscriptions may differ by one tick).
        for (x, &at_src) in src.iter().enumerate().skip(1) {
            for (y, &at_dst) in dst.iter().enumerate().skip(1) {
                assert!(
                    x.abs_diff(y) > 1 || at_src + at_dst <= 1,
                    "shard 0 at tick {x} and shard 1 at tick {y} both count \
                     the player (first = {first})"
                );
            }
        }
        // Two ticks apart is a torn pair: both count it. This is the
        // report the loadgen must not sum (`loadgen::report::spread`).
        assert_eq!(src[COMMIT - 1] + dst[COMMIT + 1], 2, "the torn pair");
    }
}
