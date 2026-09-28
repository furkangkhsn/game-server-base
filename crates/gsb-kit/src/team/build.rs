//! The team room's construction and configuration: the constructor, the
//! snapshot-mode and disconnect-policy builders, the game accessors (a
//! child of the team module).

use std::collections::HashMap;

use crate::common::{Baselines, Cached, InputSeq, ParkPolicy};
use crate::game::TeamGame;
use crate::identity::Minter;
use crate::space::Vision;
use crate::team::TeamRoom;

impl<G: TeamGame, V: Vision> TeamRoom<G, V> {
    /// Build a team-fog room running `game` with the vision model
    /// `vision`.
    #[must_use]
    pub fn with_game(game: G, vision: V) -> Self {
        Self {
            game,
            vision,
            player_entity: HashMap::new(),
            next_player_id: 0,
            park: ParkPolicy::default(),
            park_ledger: HashMap::new(),
            minter: Minter::sequential(),
            ledgers: Vec::new(),
            delta: false,
            baselines: Baselines::default(),
            step: 0,
            tick: 0,
            team_units: Vec::new(),
            neutral: Vec::new(),
            cells: HashMap::new(),
            contents: Vec::new(),
            input: InputSeq::default(),
            encoded: 0,
            sighted: Cached::default(),
            orphans: Cached::default(),
        }
    }

    /// Ship DELTA snapshots (the AOI room's envelope and client rules —
    /// `kit.proto`): per team and tick, the records that left the team's
    /// view (`removed`) and those that entered it or whose wire value
    /// changed (upserts), encoded once per team; a FULL for a fresh team
    /// and on the keep-alive cadence; a one-shot private full to a
    /// member without a baseline. Without it the room ships full frames
    /// only — byte for byte what it always did.
    #[must_use]
    pub fn with_delta(mut self) -> Self {
        self.delta = true;
        self
    }

    /// Set the disconnect-park grace (see
    /// [`crate::room::OpenRoom::with_disconnect_grace`]; RECONNECT §3).
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: std::time::Duration) -> Self {
        self.park.grace = Some(grace);
        self
    }

    /// Set the whole disconnect-park policy (see
    /// [`crate::room::OpenRoom::with_disconnect_policy`]; RECONNECT
    /// §3/§14.4).
    #[must_use]
    pub fn with_disconnect_policy(
        mut self,
        grace: Option<std::time::Duration>,
        to: gsb_core::room::ExpireTo,
    ) -> Self {
        self.park.grace = grace;
        self.park.to = to;
        self
    }

    /// Override the disconnect policy for one cause (see
    /// [`crate::room::OpenRoom::with_disconnect_policy_for`]; BACKLOG
    /// F27).
    #[must_use]
    pub fn with_disconnect_policy_for(
        mut self,
        cause: gsb_core::room::DisconnectCause,
        grace: Option<std::time::Duration>,
        to: gsb_core::room::ExpireTo,
    ) -> Self {
        self.park.set_for(cause, grace, to);
        self
    }

    /// The game this room runs.
    pub fn game(&self) -> &G {
        &self.game
    }

    /// The game this room runs, for configuration after construction.
    pub fn game_mut(&mut self) -> &mut G {
        &mut self.game
    }
}
