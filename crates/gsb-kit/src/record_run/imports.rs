//! The records a sharded team frame does not encode itself, in the
//! record run: a LENT record (a neighbour's entity in the border strip —
//! a typed value, encoded here) and an IMPORTED one (a body another
//! shard's codec wrote, spliced in verbatim). The export body is the
//! codec's body alone — no framing, no id: the importing shard frames it
//! exactly as its own records, so a client cannot tell the three apart.
//!
//! The framing is the codec TYPE's (`RecordCodec::RUN`): every shard of
//! a room runs one game, so the exporter and the importer cannot
//! disagree; `the_run_is_the_codecs_not_the_rooms` pins that a room's
//! frames follow its game's codec and nothing else.

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::room::{GameLogic, TickCtx};
use gsb_core::shard::{
    BorderRecord, ShardLogic, TeamImport, TeamImports, TeamRecord, interleaved_id,
};

use bevy_ecs::prelude::World;

use super::game::{FixLike, Pair};
use super::layout::parts;
use super::rooms::plain;
use super::{RADIUS, SHARDS};
use crate::codec::RecordCodec;
use crate::game::Game;
use crate::sharded::ShardedTeamRoom;
use crate::space::{GridPartition2, VisionGrid2};
use crate::team::Team;
use crate::testing::{FixCodec, PackedCodec, Position, WirePos, fix_lent_pos, packed_body};

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
    }
}

type TeamShard<C> = ShardedTeamRoom<Pair<C>, GridPartition2<Position>, VisionGrid2<Position>>;

/// Shard 0 (x < 0, y < 0) with a team-0 viewer at (-10, -10).
fn shard0<C: FixLike>(delta: bool) -> (World, TeamShard<C>, u64) {
    let mut world = World::new();
    let room = ShardedTeamRoom::with_shard(plain::<C>(0), VisionGrid2::new(RADIUS), fix_lent_pos);
    let mut room = if delta { room.with_delta() } else { room };
    let a = room.on_join_as(&mut world, ConnectionId(1), "-10:-10:0");
    (world, room, a.entity)
}

/// What another shard exported for team 0: one record, its body as
/// that shard's codec `C` wrote it.
fn imported<C: FixLike>(wire: u64, at: WirePos) -> TeamImports {
    let mut body = BytesMut::new();
    C::default().encode(wire, &at, &mut body);
    let mut im = TeamImports::default();
    im.insert(TeamImport {
        from: 3,
        tick: 1,
        records: vec![TeamRecord {
            team: 0,
            wire,
            bytes: body.freeze(),
        }],
    });
    im.settle();
    im
}

/// One tick of shard 0: update, the TEAMS phase with a lent record
/// (a neighbour's entity 5 from the viewer) and one import, team 0's
/// frame. Returns the frame and the export.
fn tick<C: FixLike>(delta: bool) -> (Vec<u8>, Vec<TeamRecord>, [u64; 3]) {
    let (mut world, mut room, own) = shard0::<C>(delta);
    let (lent, far) = (interleaved_id(1, SHARDS, 9), interleaved_id(3, SHARDS, 4));
    let borrowed = [BorderRecord {
        wire: lent,
        state: WirePos { x: 2, y: -10 },
    }];
    let imports = imported::<C>(far, WirePos { x: 70, y: 60 });
    room.update(&mut world, &ctx(1));
    let export = room
        .team_exchange(&mut world, &ctx(1), &borrowed, &imports)
        .expect("the composite takes part");
    let mut out = BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &Team(0), &borrowed, &mut out));
    (out.to_vec(), export.records, [own, lent, far])
}

#[test]
fn lent_and_imported_records_ride_the_run_like_own_ones() {
    for delta in [false, true] {
        let (frame, export, [own, lent, far]) = tick::<PackedCodec>(delta);
        let p = parts(&frame, true);
        assert_eq!(
            p.records,
            [(own, -10, -10), (lent, 2, -10), (far, 70, 60)]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>(),
            "own, lent and imported, each `id + body` in the one run (delta: {delta})"
        );
        // The export: the codec's body alone (no framing, no id).
        let mut want = BytesMut::new();
        packed_body(&WirePos { x: -10, y: -10 }, &mut want);
        let mine = export.iter().find(|r| r.wire == own).expect("exported");
        assert_eq!(mine.bytes, Bytes::from(want.to_vec()));
    }
}

/// The twin: the entities framing of the same tick carries the same
/// records — and an import encoded by the entities codec is framed as
/// an `entities` entry, byte for byte what the owner's frame holds.
#[test]
fn the_run_is_the_codecs_not_the_rooms() {
    let (ent, _, ids) = tick::<FixCodec>(true);
    let (run, _, ids2) = tick::<PackedCodec>(true);
    assert_eq!(ids, ids2);
    assert_eq!(parts(&ent, false).records, parts(&run, true).records);
    const { assert!(!<Pair<FixCodec> as Game>::Codec::RUN) };
    const { assert!(<Pair<PackedCodec> as Game>::Codec::RUN) };
}
