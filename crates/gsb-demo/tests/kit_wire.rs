//! Byte compatibility across the kit/demo crate boundary
//! (KIT-ARCHITECTURE §5): the kit's envelope (`gsb_kit::proto`, opaque
//! `repeated bytes` records and cell exits) and the demo's typed mirror
//! (`gsb_demo::game`, `repeated EntityRecord` / `repeated CellExit`) are
//! two definitions of ONE wire format. Existing clients (the load
//! generator, `examples/client.rs`) decode with the mirror, the kit
//! writes by hand against its own proto — these tests pin the two to the
//! same bytes, on constructed frames and on frames a real demo room
//! produced.

use std::time::Duration;

use bevy_ecs::prelude::{Entity, World};
use bytes::{Bytes, BytesMut};
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::room::{GameLogic, TickCtx};
use gsb_core::rpc::RpcReply;
use gsb_demo::aoi::{AoiRoom, Cell};
use gsb_demo::components::{Position, WireId};
use gsb_demo::game::{CellExit, EntityRecord, InputAck};
use gsb_demo::prelude::*;
use gsb_kit::proto as kit;
use gsb_protocol::base::RpcResponse;
use prost::Message;

type Typed = gsb_demo::game::WorldSnapshot;
type TypedPrivate = gsb_demo::game::Private;
use gsb_demo::game::private as typed_private;

fn rec(entity: u64, x: i32, y: i32) -> EntityRecord {
    EntityRecord { entity, x, y }
}

/// The kit form of a typed snapshot: every record and cell exit as the
/// opaque body the demo's codec / the kit's `Grid2` writes.
fn to_kit(t: &Typed) -> kit::WorldSnapshot {
    kit::WorldSnapshot {
        sequence: t.sequence,
        entities: t.entities.iter().map(Message::encode_to_vec).collect(),
        removed: t.removed.clone(),
        cell_exits: t.cell_exits.iter().map(Message::encode_to_vec).collect(),
        delta: t.delta,
        // The record run (A31): absent — this game did not opt in.
        records: Vec::new(),
    }
}

fn responses() -> Vec<RpcResponse> {
    vec![
        RpcResponse {
            id: 7,
            ok: true,
            op: 1005,
            reason: String::new(),
            payload: vec![0x08, 0x01],
        },
        RpcResponse {
            id: 8,
            ok: false,
            op: 1006,
            reason: "unknown item".into(),
            payload: Vec::new(),
        },
    ]
}

/// Both definitions encode to the same bytes, and each decodes the
/// other's bytes back to itself.
fn assert_same_wire<K: Message + Default + PartialEq, T: Message + Default + PartialEq>(
    name: &str,
    kit_msg: &K,
    typed_msg: &T,
) {
    let kit_bytes = kit_msg.encode_to_vec();
    let typed_bytes = typed_msg.encode_to_vec();
    assert_eq!(kit_bytes, typed_bytes, "{name}: identical bytes");
    assert!(
        K::decode(&typed_bytes[..]).expect("kit decodes the mirror") == *kit_msg,
        "{name}: the kit decodes the mirror's bytes to itself"
    );
    assert!(
        T::decode(&kit_bytes[..]).expect("the mirror decodes the kit") == *typed_msg,
        "{name}: the mirror decodes the kit's bytes to itself"
    );
}

/// Representative frames built both ways: a full snapshot, a delta with
/// removals + cell exits + upserts, and the two private-frame shapes (the
/// oneof carries an ack OR a one-shot full; responses ride beside
/// either).
#[test]
fn kit_envelope_and_typed_mirror_encode_identically() {
    let full = Typed {
        sequence: 42,
        entities: vec![rec(1, 3, -4), rec(2, 0, 0), rec(300, -70_000, 63)],
        ..Default::default()
    };
    assert_same_wire("full", &to_kit(&full), &full);

    let delta = Typed {
        sequence: 43,
        entities: vec![rec(2, 1, 0), rec(129, -64, 64)],
        removed: vec![3, 7, 128],
        cell_exits: vec![CellExit { x: -1, y: 2 }, CellExit { x: 0, y: 0 }],
        delta: true,
    };
    assert_same_wire("delta", &to_kit(&delta), &delta);

    let ack_kit = kit::Private {
        payload: Some(kit::private::Payload::Ack(InputAck {
            processed_up_to: 300,
        })),
        responses: responses(),
        game: Vec::new(),
    };
    let ack_typed = TypedPrivate {
        payload: Some(typed_private::Payload::Ack(InputAck {
            processed_up_to: 300,
        })),
        responses: responses(),
    };
    assert_same_wire("private ack + responses", &ack_kit, &ack_typed);

    let one_shot = Typed {
        sequence: 44,
        entities: vec![rec(1, 3, -4), rec(129, -64, 64)],
        ..Default::default()
    };
    let snap_kit = kit::Private {
        payload: Some(kit::private::Payload::Snapshot(to_kit(&one_shot))),
        responses: responses(),
        game: Vec::new(),
    };
    let snap_typed = TypedPrivate {
        payload: Some(typed_private::Payload::Snapshot(one_shot)),
        responses: responses(),
    };
    assert_same_wire("private snapshot + responses", &snap_kit, &snap_typed);
}

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
        kicks: Default::default(),
        paths: Default::default(),
    }
}

