//! The war client's view: the kit's reference client
//! (`gsb_kit::client::ClientView`, the client rules of gsb-kit's
//! `kit.proto`) over the war's decode seam (the war's records ride the
//! kit's record run) — a faction full replaces the
//! view, a delta with a baseline applies `removed` then the upserts, a
//! one-shot private full replaces it; the session's `Welcome` (the kit's
//! `Private.game`) is kept by the decoder.

use gsb_demo_war::codec::{read_record, to_dm};
use gsb_demo_war::op;
use gsb_demo_war::war::{self, Kind, UnitRecord, Welcome};
use gsb_demo_war::world::VISION_RADIUS;
use gsb_kit::client::{ClientDecoder, ClientError, ClientView, PrivateEvent};
use prost::Message;

use super::{Client, View};

/// The war's decode seam: the war rides the kit's record run — a record
/// is read off the run and kept whole; no cell exits on a team frame;
/// the welcome kept.
#[derive(Default)]
pub struct WarDecoder {
    pub welcomes: Vec<Welcome>,
}

impl ClientDecoder for WarDecoder {
    type Record = UnitRecord;
    type Cell = ();

    const RUN: bool = true;

    fn record(&self, _body: &[u8]) -> Result<(u64, UnitRecord), ClientError> {
        Err(ClientError::Malformed(
            "the war's records ride the record run",
        ))
    }

    fn run_record(&self, id: u64, run: &mut &[u8]) -> Result<UnitRecord, ClientError> {
        read_record(id, run)
    }

    fn cell_of(&self, _: &UnitRecord) {}

    fn cell_exit(&self, _: &[u8]) -> Result<(), ClientError> {
        Err(ClientError::Malformed("a team frame carries no cell exit"))
    }

    fn session_private(&mut self, body: &[u8]) -> Result<(), ClientError> {
        self.welcomes.push(Welcome::decode(body)?);
        Ok(())
    }
}

/// What a war client holds.
#[derive(Default)]
pub struct WarView {
    pub units: ClientView<WarDecoder>,
    /// Every input ack, in order.
    pub acks: Vec<u64>,
}

impl View for WarView {
    fn apply(&mut self, code: u16, payload: &[u8]) {
        match code {
            op::WAR_SNAPSHOT => {
                self.units.apply_snapshot(payload).expect("war snapshot");
            }
            op::WAR_PRIVATE => match self.units.apply_private(payload) {
                Ok(PrivateEvent::Ack(up_to)) => self.acks.push(up_to),
                Ok(_) => {}
                Err(e) => panic!("war private: {e}"),
            },
            other => panic!("unexpected war frame op {other}"),
        }
    }
}

impl WarView {
    /// The session's faction (1-based), once welcomed.
    pub fn faction(&self) -> Option<u32> {
        self.units.decoder().welcomes.last().map(|w| w.faction)
    }

    /// The wire ids of `kind` in view, sorted.
    pub fn of_kind(&self, kind: Kind) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .units
            .iter()
            .filter(|(_, r)| r.kind == kind as i32)
            .map(|(id, _)| id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// The team fog from the network side: every unit in view is the
    /// client's faction's (seen map-wide), an unclaimed capture point, or
    /// an enemy within sight of one of the faction's units in the same
    /// view. `slack` metres cover records a tick apart.
    pub fn fog_holds(&self, slack: f32) -> Result<(), String> {
        let Some(me) = self.faction() else {
            return Ok(()); // not welcomed yet
        };
        let eyes: Vec<&UnitRecord> = self.units.values().filter(|r| r.faction == me).collect();
        let reach = to_dm(VISION_RADIUS + slack) as f32;
        for r in self.units.values() {
            if r.faction == me || (r.faction == 0 && r.kind == Kind::Point as i32) {
                continue;
            }
            let seen = eyes.iter().any(|e| {
                let (dx, dz) = ((e.x - r.x) as f32, (e.z - r.z) as f32);
                (dx * dx + dz * dz).sqrt() <= reach
            });
            if !seen {
                return Err(format!(
                    "enemy {} un-spotted in faction {me}'s view",
                    r.entity
                ));
            }
        }
        Ok(())
    }
}

impl Client<WarView> {
    /// This client's own record, if in view.
    pub fn me(&self) -> Option<UnitRecord> {
        self.view.units.get(self.entity).copied()
    }

    /// Whether the view holds `id`.
    pub fn sees(&self, id: u64) -> bool {
        self.view.units.contains(id)
    }

    /// Walk toward `(x, z)` metres.
    pub async fn move_to(&mut self, x: f32, z: f32, seq: u64) {
        let m = war::MoveTo {
            x: to_dm(x),
            z: to_dm(z),
            seq,
        };
        self.send(op::WAR_MOVE_TO, &m.encode_to_vec()).await;
    }
}
