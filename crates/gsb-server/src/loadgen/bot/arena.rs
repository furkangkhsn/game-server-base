//! The 3D arena's bot (GAME-MODULE §6 decision 8): every unit runs from
//! its team's base to the arena's centre and back, climbing and dropping
//! as it goes — so the three teams' units genuinely meet (and leave)
//! each other's fog, in 3D: height alone can hide a unit.
//!
//! **Home = the team's base, as the wire names it.** The session's first
//! private frame carries the arena's `Welcome` (team, team count — the
//! kit's session payload, `Private.game`); the bot places the base with
//! the arena's own formula (`ArenaGame::base_of`). Until the welcome has
//! arrived the bot sends nothing. (Before the welcome existed the bot
//! read its home off the first snapshot that showed its own unit — the
//! wire did not name the team: GAME-MODULE G3-3.)
//!
//! **The run** (per client, phase-shifted by id like the demo's ring so
//! the units do not move in lockstep): along the straight line home →
//! centre, `s = (1 − cos φ) / 2` of the way (0 = home, 1 = centre),
//! `φ = (t + 0.618·id) · 2π / ROUND_TRIP`; height `10 · (1 − cos(φ +
//! 1.3·id))` m, 0..20 m under the 30 m ceiling. An 8 s round trip over
//! the 25 m base ring keeps the target below the units' 12 m/s, so a
//! unit tracks it; a 20 m height swing exceeds the 15 m vision radius,
//! so two units over the same floor spot see each other only part of
//! the time.

use std::f64::consts::TAU;
use std::time::Duration;

use gsb_demo_arena::ArenaGame;
use gsb_demo_arena::arena::{MoveTo, Welcome};
use gsb_demo_arena::codec::{Cm3, to_cm};
use gsb_demo_arena::op;
use gsb_kit::client::wire::{Fields, Value, sint32};
use gsb_kit::client::{ClientDecoder, ClientError, ClientView, Counters, PrivateEvent, Snapshot};
use prost::Message;

use super::{BotClient, Labels, LoadBot};

#[cfg(test)]
mod tests;

/// One base → centre → base run, seconds.
const ROUND_TRIP_SECS: f64 = 8.0;
/// Half the height swing (metres): targets span `0..2·HEIGHT_SWING`.
const HEIGHT_SWING: f64 = 10.0;

/// The arena's bot family (stateless: every knob is the game's).
pub(crate) struct ArenaBot;

impl LoadBot for ArenaBot {
    fn snapshot_op(&self) -> u16 {
        op::ARENA_SNAPSHOT
    }

    fn private_op(&self) -> u16 {
        op::ARENA_PRIVATE
    }

    fn client(&self, id: u64) -> Box<dyn BotClient> {
        Box::new(ArenaClient {
            id,
            view: ClientView::new(ArenaDecoder::default()),
        })
    }

    fn flood_input(&self) -> (u16, Vec<u8>) {
        let msg = MoveTo {
            x: 0,
            y: 0,
            z: 0,
            seq: 0,
        };
        (op::ARENA_MOVE_TO, msg.encode_to_vec())
    }

    fn churn_input(&self, id: u64, seq: u64) -> (u16, Vec<u8>) {
        // A per-id spot around the centre (the churn client keeps no
        // view, so it cannot know its home).
        let msg = MoveTo {
            x: to_cm((id % 20) as f32 - 10.0),
            y: to_cm((id % 3) as f32 * 5.0),
            z: to_cm((id % 17) as f32 - 8.0),
            seq,
        };
        (op::ARENA_MOVE_TO, msg.encode_to_vec())
    }

    fn labels(&self) -> Option<Labels> {
        Some(Labels {
            visibility: "team",
            shards: 1,
            profile: "base-centre",
        })
    }

    fn describe(&self) -> String {
        format!(
            "arena: units run team base → centre → base every {ROUND_TRIP_SECS} s, \
             height 0..{} m; one numbered MoveTo per --move-ms once welcomed (team from the wire)",
            2.0 * HEIGHT_SWING
        )
    }
}

/// One arena client.
struct ArenaClient {
    id: u64,
    /// The view; its decoder keeps the session's welcome (the home).
    view: ClientView<ArenaDecoder>,
}

impl ArenaClient {
    /// The target at `elapsed`, centimetres.
    fn target(&self, home: [i32; 3], elapsed: Duration) -> [i32; 3] {
        let id = self.id as f64;
        let phi = (elapsed.as_secs_f64() + id * 0.618) * TAU / ROUND_TRIP_SECS;
        let stay = (1.0 + phi.cos()) / 2.0; // 1 = home, 0 = centre
        let y = HEIGHT_SWING * (1.0 - (phi + id * 1.3).cos());
        [
            (f64::from(home[0]) * stay).round() as i32,
            (y * 100.0).round() as i32,
            (f64::from(home[2]) * stay).round() as i32,
        ]
    }
}

impl BotClient for ArenaClient {
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
        let [x, y, z] = self.target(self.view.decoder().home?, elapsed);
        let msg = MoveTo { x, y, z, seq };
        Some((op::ARENA_MOVE_TO, msg.encode_to_vec()))
    }
}

/// The arena's decode seam: a record is the unit's position in
/// centimetres (`arena.proto`'s `UnitRecord { uint64 entity = 1; sint32
/// x = 2; sint32 y = 3; sint32 z = 4; }`, walked by hand — pinned to the
/// generated decoder by this module's tests). The arena runs no cell
/// space: its team room sends full snapshots only and no frame carries a
/// cell exit, so one is a protocol error. The session payload is the
/// arena's `Welcome` (decoded with the generated type: once per session);
/// the decoder keeps the base it names.
#[derive(Default)]
pub(crate) struct ArenaDecoder {
    /// The own team's base, centimetres (`None` until welcomed).
    home: Option<[i32; 3]>,
}

impl ClientDecoder for ArenaDecoder {
    type Record = [i32; 3];
    type Cell = ();

    #[inline]
    fn record(&self, body: &[u8]) -> Result<(u64, [i32; 3]), ClientError> {
        let (mut entity, mut at) = (0, [0; 3]);
        for field in Fields::new(body) {
            match field? {
                (1, Value::Varint(v)) => entity = v,
                (n @ 2..=4, Value::Varint(v)) => at[n as usize - 2] = sint32(v),
                (1..=4, _) => return Err(ClientError::Malformed("wrong wire type")),
                _ => {}
            }
        }
        Ok((entity, at))
    }

    #[inline]
    fn cell_of(&self, _: &[i32; 3]) {}

    fn cell_exit(&self, _: &[u8]) -> Result<(), ClientError> {
        Err(ClientError::Malformed("the arena sends no cell exits"))
    }

    fn session_private(&mut self, body: &[u8]) -> Result<(), ClientError> {
        let Welcome { team, teams } = Welcome::decode(body)?;
        let (Ok(team), Ok(teams)) = (u8::try_from(team), u8::try_from(teams)) else {
            return Err(ClientError::Malformed("a team out of range"));
        };
        if team >= teams {
            return Err(ClientError::Malformed("a team out of range"));
        }
        let Cm3 { x, y, z } = Cm3::from(ArenaGame::with_teams(teams).base_of(team));
        self.home = Some([x, y, z]);
        Ok(())
    }
}
