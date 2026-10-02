//! The room side: each member's latest path ([`PathTable`]) and the view
//! a tick context lends the game ([`PathView`]). A CHILD of [`super`].

use std::collections::HashMap;
use std::time::Duration;

use crate::id::PlayerId;
use crate::path::PathState;

/// Each member's latest path state, as its connection actor delivered
/// it. The room and the shard actor own one (set by READ, emptied for a
/// member wherever its session ends or is parked — leave, detach,
/// despawn, the input-idle ceiling — and handed over with a member that
/// crosses to another shard); a test that builds its own tick context
/// owns one too and lends it with [`Self::view`].
///
/// Only members whose transport measures anything are in it: the table
/// of a room whose members are all on TCP stays empty, and an empty
/// table costs the tick nothing.
#[derive(Debug, Default)]
pub struct PathTable {
    paths: HashMap<PlayerId, PathState>,
}

impl PathTable {
    /// `player`'s path is now `state`.
    pub fn set(&mut self, player: PlayerId, state: PathState) {
        self.paths.insert(player, state);
    }

    /// `player`'s session ended or changed: its path is unknown again.
    /// Returns what was known (a shard hands it over with the member).
    pub fn remove(&mut self, player: PlayerId) -> Option<PathState> {
        self.paths.remove(&player)
    }

    /// `player`'s latest path.
    pub fn get(&self, player: PlayerId) -> Option<PathState> {
        self.paths.get(&player).copied()
    }

    /// How many members have a path.
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// No member has a path.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The view a tick context carries: this table, with the budget
    /// measured per `period` (the room's tick period).
    pub fn view(&self, period: Duration) -> PathView<'_> {
        PathView {
            inner: Some((self, period)),
        }
    }
}

/// The members' paths as the game reads them through its tick context
/// ([`crate::room::TickCtx::path`], [`crate::room::TickCtx::budget`]).
/// A hand-built context defaults to the EMPTY view: every member's path
/// is unknown.
#[derive(Debug, Clone, Copy, Default)]
pub struct PathView<'a> {
    inner: Option<(&'a PathTable, Duration)>,
}

impl PathView<'_> {
    /// `player`'s latest path; `None` — not a member here, or its
    /// transport measures nothing (TCP, TLS, WebSocket today).
    pub fn path(&self, player: PlayerId) -> Option<PathState> {
        let (table, _) = self.inner?;
        table.get(player)
    }

    /// The bytes `player`'s path takes per tick while its transport
    /// limits it ([`PathState::budget`] over the tick period); `None` —
    /// not limited or unknown: send as always.
    pub fn budget(&self, player: PlayerId) -> Option<usize> {
        let (table, period) = self.inner?;
        if table.is_empty() {
            return None;
        }
        table.get(player)?.budget(period)
    }

    /// The tick period budgets are measured over (`None` for the empty
    /// view).
    pub fn period(&self) -> Option<Duration> {
        self.inner.map(|(_, p)| p)
    }
}
