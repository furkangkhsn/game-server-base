//! The events of the tests (what `gsb-build` would generate) and the
//! stand-in room / shard logic that reports every event it takes.

use gsb_core::calls::{BootError, Calls};
use gsb_core::events::{ClientEvent, Dest, Event, Events, WorldEvent};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, Admission, Detach, ExpireTo, GameLogic, RoomLogic, TickCtx};
use gsb_core::shard::{BorderRecord, CrossSeam, Migrating, ShardLogic};
use std::collections::HashMap;
use tokio::sync::mpsc;

/// Wire ids (and players) are `shard * SPAN + serial`.
pub const SPAN: u64 = 1_000;
pub const SHARDS: usize = 3;

#[derive(Clone, PartialEq, prost::Message)]
pub struct GoldReceived {
    #[prost(uint64, tag = "1")]
    pub balance: u64,
}
impl Event for GoldReceived {
    const OP: u16 = 2001;
    const TO: Dest = Dest::World;
}
impl WorldEvent for GoldReceived {}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ChatLine {
    #[prost(string, tag = "1")]
    pub text: String,
}
impl Event for ChatLine {
    const OP: u16 = 2002;
    const TO: Dest = Dest::Client;
}
impl ClientEvent for ChatLine {}

#[derive(Clone, PartialEq, prost::Message)]
pub struct BalanceChanged {
    #[prost(uint64, tag = "1")]
    pub balance: u64,
}
impl Event for BalanceChanged {
    const OP: u16 = 2005;
    const TO: Dest = Dest::Both;
}
impl WorldEvent for BalanceChanged {}
impl ClientEvent for BalanceChanged {}

/// Moves the addressed player to shard `to` in this tick's MIGRATE (a
/// sharded room's game deciding a crossing, `docs/EVENTS.md` §7.3).
#[derive(Clone, PartialEq, prost::Message)]
pub struct Hop {
    #[prost(uint32, tag = "1")]
    pub to: u32,
}
impl Event for Hop {
    const OP: u16 = 2009;
    const TO: Dest = Dest::World;
}
impl WorldEvent for Hop {}

/// The room's game panics on it (a room death, §7.4).
#[derive(Clone, PartialEq, prost::Message)]
pub struct Boom {
    #[prost(uint32, tag = "1")]
    pub n: u32,
}
impl Event for Boom {
    const OP: u16 = 2010;
    const TO: Dest = Dest::World;
}
impl WorldEvent for Boom {}

/// One event a room or shard took: `(shard, op, addressee identity —
/// None for room-wide, owner, publisher, seq)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Took {
    pub shard: usize,
    pub op: u16,
    pub to: Option<String>,
    pub owner: u64,
    pub from: u32,
    pub seq: u64,
}

pub type Seen = mpsc::UnboundedSender<Took>;

/// A room (index 0) or one shard of [`SHARDS`]; parks a disconnected
/// player when `park`.
pub struct EvLogic {
    pub index: usize,
    pub serial: u64,
    pub park: bool,
    pub seen: Seen,
    /// A shard's neighbours (every other shard); empty for a room.
    pub neighbors: Vec<usize>,
    /// Players a `Hop` sends away: player → target shard.
    pub hops: HashMap<PlayerId, usize>,
}

impl EvLogic {
    /// A room (`index` = its id) or one shard of the grid.
    pub fn new(index: usize, park: bool, seen: Seen, sharded: bool) -> Self {
        Self {
            index,
            serial: 0,
            park,
            seen,
            neighbors: if sharded {
                (0..SHARDS).filter(|&j| j != index).collect()
            } else {
                Vec::new()
            },
            hops: HashMap::new(),
        }
    }

    fn report(&mut self, events: &mut Events) {
        for any in events.take_all() {
            match any.op() {
                Boom::OP => panic!("Boom: the room dies (test)"),
                Hop::OP => {
                    let player = PlayerId(any.owner().0);
                    if let Ok(hop) = any.downcast::<Hop>() {
                        self.hops.insert(player, hop.body.to as usize);
                        let _ = self.seen.send(Took {
                            shard: self.index,
                            op: Hop::OP,
                            to: Some("hop".into()),
                            owner: player.0,
                            from: hop.from.0,
                            seq: hop.seq,
                        });
                    }
                    continue;
                }
                _ => {}
            }
            let to = match any.to() {
                gsb_core::events::Addressee::Player(id) => Some(id.as_str().to_owned()),
                gsb_core::events::Addressee::Room => None,
            };
            let _ = self.seen.send(Took {
                shard: self.index,
                op: any.op(),
                to,
                owner: any.owner().0,
                from: any.from().0,
                seq: any.seq(),
            });
        }
    }
}

impl GameLogic<()> for EvLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7F30
    }
    fn private_op(&self) -> u16 {
        0x7F31
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _b: &[BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), _c: ConnectionId) -> Admission {
        self.serial += 1;
        let wire = self.index as u64 * SPAN + self.serial;
        Admission {
            player: PlayerId(wire),
            entity: wire,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn on_disconnect(&mut self, _w: &mut (), _p: PlayerId, _identity: &str) -> Detach {
        if self.park {
            Detach::Hold {
                grace: Some(std::time::Duration::from_secs(60)),
                to: ExpireTo::Despawn,
            }
        } else {
            Detach::Despawn
        }
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
    fn boot_events(&mut self, events: &mut Events) -> Result<(), BootError> {
        events.accept::<GoldReceived>()?;
        events.accept::<BalanceChanged>()?;
        events.accept::<Hop>()?;
        events.accept::<Boom>()
    }
    fn update_with_events(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _calls: &mut Calls,
        events: &mut Events,
    ) {
        self.report(events);
    }
}

impl RoomLogic<()> for EvLogic {}

impl ShardLogic<()> for EvLogic {
    type State = ();
    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        SHARDS
    }
    fn serial_capacity(&self) -> u64 {
        SPAN
    }
    fn serial_used(&self) -> u64 {
        self.serial
    }
    fn neighbors(&self) -> &[usize] {
        &self.neighbors
    }
    /// Wire ids are the players' (`on_join`).
    fn collect_migrations(&mut self, _w: &mut (), nb: usize) -> Vec<Migrating<()>> {
        let go: Vec<PlayerId> = self
            .hops
            .iter()
            .filter(|(_, to)| **to == nb)
            .map(|(p, _)| *p)
            .collect();
        go.into_iter()
            .map(|p| {
                self.hops.remove(&p);
                Migrating {
                    wire: p.0,
                    state: (),
                    player: Some(p),
                }
            })
            .collect()
    }
    fn on_migrate_in(&mut self, _w: &mut (), _wire: u64, _s: (), _p: Option<PlayerId>) {}
    fn on_migrate_out(&mut self, _w: &mut (), _wire: u64) {}
    fn collect_border(&self, _w: &()) -> Vec<BorderRecord<()>> {
        Vec::new()
    }
    fn own_wires(&self, _w: &()) -> Vec<u64> {
        Vec::new()
    }
    fn update_seam_with_events(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _seam: &mut CrossSeam<'_, ()>,
        _calls: &mut Calls,
        events: &mut Events,
    ) {
        self.report(events);
    }
}
