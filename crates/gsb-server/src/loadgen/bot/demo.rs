//! The 2D demo's bot — the load generator's historical client, moved
//! here VERBATIM (GAME-MODULE §4.4): the three movement profiles
//! (`ring`, `spread`, `still` with `--still-frac`), the `as i32`
//! truncation of the target, and the demo's decode seam. Its inputs are
//! byte-identical to the pre-G3 generator's.

use std::time::Duration;

use gsb_demo::game::{CellExit, MoveTo};
use gsb_kit::client::wire::{Fields, Value, sint32};
use gsb_kit::client::{ClientDecoder, ClientError, ClientView, Counters, PrivateEvent, Snapshot};
use prost::Message;

use super::{BotClient, LoadBot};
use crate::Profile;

#[cfg(test)]
mod tests;

/// The demo's bot family: the command line's profile knobs.
pub(crate) struct DemoBot {
    pub(crate) profile: Profile,
    pub(crate) still_frac: f64,
    pub(crate) spawn_half: f32,
    /// The AOI cell size (for the client view's `CellExit` handling;
    /// ignored by the non-spatial strategies, whose snapshots are full).
    pub(crate) cell_size: f32,
}

impl LoadBot for DemoBot {
    fn snapshot_op(&self) -> u16 {
        gsb_demo::op::WORLD_SNAPSHOT
    }

    fn private_op(&self) -> u16 {
        gsb_demo::op::PRIVATE
    }

    fn client(&self, id: u64) -> Box<dyn BotClient> {
        Box::new(DemoClient {
            id,
            profile: self.profile,
            spawn_half: self.spawn_half,
            // The still profile's deterministic per-id split (the same
            // id-based determinism as the stagger, 1% granularity — the
            // id's hundredths digit decides, so any client count gets the
            // ratio): the first `still_frac` fraction of the ids is
            // "still" — it issues ONE MOVE_TO (settling at a ring target)
            // and then sends nothing more; the moving minority chases the
            // ring target as in the historical profile.
            is_still: self.profile == Profile::Still && (id % 100) as f64 / 100.0 < self.still_frac,
            settled: false,
            view: ClientView::new(DemoDecoder {
                cell_size: self.cell_size,
            }),
        })
    }

    fn flood_input(&self) -> (u16, Vec<u8>) {
        let msg = MoveTo { x: 0, y: 0, seq: 0 };
        (gsb_demo::op::MOVE_TO, msg.encode_to_vec())
    }

    fn churn_input(&self, id: u64, seq: u64) -> (u16, Vec<u8>) {
        let msg = MoveTo {
            x: ((id as i64 % 80) - 40) as i32,
            y: ((id as i64 % 37) - 18) as i32,
            seq,
        };
        (gsb_demo::op::MOVE_TO, msg.encode_to_vec())
    }

    fn describe(&self) -> String {
        "demo: MOVE_TO toward the profile's target every --move-ms".into()
    }
}

/// The `spread` profile's deterministic home for client `id`: the SAME
/// lattice the server's `gsb_demo::room::spawn_pos` uses (same hash, same
/// scaling), so spawn points and homes live on the same map. The
/// *distribution* is what the profile contributes (uniform over the map,
/// statistically steady from tick 1 — see `Profile::Spread`).
pub(crate) fn spawn_home(id: u64, half: f32) -> (f64, f64) {
    let h = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let scale = half as f64 / 50.0;
    let x = ((h % 1000) as f64 / 10.0 - 50.0) * scale;
    let y = (((h >> 32) % 1000) as f64 / 10.0 - 50.0) * scale;
    (x, y)
}

/// One demo client.
struct DemoClient {
    id: u64,
    profile: Profile,
    spawn_half: f32,
    is_still: bool,
    settled: bool,
    view: ClientView<DemoDecoder>,
}

impl BotClient for DemoClient {
    fn apply_snapshot(&mut self, frame: &[u8]) -> Result<Snapshot, ClientError> {
        self.view.apply_snapshot(frame)
    }

    fn apply_private(&mut self, frame: &[u8]) -> Result<PrivateEvent, ClientError> {
        self.view.apply_private(frame)
    }

