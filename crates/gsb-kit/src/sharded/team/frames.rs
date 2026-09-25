//! The composite's frames: the team room's two snapshot modes
//! (`crate::team`'s `frames`) over contents that mix typed records with
//! records another shard encoded.

use bevy_ecs::prelude::World;
use bytes::{Bytes, BytesMut};
use gsb_core::id::PlayerId;
use gsb_core::rpc::RpcReply;

use crate::codec::RecordCodec;
use crate::common::{Emitted, SetLedger, WriteRecord, put_entity_body, put_entity_record};
use crate::game::{ShardGame, TeamGame, Wire};
use crate::sharded::team::*;
use crate::space::{Partition, Vision};

/// One record of a team's content: a value this shard knows typed (its
/// own entity, a lent record), or one another shard encoded (an import —
/// its body is exactly what the game's codec wrote there). Equality is
/// the ledger's change test: a record that switches between the two
/// forms is re-sent once, which is only ever a surplus upsert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Shown<W> {
    /// A typed wire value, encoded here.
    Typed(W),
    /// An encoded record body, written as is.
    Encoded(Bytes),
}

/// The ledger's record writer over [`Shown`]: typed values through the
/// game's codec, encoded bodies verbatim — both in the same record
/// framing (`entities` entries or the run), so the client cannot tell
/// them apart.
pub(crate) struct ShownWriter<'a, C>(pub(crate) &'a C);

impl<C: RecordCodec> WriteRecord<Shown<C::Wire>> for ShownWriter<'_, C> {
    /// The game's framing: an import was encoded by the same codec TYPE
    /// on its own shard (every shard of the room runs one game), so its
    /// body is spliced in the framing its owner's frames use.
    const RUN: bool = C::RUN;

    fn put(&self, id: u64, shown: &Shown<C::Wire>, out: &mut BytesMut) {
        match shown {
            Shown::Typed(wire) => put_entity_record(self.0, id, wire, out),
            Shown::Encoded(body) => put_entity_body::<C>(id, body, out),
        }
    }
}

impl<G, P, V> ShardedTeamRoom<G, P, V>
where
    G: ShardGame + TeamGame,
    P: Partition<Wire<G>>,
    V: Vision,
{
    /// The content slot of `team`, created on demand (a group whose team
    /// the TEAMS phase built nothing for sees nothing).
    fn team_slot(&mut self, team: Team) -> usize {
        let t = usize::from(team.0);
        if self.contents.len() <= t {
            self.contents.resize_with(t + 1, Default::default);
            self.ledgers.resize_with(t + 1, SetLedger::default);
        }
        t
    }

    /// `GameLogic::snapshot`: `team`'s frame this tick.
    pub(in crate::sharded) fn emit_snapshot(
        &mut self,
        tick: u64,
        team: Team,
        out: &mut BytesMut,
    ) -> bool {
        let t = self.team_slot(team);
        let writer = ShownWriter(self.inner.game.codec());
        let (content, ledger) = (&self.contents[t], &mut self.ledgers[t]);
        if !self.delta {
            return ledger.emit_full(&writer, tick, content, out, &mut self.encoded);
        }
        let emitted = ledger.emit_delta(&writer, self.step, tick, content, out, &mut self.encoded);
        emitted != Emitted::Silent
    }

    /// `GameLogic::keepalive`: delta mode ships a fresh FULL (the
    /// convergence guarantee); full mode keeps the core's re-send.
    pub(in crate::sharded) fn emit_keepalive(
        &mut self,
        tick: u64,
        team: Team,
        out: &mut BytesMut,
    ) -> bool {
        if !self.delta {
            return false;
        }
        let t = self.team_slot(team);
        let writer = ShownWriter(self.inner.game.codec());
        let full = self.ledgers[t].resync(
            &writer,
            self.step,
            tick,
            &self.contents[t],
            &mut self.encoded,
        );
        out.extend_from_slice(&full);
        true
    }

    /// `GameLogic::private`: in delta mode the one-shot FULL for a member
    /// without a baseline (a join, a resume, a team change, a migration
    /// arrival); otherwise the ordinary frame.
    pub(in crate::sharded) fn emit_private(
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
            if self.baselines.owed(player, team, group_full) {
                let writer = ShownWriter(self.inner.game.codec());
                let full = self.ledgers[t].full_frame(
                    &writer,
                    self.step,
                    self.tick,
                    &self.contents[t],
                    &mut self.encoded,
                );
                return crate::common::emit_private_full(
                    &mut self.inner.game,
                    world,
                    &self.inner.player_entity,
                    &mut self.inner.input,
                    player,
                    &full,
                    responses,
                    out,
                );
            }
        }
        crate::common::emit_private_frame(
            &mut self.inner.game,
            world,
            &self.inner.player_entity,
            &mut self.inner.input,
            player,
            responses,
            out,
        )
    }
}
