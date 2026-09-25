//! The per-record send rate (`RecordCodec::send_every`, KIT-ARCHITECTURE
//! §10 "A10") room kind by room kind, on the record-run twins: the same
//! seeded session, the first room over the fixture's default codec, the
//! second over `Rated<PackedCodec>` — the fixture's record in the record
//! run WITH a send rate (every step, 2nd, 4th or 8th by the x band; the
//! players and the wandering NPCs cross bands, so records change class).
//!
//! - The full-only kinds (open, PVS, plain sharded, both team rooms
//!   without delta) IGNORE the rate: the two rooms carry the same
//!   content, frame by frame ([`Check::Same`]).
//! - The delta kinds (AOI, team with delta, sharded × spatial, sharded
//!   team with delta) hold every changed record to its due steps: the
//!   rated clients see the same records, exits and fulls, and every
//!   value at most 7 ticks old — exactly 7 at worst, the every-8th
//!   class's bound — and every full carries the current values
//!   ([`Lag`]). That covers own, lent (the border strip) and imported
//!   records, migrations, the record run's framing and the convergence
//!   at every keep-alive.

mod export;

use super::compare::{Check, Stats};
use super::lag::Lag;
use super::rig::Rig;
use super::rooms::*;
use super::script::Script;
use super::shards::{Key, Mig, Shards};
use super::{SHARDS, TICKS};
use crate::sharded::KitMig;
use crate::testing::{BAND_MAX, FixCodec, FixMig, PackedCodec, Rated};

use bevy_ecs::prelude::World;
use gsb_core::registry::RoomFactory;

/// The rated side: the record run with the band classes.
type R = Rated<PackedCodec>;

/// The staleness bound of the band classes.
const BOUND: u64 = BAND_MAX.ticks() - 1;

async fn single_run<G, St, Sp>(
    factory: RoomFactory<World, G, St, Sp>,
    seed: u64,
    check: Check,
) -> Rig
where
    G: Eq + std::hash::Hash + Clone + std::fmt::Debug + Send + 'static,
    St: std::fmt::Debug + Send + 'static,
    Sp: std::fmt::Debug + Clone + PartialEq + Send + 'static,
{
    let mut rig = Rig::new(factory, 1).await;
    rig.check = check;
    let mut script = Script::new(seed, 20);
    for _ in 0..TICKS {
        rig.play(&script.tick()).await;
    }
    assert_eq!(rig.counters().errors, 0);
    rig
}

fn shard_run<G: Key, St: Mig>(
    build: fn(bool, usize) -> Shard<G, St>,
    seed: u64,
    check: Check,
) -> Shards<G, St> {
    let mut twin = Shards::new(build, SHARDS);
    twin.check = check;
    let mut script = Script::new(seed, 20);
    for _ in 0..TICKS {
        twin.play(&script.tick());
    }
    assert_eq!(twin.counters().errors, 0);
    assert!(twin.spread().len() == SHARDS, "every shard hosted players");
    twin
}

/// The same content on both sides.
fn alike((a, b): &(Stats, Stats)) {
    assert_eq!(
        (a.fulls, a.deltas, a.records, a.digest),
        (b.fulls, b.deltas, b.records, b.digest),
        "the rate changed a full-only room's frames"
    );
    assert!(a.records > 2_000, "{a:?}");
}

/// The rate held changes back — to exactly `bound` at worst — and the
/// rated side sent fewer records.
fn held(check: &Check, (a, b): &(Stats, Stats), exact: bool, bound: u64) {
    let Check::Lag(lag) = check else {
        unreachable!("a rated twin")
    };
    assert_eq!(lag.max_seen, bound, "the bound is reached, never passed");
    assert!(lag.lagging > 1_000, "changes waited: {}", lag.lagging);
    assert_eq!(
        lag.exact_fulls > 0,
        exact,
        "fulls checked: {}",
        lag.exact_fulls
    );
    assert_eq!((a.fulls, a.removed, a.exits), (b.fulls, b.removed, b.exits));
    assert!(
        b.records * 10 < a.records * 8,
        "the rate saves records: {} vs {}",
        b.records,
        a.records
    );
}