    fn counters(&self) -> Counters {
        *self.view.counters()
    }

    fn view_len(&self) -> usize {
        self.view.len()
    }

    fn next_input(&mut self, elapsed: Duration, seq: u64) -> Option<(u16, Vec<u8>)> {
        // A still client settles once: the first interval sends its only
        // command, the rest of the run it is silent (that IS the profile
        // — the majority of the world stands still).
        if self.is_still && self.settled {
            return None;
        }
        self.settled = true;
        let id = self.id;
        let (tx, ty) = match self.profile {
            // The historical profile (UNCHANGED — all previous
            // measurements stay comparable): a circle of radius 40 around
            // the map center at 4 rad/s, phase-shifted per client (id-based
            // offset so N entities do not move in lockstep). The targets
            // outrun the entities, so the entities crowd the central band —
            // the *clustered* layout.
            Profile::Ring => {
                let angle = (elapsed.as_secs_f64() + id as f64 * 0.618) * 4.0;
                (angle.cos() * 40.0, angle.sin() * 40.0)
            }
            // The *spread* profile: each client wanders a small circle
            // (radius 20, 0.4 rad/s — the target stays reachable at 10 u/s,
            // so the entity tracks it closely) around its deterministic
            // home, uniform over the ±spawn_half map. The layout stays
            // ~uniform over the whole run (the server spawns with the same
            // distribution), so there is no migration artifact and the run
            // is statistically steady from tick 1. This is the sparse,
            // wide-map layout a real MOBA arena resembles — where team fog
            // actually hides most enemies (the clustered profile hides
            // none).
            Profile::Spread => {
                let (hx, hy) = spawn_home(id, self.spawn_half);
                let w = (elapsed.as_secs_f64() + id as f64 * 0.618) * 0.4;
                (hx + w.cos() * 20.0, hy + w.sin() * 20.0)
            }
            // The still profile's moving minority chases the ring target
            // (the historical profile's shape).
            Profile::Still => {
                let angle = (elapsed.as_secs_f64() + id as f64 * 0.618) * 4.0;
                (angle.cos() * 40.0, angle.sin() * 40.0)
            }
        };
        let msg = MoveTo {
            x: tx as i32,
            y: ty as i32,
            seq,
        };
        Some((gsb_demo::op::MOVE_TO, msg.encode_to_vec()))
    }
}

/// The demo's decode seam. A record's cell uses the server's own
/// formula (floor of the WIRE coordinates / cell_size, the kit's
/// `Grid2`), so a `CellExit` forgets exactly the entities the server
/// considers to be in that cell; a `CellExit` carries the cell's INDEX.
///
/// The record — every entity of every frame, the receive loop's hot
/// path — is walked by hand (`game.proto`'s `EntityRecord { uint64
/// entity = 1; sint32 x = 2; sint32 y = 3; }`; pinned to the generated
/// decoder by this module's tests); the rare cell exit uses the
/// generated `CellExit`.
pub(crate) struct DemoDecoder {
    pub(crate) cell_size: f32,
}

impl ClientDecoder for DemoDecoder {
    type Record = (i32, i32);
    type Cell = (i32, i32);

    #[inline]
    fn record(&self, body: &[u8]) -> Result<(u64, (i32, i32)), ClientError> {
        let (mut entity, mut x, mut y) = (0, 0, 0);
        for field in Fields::new(body) {
            match field? {
                (1, Value::Varint(v)) => entity = v,
                (2, Value::Varint(v)) => x = sint32(v),
                (3, Value::Varint(v)) => y = sint32(v),
                (1..=3, _) => return Err(ClientError::Malformed("wrong wire type")),
                _ => {}
            }
        }
        Ok((entity, (x, y)))
    }

    #[inline]
    fn cell_of(&self, &(x, y): &(i32, i32)) -> (i32, i32) {
        (
            (x as f32 / self.cell_size).floor() as i32,
            (y as f32 / self.cell_size).floor() as i32,
        )
    }

    #[inline]
    fn cell_exit(&self, body: &[u8]) -> Result<(i32, i32), ClientError> {
        let c = CellExit::decode(body)?;
        Ok((c.x, c.y))
    }
}
