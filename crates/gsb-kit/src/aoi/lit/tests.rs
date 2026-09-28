//! The lit AOI room's tests (A9), on a scripted room stepped in the
//! core's broadcast order: `update`, every player's group (phase 4a),
//! each group's frame once (4c, with the keep-alive on its cadence and
//! the core's "a group without members forgets its cache"), then each
//! player's private frame (4d). Every player's frames are kept and
//! applied to its own reference client (`ClientView`), so a test can ask
//! both what reached a viewer and what the viewer ends up holding.

use std::collections::{BTreeSet, HashMap};
use std::hash::Hash;
use std::time::Duration;

use bevy_ecs::prelude::{Entity, World};
use bytes::{Bytes, BytesMut};
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::room::{GameLogic, TickCtx};
use prost::Message;

use crate::aoi::{LitAoiRoom, LitGroup};
use crate::client::ClientView;
use crate::identity::WireId;
use crate::space::{Cell, Grid2};
use crate::testing::{Dec, Lamp, Position, Private, WorldSnapshot, private::Payload};

mod filter;
mod same;
mod switch;

/// The AOI cell edge.
const EDGE: f32 = 20.0;

/// The room under test.
type Room = LitAoiRoom<Lamp, Grid2>;

fn lit_room() -> Room {
    LitAoiRoom::with_game(Lamp::default(), Grid2::new(EDGE))
}

fn ctx(tick: u64) -> TickCtx<'static> {
    TickCtx {
        room: RoomId(1),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
        idle: Default::default(),
        kicks: Default::default(),
    }
}

/// One player of the scripted room.
struct Seat {
    player: PlayerId,
    entity: Entity,
    wire: u64,
    view: ClientView<Dec<false>>,
    /// This tick's frames: `(private?, payload)`.
    frames: Vec<(bool, Bytes)>,
}

/// The scripted room (module docs).
struct Sim<R: GameLogic<World>> {
    room: R,
    world: World,
    seats: Vec<Seat>,
    tick: u64,
    /// Each group's cached payload (the core's `GroupState::last`).
    last: HashMap<R::GroupKey, Bytes>,
}

impl<R: GameLogic<World>> Sim<R>
where
    R::GroupKey: Hash + Eq + Clone,
{
    fn new(room: R) -> Self {
        Self {
            room,
            world: World::new(),
            seats: Vec::new(),
            tick: 0,
            last: HashMap::new(),
        }
    }

    /// Join a player and place its entity at `(x, y)`; its seat index.
    fn join(&mut self, conn: u64, x: f32, y: f32) -> usize {
        let admission = self.room.on_join(&mut self.world, ConnectionId(conn));
        let wire = admission.entity;
        let entity = self
            .world
            .query::<(Entity, &WireId)>()
            .iter(&self.world)
            .find(|(_, w)| w.get() == wire)
            .map(|(e, _)| e)
            .expect("the joiner is stamped");
        self.world.entity_mut(entity).insert(Position { x, y });
        self.seats.push(Seat {
            player: admission.player,
            entity,
            wire,
            view: ClientView::new(Dec::new(EDGE)),
            frames: Vec::new(),
        });
        self.seats.len() - 1
    }

    /// Move seat `i`'s entity to `(x, y)`.
    fn at(&mut self, i: usize, x: f32, y: f32) {
        self.world
            .entity_mut(self.seats[i].entity)
            .insert(Position { x, y });
    }

    /// Seat `i`'s group this tick (after `step`).
    fn group(&self, i: usize) -> R::GroupKey {
        self.room.group_of(&self.world, self.seats[i].player)
    }

    /// One tick (module docs); `keep` = a keep-alive tick; the seats in
    /// `dropped` get their batch dropped (not applied; the room told).
    fn step_with(&mut self, keep: bool, dropped: &[usize]) {
        self.tick += 1;
        let ctx = ctx(self.tick);
        self.room.update(&mut self.world, &ctx);
        let groups: Vec<R::GroupKey> = (0..self.seats.len()).map(|i| self.group(i)).collect();
        self.last.retain(|g, _| groups.contains(g));
        let mut sent: HashMap<R::GroupKey, Bytes> = HashMap::new();
        for g in &groups {
            if sent.contains_key(g) {
                continue;
            }
            let mut out = BytesMut::new();
            let mut payload = None;
            if self.room.snapshot(&mut self.world, &ctx, g, &[], &mut out) {
                payload = Some(out.split().freeze());
                self.last.insert(g.clone(), payload.clone().expect("set"));
            }
            if keep && let Some(last) = self.last.get(g).cloned() {
                if self
                    .room
                    .keepalive(&mut self.world, &ctx, g, Some(&last), &mut out)
                {
                    let full = out.split().freeze();
                    self.last.insert(g.clone(), full.clone());
                    payload = Some(full);
                } else {
                    payload = Some(last);
                }
            }
            sent.insert(g.clone(), payload.unwrap_or_default());
        }
        for (i, g) in groups.iter().enumerate() {
            let seat = &mut self.seats[i];
            seat.frames.clear();
            let group_frame = sent.get(g).filter(|p| !p.is_empty()).cloned();
            if let Some(p) = &group_frame {
                seat.frames.push((false, p.clone()));
            }
            let mut out = BytesMut::new();
            if self
                .room
                .private(&mut self.world, seat.player, g, &[], &mut out)
            {
                seat.frames.push((true, out.freeze()));
            }
            if dropped.contains(&i) {
                self.room
                    .on_batch_dropped(&mut self.world, seat.player, group_frame.is_some());
                continue;
            }
            for (private, frame) in &seat.frames {
                if *private {
                    seat.view
                        .apply_private(frame)
                        .expect("a valid private frame");
                } else {
                    seat.view
                        .apply_snapshot(frame)
                        .expect("a valid group frame");
                }
            }
        }
    }

    fn step(&mut self) {
        self.step_with(false, &[]);
    }

    /// The wire ids seat `i`'s client holds.
    fn holds(&self, i: usize) -> BTreeSet<u64> {
        self.seats[i].view.ids().collect()
    }

    /// The wire ids of the given seats.
    fn wires(&self, seats: &[usize]) -> BTreeSet<u64> {
        seats.iter().map(|&i| self.seats[i].wire).collect()
    }
}

/// Every snapshot seat `i` got this tick, decoded: the group frame and
/// the private frame's one-shot full (`(private?, snapshot)`).
fn snapshots<R: GameLogic<World>>(sim: &Sim<R>, i: usize) -> Vec<(bool, WorldSnapshot)> {
    let mut out = Vec::new();
    for (private, frame) in &sim.seats[i].frames {
        if *private {
            if let Some(Payload::Snapshot(s)) = Private::decode(frame.as_ref())
                .expect("a private frame")
                .payload
            {
                out.push((true, s));
            }
        } else {
            out.push((
                false,
                WorldSnapshot::decode(frame.as_ref()).expect("a snapshot"),
            ));
        }
    }
    out
}

/// Every record id seat `i`'s frames carried this tick.
fn records_sent<R: GameLogic<World>>(sim: &Sim<R>, i: usize) -> BTreeSet<u64> {
    snapshots(sim, i)
        .iter()
        .flat_map(|(_, s)| s.entities.iter().map(|r| r.entity))
        .collect()
}

/// The lit room's key of AOI cell `(x, y)`'s shared group.
fn cell(x: i32, y: i32) -> LitGroup<Cell> {
    LitGroup::Cell(Cell(x, y))
}
