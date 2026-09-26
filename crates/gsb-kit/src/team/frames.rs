//! The team room's frames: the group snapshot, the keep-alive and the
//! per-connection private frame, in the room's two snapshot modes (a
//! child of the team module: `logic` delegates the three hooks here).
//!
//! ## Full mode (the default)
//!
//! Every tick a team's content changed, its whole content as a FULL
//! frame; the keep-alive re-sends the cached frame (the core's default);
//! the private frame is the ack / RPC answers / session payload. Byte for
//! byte what the room always shipped (pinned by `tests/delta/full_only`).
//!
//! ## Delta mode ([`TeamRoom::with_delta`])
//!
//! The AOI room's envelope and client rules (`kit.proto`), so every
//! client that speaks them (`gsb_kit::client::ClientView`, the loadgen
//! bots) needs no change:
//!
//! - **The delta is per team, against what its clients hold.** The team's
//!   content (own units ∪ neutrals ∪ enemies in vision — rebuilt in
//!   `update`, unchanged) is diffed against the content of the team's
//!   last emitted frame (the shared set ledger, `crate::common::
//!   SetLedger`): `removed` = the wire ids that left the team's view,
//!   `entities` = upserts of the records that entered it or whose WIRE
//!   value changed (the codec's quantization decides: a unit that moved
//!   less than one wire unit is not re-sent). Computed and encoded once
//!   per team per tick and shared by the team's members.
//! - **No `cell_exits`.** A cell exit makes a client forget every record
//!   it holds in a cell it can compute from the record itself — sound in
//!   the AOI room because its view IS a union of whole cells. A vision
//!   set is not: whether an enemy is in a team's view is an exact
//!   distance test against the team's units, so the enemies in one grid
//!   cell leave a team's view one by one, and the client cannot
//!   recompute membership (it would need positions it never received).
//!   The cell-granular exit would also save little: vision cells are
//!   radius-sized, so a cell rarely holds more than a handful of enemies
//!   leaving at once — `removed` costs 2–3 bytes per id.
//! - **Fulls.** A fresh team group (not asked for on the previous step —
//!   the core asks every group with members every step) gets a FULL,
//!   which baselines all its members. On the keep-alive cadence every
//!   team ships a fresh FULL, active or silent (the convergence
//!   guarantee: a client that lost any number of deltas holds the exact
//!   view again within one keep-alive period). A member whose session
//!   has no baseline for its team's view — a join into an established
//!   team, a resume, a runtime team change — gets a one-shot FULL in its
//!   private frame (the AOI room's rule; the group's delta precedes it
//!   in the same batch and is dropped by that client as a gap — the
//!   core's batch order, GAME-MODULE G3-2), unless the team's own frame
//!   this step was already a full. The one full frame per team per step
//!   is encoded once and shared by all three uses.
//! - **Bounded state.** Per team: the last emitted content (the view,
//!   each record's wire value as its fingerprint) and this step's full;
//!   per player: the team its session is baselined on.

use bevy_ecs::prelude::World;
use bytes::BytesMut;
use gsb_core::id::PlayerId;
use gsb_core::rpc::RpcReply;

use crate::common::Emitted;
use crate::game::TeamGame;
use crate::space::Vision;
use crate::team::*;

impl<G: TeamGame, V: Vision> TeamRoom<G, V> {
    /// `GameLogic::snapshot`: `team`'s frame this tick (module docs).
    pub(super) fn emit_snapshot(&mut self, tick: u64, team: Team, out: &mut BytesMut) -> bool {
        let t = self.team_slot(team);
        let (codec, content, ledger) = (self.game.codec(), &self.contents[t], &mut self.ledgers[t]);
        if !self.delta {
            return ledger.emit_full(codec, tick, content, out, &mut self.encoded);
        }
        let emitted = ledger.emit_delta(codec, self.step, tick, content, out, &mut self.encoded);
        emitted != Emitted::Silent
    }

    /// `GameLogic::keepalive`: the delta mode ships a fresh FULL of the
    /// team's view (and re-syncs the ledger to it); the full mode keeps
    /// the core's default (re-send the cached full).
    pub(super) fn emit_keepalive(&mut self, tick: u64, team: Team, out: &mut BytesMut) -> bool {
        if !self.delta {
            return false;
        }
        let t = self.team_slot(team);
        let full = self.ledgers[t].resync(
            self.game.codec(),
            self.step,
            tick,
            &self.contents[t],
            &mut self.encoded,
        );
        out.extend_from_slice(&full);
        true
    }

    /// `GameLogic::private`: in delta mode a one-shot FULL of the team's
    /// view for a member without a baseline (module docs); otherwise the
    /// ordinary frame (ack, RPC answers, session payload).
    pub(super) fn emit_private(
        &mut self,
        world: &World,
        player: PlayerId,
        team: Team,
        responses: &[RpcReply],
        out: &mut BytesMut,
    ) -> bool {
        if self.delta {
            let t = self.team_slot(team);
            let group_full = self.ledgers[t].full_sent(self.step);
            if self.baselines.owed(player, team, self.step, || group_full) {
                let full = self.ledgers[t].full_frame(
                    self.game.codec(),
                    self.step,
                    self.tick,
                    &self.contents[t],
                    &mut self.encoded,
                );
                return crate::common::emit_private_full(
                    &mut self.game,
                    world,
                    &self.player_entity,
                    &mut self.input,
                    player,
                    &full,
                    responses,
                    out,
                );
            }
        }
        crate::common::emit_private_frame(
            &mut self.game,
            world,
            &self.player_entity,
            &mut self.input,
            player,
            responses,
            out,
        )
    }
}
