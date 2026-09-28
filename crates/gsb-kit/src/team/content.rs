//! The per-tick content rebuild: who is on which team, the vision
//! grid, and what each team's snapshot carries (a child of the team
//! module: it fills the room's private caches).

use std::collections::HashMap;

use bevy_ecs::prelude::World;

use crate::codec::RecordCodec;
use crate::common::SetLedger;
use crate::game::TeamGame;
use crate::space::Vision;
use crate::team::*;

impl<G: TeamGame, V: Vision> TeamRoom<G, V> {
    /// Rebuild the per-tick caches (module docs): team units, neutrals, the
    /// vision grid, and — the important part — each team's *content*: own
    /// team ∪ neutral ∪ the units of EVERY other team in the team's
    /// vision.
    pub(super) fn rebuild(&mut self, world: &mut World) {
        let Self {
            game,
            vision,
            ledgers,
            team_units,
            neutral,
            sight,
            contents,
            sighted,
            ..
        } = self;
        for units in team_units.iter_mut() {
            units.clear();
        }
        neutral.clear();
        sight.clear();

        // Membership is read from the WORLD (each entity's `TeamMember`
        // component, written at join): no reverse connection map — the
        // component *is* the table, and a runtime team change needs no
        // bookkeeping here at all. An entity without the component is
        // neutral (ownerless): broadcast to ALL teams — the broadcast set
        // stays exactly the codec's marker.
        let codec = game.codec();
        for (wire_id, item, pos, member, radius) in sighted.state(world).iter(world) {
            let wire = codec.wire(item);
            match member {
                Some(&TeamMember(team)) => {
                    let t = usize::from(team.0);
                    if t >= team_units.len() {
                        team_units.resize_with(t + 1, Vec::new);
                    }
                    if let Some(p) = pos {
                        sight.add(vision, team, *p, radius.copied());
                    }
                    team_units[t].push((wire_id.get(), wire, pos.copied()));
                }
                None => neutral.push((wire_id.get(), wire)),
            }
        }
        // One slot per team seen so far (the slots only grow, so a team
        // whose members all left keeps an empty-ledger slot).
        let teams = team_units.len();
        contents.resize_with(teams, HashMap::new);
        ledgers.resize_with(teams, SetLedger::default);

        // Content: own team + neutral, then enemy units in vision.
        for (t, content) in contents.iter_mut().enumerate() {
            content.clear();
            for (id, wire, _) in &team_units[t] {
                content.insert(*id, wire.clone());
            }
            for (id, wire) in neutral.iter() {
                content.insert(*id, wire.clone());
            }
        }
        for (t, content) in contents.iter_mut().enumerate() {
            let team = Team(t as u8);
            for (_, enemies) in team_units.iter().enumerate().filter(|(e, _)| *e != t) {
                for (id, wire, pos) in enemies {
                    let Some(target) = pos else { continue };
                    if sight.sees(vision, team, target) {
                        content.insert(*id, wire.clone());
                    }
                }
            }
        }
    }

    /// The content slot of `team`, created on demand for a team this
    /// tick's rebuild has not seen (a group whose members carry no
    /// team-member entity): such a team has no units, so it sees
    /// exactly the neutral entities.
    pub(super) fn team_slot(&mut self, team: Team) -> usize {
        let t = usize::from(team.0);
        while self.contents.len() <= t {
            let neutral = self.neutral.iter().map(|(id, wire)| (*id, wire.clone()));
            self.contents.push(neutral.collect());
            self.ledgers.push(SetLedger::default());
            self.team_units.push(Vec::new());
        }
        t
    }
}
