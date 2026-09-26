//! The TEAMS phase of the composite (`docs/CROSS-SHARD.md` §8b.4): who
//! sees what on this shard, each viewed team's content (own + lent +
//! imported, one record per wire) and this shard's export.

use std::collections::{BTreeSet, HashMap};

use bevy_ecs::prelude::{With, World};
use bytes::{Bytes, BytesMut};
use gsb_core::shard::{BorderRecord, TeamExport, TeamImports, TeamRecord};

use crate::codec::RecordCodec;
use crate::common::SetLedger;
use crate::game::{Game, ShardGame, TeamGame, Wire};
use crate::identity::WireId;
use crate::sharded::team::*;
use crate::space::{Partition, Vision};

/// The game's broadcast marker (the codec's `Marker`).
type Marker<G> = <<G as Game>::Codec as RecordCodec>::Marker;
/// The game's record query (the codec's `Query`).
type RecordQuery<G> = <<G as Game>::Codec as RecordCodec>::Query;

/// One record this shard knows typed: its wire id, wire value, vision
/// position (if any) and team (own entities only — a lent record's team
/// is unknown).
struct Known<W, P> {
    wire: u64,
    value: W,
    pos: Option<P>,
    team: Option<Team>,
    lent: bool,
}

impl<G, P, V> ShardedTeamRoom<G, P, V>
where
    G: ShardGame + TeamGame,
    P: Partition<Wire<G>>,
    V: Vision,
{
    /// `ShardLogic::team_exchange`: rebuild every viewed team's content
    /// and return this shard's export.
    pub(in crate::sharded) fn exchange(
        &mut self,
        world: &mut World,
        tick: u64,
        borrowed: &[BorderRecord<Wire<G>>],
        imported: &TeamImports,
    ) -> TeamExport {
        self.tick = tick;
        let known = self.known(world, borrowed);
        let index: HashMap<u64, usize> =
            known.iter().enumerate().map(|(i, k)| (k.wire, i)).collect();
        // Vision sources: this shard's OWN units, per team.
        let mut cells: HashMap<(V::Cell, Team), Vec<V::Pos>> = HashMap::new();
        for k in known.iter().filter(|k| !k.lent) {
            if let (Some(team), Some(pos)) = (k.team, k.pos) {
                cells
                    .entry((self.vision.cell(&pos), team))
                    .or_default()
                    .push(pos);
            }
        }
        let views = self.viewed_teams(world);
        // The teams this shard exports for (any with a unit here) and
        // builds content for (any with a viewer here).
        let mut teams: BTreeSet<Team> = known.iter().filter_map(|k| k.team).collect();
        teams.extend(views.iter().copied());

        let mut records: Vec<TeamRecord> = Vec::new();
        let top = teams
            .iter()
            .map(|t| usize::from(t.0) + 1)
            .max()
            .unwrap_or(0);
        if self.contents.len() < top {
            self.contents.resize_with(top, HashMap::new);
            self.ledgers.resize_with(top, SetLedger::default);
        }
        for content in &mut self.contents {
            content.clear();
        }
        for &team in &teams {
            let sees = |pos: &Option<V::Pos>| {
                pos.is_some_and(|target| {
                    self.vision
                        .neighborhood(self.vision.cell(&target))
                        .any(|c| {
                            cells.get(&(c, team)).is_some_and(|units| {
                                units.iter().any(|v| self.vision.sees(v, &target))
                            })
                        })
                })
            };
            // Members first (the budget keeps them), then what they see.
            let mut visible: Vec<&Known<Wire<G>, V::Pos>> = known
                .iter()
                .filter(|k| !k.lent && k.team == Some(team))
                .collect();
            let members = visible.len();
            let seen = known
                .iter()
                .filter(|k| k.team != Some(team) && sees(&k.pos));
            visible.extend(seen);
            let kept = self
                .budget
                .cut(visible.iter().map(|k| (k.wire, &k.value)), members);
            let keep = visible.len().min(self.budget.records);
            self.over_budget += (visible.len() - keep) as u64;
            for (i, k) in visible.iter().enumerate() {
                if !self.budget.keeps(kept, i) {
                    continue;
                }
                let bytes = self.body(k.wire, &k.value);
                records.push(TeamRecord {
                    team: u64::from(team.0),
                    wire: k.wire,
                    bytes,
                });
            }
            if !views.contains(&team) {
                continue;
            }
            // The viewed team's content: all it sees here (uncut — the
            // budget bounds the EXPORT), this shard's neutrals, and what
            // the other shards exported for it. One record per wire;
            // own > lent > imported.
            let content = &mut self.contents[usize::from(team.0)];
            for k in &visible {
                content.insert(k.wire, Shown::Typed(k.value.clone()));
            }
            for k in known.iter().filter(|k| !k.lent && k.team.is_none()) {
                content.insert(k.wire, Shown::Typed(k.value.clone()));
            }
            for r in imported.team(u64::from(team.0)) {
                content
                    .entry(r.wire)
                    .or_insert_with(|| match index.get(&r.wire) {
                        Some(&i) => Shown::Typed(known[i].value.clone()),
                        None => Shown::Encoded(r.bytes.clone()),
                    });
            }
        }
        self.bodies.retain(|_, (_, _, at)| *at == tick);
        TeamExport {
            views: views.iter().map(|t| u64::from(t.0)).collect(),
            records,
        }
    }

    /// Every record this shard knows typed: its own broadcast entities,
    /// then the lent records (the core already dropped a lent copy of an
    /// own entity — own wins).
    fn known(
        &self,
        world: &mut World,
        borrowed: &[BorderRecord<Wire<G>>],
    ) -> Vec<Known<Wire<G>, V::Pos>> {
        let codec = self.inner.game.codec();
        let mut query = world.query_filtered::<(
            &WireId,
            RecordQuery<G>,
            Option<&V::Pos>,
            Option<&TeamMember>,
        ), With<Marker<G>>>();
        let mut known: Vec<Known<Wire<G>, V::Pos>> = query
            .iter(world)
            .map(|(wire, item, pos, member)| Known {
                wire: wire.get(),
                value: codec.wire(item),
                pos: pos.copied(),
                team: member.map(|m| m.0),
                lent: false,
            })
            .collect();
        known.sort_unstable_by_key(|k| k.wire);
        known.extend(borrowed.iter().map(|r| Known {
            wire: r.wire,
            value: r.state.clone(),
            pos: (self.lent_pos)(&r.state),
            team: None,
            lent: true,
        }));
        known
    }

    /// The teams this shard hosts viewers of (its players' teams).
    fn viewed_teams(&self, world: &World) -> BTreeSet<Team> {
        self.inner
            .player_entity
            .values()
            .filter_map(|&e| Self::team_of_entity(world, e))
            .collect()
    }

    /// The export body of record `wire` with value `value`: the cached
    /// bytes while the value is unchanged, else a fresh encode.
    ///
    /// **The owner paces its exports (A10).** In delta mode a changed
    /// value that is not due on this step keeps the cached (last
    /// exported) body: an importing shard cannot compute the class of a
    /// body, so the owner — which holds the typed value — advances the
    /// body only on the record's due steps, and the importer passes a
    /// changed body straight on (`Shown::Encoded` is always due). The
    /// schedule is the one the importer would apply itself (same class,
    /// same wire phase, the room's shards step in lockstep). A record
    /// not exported on the previous step is encoded fresh (it enters an
    /// export at its current value). The full mode ignores the rate.
    fn body(&mut self, wire: u64, value: &Wire<G>) -> Bytes {
        let (tick, step) = (self.tick, self.step);
        let codec = self.inner.game.codec();
        if let Some((cached, bytes, at)) = self.bodies.get_mut(&wire)
            && (cached == value || (self.delta && !codec.send_every(value).due(step, wire)))
        {
            *at = tick;
            return bytes.clone();
        }
        let mut buf = BytesMut::new();
        self.inner.game.codec().encode(wire, value, &mut buf);
        let bytes = buf.freeze();
        self.bodies
            .insert(wire, (value.clone(), bytes.clone(), tick));
        bytes
    }
}