/// The entity carrying wire id `wire`.
fn entity_of(world: &mut World, wire: u64) -> Entity {
    world
        .query::<(Entity, &WireId)>()
        .iter(world)
        .find_map(|(e, w)| (w.get() == wire).then_some(e))
        .expect("an entity with this wire id")
}

/// A frame the kit wrote, decoded through both definitions: the same
/// content (every kit record / cell-exit body IS the typed message's
/// encoding), and both re-encode to the same bytes.
fn assert_frame_agrees(name: &str, bytes: &[u8]) -> Typed {
    let k = kit::WorldSnapshot::decode(bytes).expect("kit decode");
    let t = Typed::decode(bytes).expect("typed decode");
    assert_eq!(k.sequence, t.sequence, "{name}: sequence");
    assert_eq!(k.delta, t.delta, "{name}: delta flag");
    assert_eq!(k.removed, t.removed, "{name}: removed");
    let bodies: Vec<Vec<u8>> = t.entities.iter().map(Message::encode_to_vec).collect();
    assert_eq!(k.entities, bodies, "{name}: record bodies");
    let cells: Vec<Vec<u8>> = t.cell_exits.iter().map(Message::encode_to_vec).collect();
    assert_eq!(k.cell_exits, cells, "{name}: cell-exit bodies");
    assert_eq!(
        k.encode_to_vec(),
        t.encode_to_vec(),
        "{name}: the definitions re-encode identically"
    );
    t
}

/// Frames a real demo room (`AoiRoom`, the demo codec over the kit's
/// `Grid2`) wrote by hand: a fresh group's full, a delta carrying all
/// three change kinds at once, and a late joiner's one-shot private full
/// with RPC responses.
#[test]
fn room_frames_decode_identically_through_both_definitions() {
    let mut world = World::new();
    let mut room = AoiRoom::new(20.0);
    let observer = room.on_join(&mut world, ConnectionId(1));
    let e = entity_of(&mut world, observer.entity);
    world.entity_mut(e).insert(Position { x: 25.0, y: 25.0 }); // Cell(1,1)
    let leaver = world.spawn(Position { x: 5.0, y: 5.0 }).id(); // Cell(0,0)
    world.spawn(Position { x: 6.0, y: 6.0 }); // Cell(0,0): keeps it populated
    let loner = world.spawn(Position { x: 45.0, y: 45.0 }).id(); // Cell(2,2)
    let mover = world.spawn(Position { x: 25.0, y: 5.0 }).id(); // Cell(1,0)
    room.update(&mut world, &ctx(1));
    let group = Cell(1, 1);

    let mut out = BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(1), &group, &[], &mut out));
    let full = assert_frame_agrees("full", &out);
    assert!(!full.delta && full.entities.len() == 5, "{full:?}");

    // Tick 2: one entity leaves a still-populated cell (`removed`), the
    // lone entity of another cell leaves it (`cell_exits`), one moves in
    // place (`entities`) — and a second player joins the group's cell.
    world
        .entity_mut(leaver)
        .insert(Position { x: 500.0, y: 500.0 });
    world.entity_mut(loner).insert(Position {
        x: 500.0,
        y: -500.0,
    });
    world.entity_mut(mover).insert(Position { x: 27.0, y: 5.0 });
    let late = room.on_join(&mut world, ConnectionId(2));
    let e = entity_of(&mut world, late.entity);
    world.entity_mut(e).insert(Position { x: 30.0, y: 30.0 }); // Cell(1,1)
    room.update(&mut world, &ctx(2));

    let mut out = BytesMut::new();
    assert!(room.snapshot(&mut world, &ctx(2), &group, &[], &mut out));
    let delta = assert_frame_agrees("delta", &out);
    assert!(delta.delta, "{delta:?}");
    assert_eq!(delta.removed.len(), 1, "{delta:?}");
    assert_eq!(delta.cell_exits, vec![CellExit { x: 2, y: 2 }]);
    assert_eq!(delta.entities.len(), 2, "the mover and the joiner");

    // The late joiner's one-shot full, with two RPC answers beside it.
    let replies = [
        RpcReply {
            id: 7,
            ok: true,
            op: 1005,
            reason: String::new(),
            payload: Bytes::from_static(&[0x08, 0x01]),
        },
        RpcReply {
            id: 8,
            ok: false,
            op: 1006,
            reason: "unknown item".into(),
            payload: Bytes::new(),
        },
    ];
    let mut out = BytesMut::new();
    assert!(room.private(&mut world, late.player, &group, &replies, &mut out));
    let k = kit::Private::decode(&out[..]).expect("kit decode");
    let t = TypedPrivate::decode(&out[..]).expect("typed decode");
    assert_eq!(k.encode_to_vec(), t.encode_to_vec(), "private re-encodes");
    assert_eq!(k.responses, responses(), "the responses");
    assert!(k.game.is_empty(), "the demo ships no game payload");
    let (Some(kit::private::Payload::Snapshot(ks)), Some(typed_private::Payload::Snapshot(ts))) =
        (k.payload, t.payload)
    else {
        panic!("the one-shot full arm on both sides");
    };
    let bodies: Vec<Vec<u8>> = ts.entities.iter().map(Message::encode_to_vec).collect();
    assert_eq!(ks.entities, bodies, "one-shot record bodies");
    assert!(!ts.delta && ts.entities.len() == 4, "{ts:?}");
}
