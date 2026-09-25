//! The MMO client's view: the kit's reference client (`gsb_kit::client`,
//! the client rules of gsb-kit's `kit.proto`) over the MMO's decode seam
//! — a FULL (group snapshot or one-shot private) replaces the view; a
//! DELTA with a baseline applies `removed`, then `cell_exits` (every held
//! record in that ground cell is forgotten), then the upserts; a delta
//! without a baseline is dropped; a stale sequence is discarded. (The
//! MMO's own test client runs the same kit view on in-process channels.)

use gsb_demo_mmo::codec::to_dm;
use gsb_demo_mmo::mmo::{self, CellExit, EntityRecord};
use gsb_demo_mmo::op;
use gsb_demo_mmo::world::client_cell;
use gsb_kit::client::{Apply, ClientDecoder, ClientError, ClientView, PrivateEvent};
use prost::Message;

use super::{Client, View};

/// The MMO's decode seam: a record is kept whole, in its ground cell
/// (`client_cell`); a `CellExit` names the ground cell `(x, z)`.
#[derive(Default)]
pub struct MmoDecoder;

impl ClientDecoder for MmoDecoder {
    type Record = EntityRecord;
    type Cell = (i32, i32);

    fn record(&self, body: &[u8]) -> Result<(u64, EntityRecord), ClientError> {
        let r = EntityRecord::decode(body)?;
        Ok((r.entity, r))
    }

    fn cell_of(&self, r: &EntityRecord) -> (i32, i32) {
        client_cell(r.x, r.z)
    }

    fn cell_exit(&self, body: &[u8]) -> Result<(i32, i32), ClientError> {
        let c = CellExit::decode(body)?;
        Ok((c.x, c.z))
    }
}

/// What an MMO client holds.
#[derive(Default)]
pub struct MmoView {
    /// The current view: wire id → record.
    pub records: ClientView<MmoDecoder>,
    /// Every input ack, in order.
    pub acks: Vec<u64>,
    /// Frames that changed the view (full or applied delta).
    pub applied: u64,
    /// An id the view must keep once it has seen it: every applied frame
    /// after which it is missing counts in [`Self::lost`].
    pub watch: Option<u64>,
    watched: bool,
    /// Applied frames after which the watched id was missing.
    pub lost: u64,
}

impl View for MmoView {
    fn apply(&mut self, code: u16, payload: &[u8]) {
        match code {
            op::MMO_SNAPSHOT => {
                let s = self.records.apply_snapshot(payload).expect("mmo snapshot");
                if let Apply::Full | Apply::Delta = s.apply {
                    self.book();
                }
            }
            op::MMO_PRIVATE => match self.records.apply_private(payload) {
                Ok(PrivateEvent::Ack(up_to)) => self.acks.push(up_to),
                Ok(PrivateEvent::Full { .. }) => self.book(),
                Ok(PrivateEvent::Empty) => {}
                Err(ClientError::PrivateDelta) => panic!("the one-shot private view is a full"),
                Err(e) => panic!("mmo private: {e}"),
            },
            other => panic!("unexpected MMO frame op {other}"),
        }
    }
}

impl MmoView {
    /// Book one applied frame (and the watched id's presence after it).
    fn book(&mut self) {
        self.applied += 1;
        if let Some(id) = self.watch {
            if self.records.contains(id) {
                self.watched = true;
            } else if self.watched {
                self.lost += 1;
            }
        }
    }

    /// Whether the view has had its first full.
    pub fn has_baseline(&self) -> bool {
        self.records.has_baseline()
    }

    /// The wire ids of the players in view, sorted.
    pub fn players(&self) -> Vec<u64> {
        self.of_kind(mmo::Kind::Player)
    }

    /// The wire ids of `kind` in view, sorted.
    pub fn of_kind(&self, kind: mmo::Kind) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .records
            .values()
            .filter(|r| r.kind == kind as i32)
            .map(|r| r.entity)
            .collect();
        ids.sort_unstable();
        ids
    }
}

impl Client<MmoView> {
    /// This client's own record, if in view.
    pub fn me(&self) -> Option<EntityRecord> {
        self.view.records.get(self.entity).copied()
    }

    /// The record of `id`, if in view.
    pub fn sees(&self, id: u64) -> Option<EntityRecord> {
        self.view.records.get(id).copied()
    }

    /// Walk toward `(x, z)` metres.
    pub async fn move_to(&mut self, x: f32, z: f32, seq: u64) {
        let m = mmo::MoveTo {
            x: to_dm(x),
            z: to_dm(z),
            seq,
        };
        self.send(op::MMO_MOVE_TO, &m.encode_to_vec()).await;
    }

    /// Hit the mob with wire id `target`.
    pub async fn attack(&mut self, target: u64, seq: u64) {
        let m = mmo::Attack { target, seq };
        self.send(op::MMO_ATTACK, &m.encode_to_vec()).await;
    }

    /// Use waystone `waystone`.
    pub async fn travel(&mut self, waystone: u32, seq: u64) {
        let m = mmo::Travel { waystone, seq };
        self.send(op::MMO_TRAVEL, &m.encode_to_vec()).await;
    }
}

/// `(x, z)` metres as the wire's decimetres (what a record carries).
pub fn dm(x: f32, z: f32) -> (i32, i32) {
    (to_dm(x), to_dm(z))
}

/// A record's ground position in decimetres.
pub fn ground(r: &EntityRecord) -> (i32, i32) {
    (r.x, r.z)
}
