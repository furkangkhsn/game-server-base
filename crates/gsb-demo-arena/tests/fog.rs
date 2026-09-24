//! Team fog of war across THREE teams, through the real room actor: what
//! each connection's channel actually receives is exactly what its team
//! can see — nothing more (the cheat test: absent from the package ⇒
//! never on that client's wire), nothing less.
//!
//! Teams are round-robin by join order: joins 1..=6 → teams 0, 1, 2, 0,
//! 1, 2. Units are placed with `MoveTo` and the ticker runs until every
//! move has settled. Vision radius: 15 m, 3D.

mod common;

use common::{Arena, Client, SETTLE, set};
use gsb_demo_arena::ArenaGame;

/// Join six players (two per team) and send each unit to its spot.
async fn six_units(arena: &mut Arena, spots: [(f32, f32, f32); 6]) -> Vec<Client> {
    let mut clients = Vec::new();
    for conn in 1..=6 {
        clients.push(arena.join(conn).await);
    }
    for (c, (x, y, z)) in clients.iter().zip(spots) {
        c.move_to(x, y, z, 0).await;
    }
    arena.advance(&mut clients, SETTLE).await;
    clients
}

/// Each of three teams receives exactly its own units plus the enemies
/// ONE of its members is within 15 m of — and both members of a team
/// receive the identical package.
///
/// ```text
/// A0 (0,0,0)    B0 (10,0,0)   C0 (-40,0,12)     team 0: A*   team 1: B*
/// A1 (-40,0,0)  B1 (40,8,0)   C1 (0,0,40)       team 2: C*
/// ```
/// A0–B0 are 10 m apart (mutual), A1–C0 12 m (mutual); every other
/// cross-team pair is more than 40 m apart.
#[tokio::test]
async fn each_team_receives_exactly_what_its_members_see() {
    let mut arena = Arena::new(ArenaGame::default());
    let spots = [
        (0.0, 0.0, 0.0),    // A0, team 0
        (10.0, 0.0, 0.0),   // B0, team 1
        (-40.0, 0.0, 12.0), // C0, team 2
        (-40.0, 0.0, 0.0),  // A1, team 0
        (40.0, 8.0, 0.0),   // B1, team 1
        (0.0, 0.0, 40.0),   // C1, team 2
    ];
    let cs = six_units(&mut arena, spots).await;
    let [a0, b0, c0, a1, b1, c1] = [0, 1, 2, 3, 4, 5].map(|i| cs[i].id);

    let team0 = set(&[a0, a1, b0, c0]);
    let team1 = set(&[b0, b1, a0]);
    let team2 = set(&[c0, c1, a1]);
    for (i, expected) in [&team0, &team1, &team2, &team0, &team1, &team2]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            &cs[i].sees(),
            expected,
            "client {i} (team {}) receives exactly its team's view",
            i % 3
        );
    }
}

/// Shared team vision: a unit seen by ONE member of a team is in the
/// package of EVERY member (the far one included); a team none of whose
/// members sees it does not get it — until one of its own does.
///
/// ```text
/// A0 (0,0,0) scout    A1 (-40,0,-40) far      team 0
/// B0 (8,0,0)          B1 (40,0,40)   far      team 1
/// C0 (-40,0,40) far   C1 (40,0,-40) → (8,0,8) team 2
/// ```
#[tokio::test]
async fn team_vision_is_shared_by_members_and_only_by_them() {
    let mut arena = Arena::new(ArenaGame::default());
    let spots = [
        (0.0, 0.0, 0.0),     // A0, team 0
        (8.0, 0.0, 0.0),     // B0, team 1
        (-40.0, 0.0, 40.0),  // C0, team 2
        (-40.0, 0.0, -40.0), // A1, team 0
        (40.0, 0.0, 40.0),   // B1, team 1
        (40.0, 0.0, -40.0),  // C1, team 2
    ];
    let mut cs = six_units(&mut arena, spots).await;
    let [a0, b0, c0, a1, b1, c1] = [0, 1, 2, 3, 4, 5].map(|i| cs[i].id);

    // A1 is 40+ m from B0, yet receives it: A0 sees it for the team.
    assert_eq!(cs[3].sees(), set(&[a0, a1, b0]), "far A1 shares A0's sight");
    assert_eq!(cs[0].sees(), set(&[a0, a1, b0]), "A0 itself");
    // Likewise far B1 receives A0 through B0.
    assert_eq!(cs[4].sees(), set(&[b0, b1, a0]), "far B1 shares B0's sight");
    // Team 2 sees neither: vision is shared within a team, never across.
    assert_eq!(cs[2].sees(), set(&[c0, c1]), "team 2 (C0)");
    assert_eq!(cs[5].sees(), set(&[c0, c1]), "team 2 (C1)");

    // C1 walks up to the skirmish: 8 m from B0, 11.3 m from A0.
    cs[5].move_to(8.0, 0.0, 8.0, 0).await;
    arena.advance(&mut cs, SETTLE).await;
    for i in [2, 5] {
        assert_eq!(
            cs[i].sees(),
            set(&[c0, c1, a0, b0]),
            "team 2, both members (far C0 through C1): client {i}"
        );
    }
    for i in [0, 3] {
        assert_eq!(cs[i].sees(), set(&[a0, a1, b0, c1]), "team 0: client {i}");
    }
    for i in [1, 4] {
        assert_eq!(cs[i].sees(), set(&[b0, b1, a0, c1]), "team 1: client {i}");
    }
}