fn rated(bound: u64, exact: bool) -> Check {
    Check::Lag(Lag::new(bound, exact))
}

#[tokio::test(start_paused = true)]
async fn full_only_rooms_ignore_the_rate() {
    let open = |run| if run { open::<R>() } else { open::<FixCodec>() };
    alike(
        &single_run(single(open), 0xA10_0001, Check::Same)
            .await
            .stats,
    );
    let pvs = |run| if run { pvs::<R>() } else { pvs::<FixCodec>() };
    alike(&single_run(single(pvs), 0xA10_0002, Check::Same).await.stats);
    let team = |run| {
        if run {
            team::<R>(false)
        } else {
            team::<FixCodec>(false)
        }
    };
    alike(
        &single_run(single(team), 0xA10_0003, Check::Same)
            .await
            .stats,
    );
}

#[test]
fn full_only_shards_ignore_the_rate() {
    let plain = |run, i| -> Shard<(), KitMig<FixMig>> {
        if run {
            Box::new(plain::<R>(i))
        } else {
            Box::new(plain::<FixCodec>(i))
        }
    };
    alike(&shard_run(plain, 0xA10_0004, Check::Same).stats);
    let team = |run, i| {
        if run {
            team_shard::<R>(i, false)
        } else {
            team_shard::<FixCodec>(i, false)
        }
    };
    alike(&shard_run(team, 0xA10_0005, Check::Same).stats);
}

#[tokio::test(start_paused = true)]
async fn the_aoi_room_holds_changes_to_the_rate() {
    let aoi = |run| if run { aoi::<R>() } else { aoi::<FixCodec>() };
    let rig = single_run(single(aoi), 0xA10_0006, rated(BOUND, true)).await;
    held(&rig.check, &rig.stats, true, BOUND);
    assert!(rig.stats.1.exits > 0 && rig.counters().private_fulls > 0);
}

#[tokio::test(start_paused = true)]
async fn the_team_room_holds_changes_to_the_rate() {
    let team = |run| {
        if run {
            team::<R>(true)
        } else {
            team::<FixCodec>(true)
        }
    };
    let rig = single_run(single(team), 0xA10_0007, rated(BOUND, true)).await;
    held(&rig.check, &rig.stats, true, BOUND);
    assert!(rig.stats.1.removed > 0 && rig.counters().private_fulls > 0);
}

#[test]
fn the_sharded_spatial_room_holds_changes_to_the_rate() {
    let spatial = |run, i| {
        if run {
            spatial::<R>(i)
        } else {
            spatial::<FixCodec>(i)
        }
    };
    let check = Check::Lag(Lag::new(BOUND, true).on_due());
    let twin = shard_run(spatial, 0xA10_0008, check);
    held(&twin.check, &twin.stats, true, BOUND);
    let Check::Lag(lag) = &twin.check else {
        unreachable!()
    };
    assert!(
        lag.on_time > 1_000,
        "upserts checked on time: {}",
        lag.on_time
    );
    assert!(twin.stats.1.exits > 0 && twin.counters().private_fulls > 0);
}

/// The sharded team room: own and lent records under the viewer's
/// schedule, imported bodies under their owner's — one schedule. An
/// import reaches the viewer one tick after its owner published it (the
/// team exchange's relay, as without a rate), so its bound is one tick
/// longer; and a full carries the imports as their owner last published
/// them, so the fulls are held to the bound, not to equality.
#[test]
fn the_sharded_team_room_holds_changes_to_the_rate() {
    let team = |run, i| {
        if run {
            team_shard::<R>(i, true)
        } else {
            team_shard::<FixCodec>(i, true)
        }
    };
    let twin = shard_run(team, 0xA10_0009, rated(BOUND + 1, false));
    held(&twin.check, &twin.stats, false, BOUND + 1);
    assert!(twin.stats.1.removed > 0 && twin.counters().private_fulls > 0);
}
