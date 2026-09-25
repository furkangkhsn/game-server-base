//! Combat on the real actors: a blow across a shard seam goes to the
//! victim's owner as a remote effect and lands there — once — and the
//! kill is credited once, by the owner, to the attacker's wire id; a
//! fallen player is back at its base. No friendly fire, no reach beyond
//! the attack range, on either side of a seam; a local blow lands
//! locally.

mod common;

use common::{War, realm};
use gsb_demo_war::codec::to_dm;
use gsb_demo_war::world::{ATTACK_DAMAGE, BASES, PLAYER_HP};

#[tokio::test(start_paused = true)]
async fn a_kill_across_a_seam_is_credited_once_by_the_victims_shard() {
    let r = realm(&[("a0", 0, -5.0, -300.0), ("v1", 1, 5.0, -300.0)]);
    let mut war = War::new(&r).await;
    let a = war.join(1, "a0").await;
    let v = war.join(2, "v1").await;
    war.steps(3).await;
    assert_eq!(war.members(), [1, 1, 0, 0], "either side of the x = 0 seam");
    let (attacker, victim) = (war.wire(a), war.wire(v));
    assert!(war.clients[a].sees(victim) && war.clients[v].sees(attacker));

    for _ in 0..4 {
        war.clients[a].attack(victim);
        war.step().await;
    }
    war.steps(3).await;
    let hits = war.hits();
    assert_eq!(hits.len(), 4, "{hits:?}");
    for (n, h) in hits.iter().enumerate() {
        assert_eq!(
            (h.shard, h.attacker, h.target),
            (1, attacker, victim),
            "{h:?}"
        );
        let left = PLAYER_HP - ATTACK_DAMAGE * (n as u16 + 1);
        assert_eq!((h.hp, h.killed), (left, left == 0), "{h:?}");
    }
    assert_eq!(hits.iter().filter(|h| h.killed).count(), 1, "credited once");

    // The fallen player is back on its feet at its base, far from the
    // seam and out of the attacker's faction's sight.
    let me = *war.clients[v].me().expect("in its own view");
    let [bx, bz] = BASES[1];
    assert_eq!((me.x, me.z, me.hp), (to_dm(bx), to_dm(bz), 100));
    assert!(
        !war.clients[a].sees(victim),
        "gone from the attacker's view"
    );
    war.clients[a].attack(victim);
    war.steps(3).await;
    assert!(war.hits().is_empty(), "nothing left to hit");
}

#[tokio::test(start_paused = true)]
async fn no_friendly_fire_no_reach_and_a_local_blow_lands_locally() {
    let r = realm(&[
        ("a0", 0, -5.0, -300.0),
        ("ally_across", 0, 5.0, -300.0),
        ("ally_here", 0, -5.0, -292.0),
        ("far_here", 1, -5.0, -270.0),
        ("far_across", 1, 25.0, -300.0),
        ("near_here", 1, -5.0, -310.0),
    ]);
    let mut war = War::new(&r).await;
    let mut ids = Vec::new();
    for (conn, name) in [
        "a0",
        "ally_across",
        "ally_here",
        "far_here",
        "far_across",
        "near_here",
    ]
    .into_iter()
    .enumerate()
    {
        ids.push(war.join(conn as u64 + 1, name).await);
    }
    war.steps(3).await;
    let a = ids[0];
    for &t in &ids[1..5] {
        assert!(war.clients[a].sees(war.wire(t)), "client {t} in view");
        let target = war.wire(t);
        war.clients[a].attack(target);
        war.step().await;
    }
    war.steps(3).await;
    let stray = war.hits();
    assert!(stray.is_empty(), "allies and targets out of reach: {stray:?}");

    let near = war.wire(ids[5]);
    war.clients[a].attack(near);
    war.steps(2).await;
    let hits = war.hits();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(
        (hits[0].shard, hits[0].attacker, hits[0].target),
        (0, war.wire(a), near)
    );
    assert_eq!(hits[0].hp, PLAYER_HP - ATTACK_DAMAGE);
}
