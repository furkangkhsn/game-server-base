//! "Cephe" hosted by the REAL server: the real registry (its team hub
//! relaying the four shards' faction views) routes joins through the war
//! module's router, and real TCP clients apply what they receive through
//! the kit's reference client (`gsb_kit::client::ClientView`). Checked
//! from the network side: the join's `Welcome` names the faction of the
//! client's own record; allies are in view map-wide; an enemy is in view
//! only within sight of a unit of the client's faction — never an
//! un-spotted one.

#![cfg(feature = "game-war")]

mod common;
mod hosted;

use std::time::{Duration, Instant};

use gsb_demo_war::realm::faction_of;
use gsb_demo_war::war::Kind;
use gsb_demo_war::world::tower;
use gsb_demo_war::{Pos3, Realm};
use gsb_kit::team::Team;
use gsb_server::games::war::WarModule;
use hosted::war::WarView;
use hosted::{Client, Door, eventually, hold, until_metric};

type War = Client<WarView>;

async fn join(addr: std::net::SocketAddr, name: &str) -> War {
    Client::join(&Door::Tcp, addr, name, 1).await
}

/// The fog invariant on every client, as a test assertion.
fn fog(cs: &[&mut War]) {
    for c in cs {
        if let Err(e) = c.view.fog_holds(1.0) {
            panic!("{}: {e}", c.entity);
        }
    }
}

/// The catalog's war (no saved characters): three unknown players, one
/// dealt to each faction by the identity hash, each at its base. Each is
/// welcomed with the faction its own record carries, sees its faction's
/// four towers — one per region, three of them on other shards — and no
/// enemy.
#[tokio::test]
async fn the_catalog_war_welcomes_each_faction_at_its_base() {
    let handle = gsb_server::start_server(Door::Tcp.config("war"))
        .await
        .expect("the war starts");
    let mut names = Vec::new();
    for f in 0..3u8 {
        let name = (0..)
            .map(|i| format!("soldier-{i}"))
            .find(|n| faction_of(n) == Team(f))
            .expect("a name for every faction");
        names.push(name);
    }
    let mut cs = Vec::new();
    for n in &names {
        cs.push(join(handle.addr, n).await);
    }
    let mut refs: Vec<&mut War> = cs.iter_mut().collect();
    eventually(
        &mut refs,
        Duration::from_secs(10),
        "every faction welcomed, with its towers in view",
        |cs| {
            fog(cs);
            cs.iter().all(|c| {
                c.view.faction().is_some()
                    && c.me().is_some()
                    && c.view.of_kind(Kind::Tower).len() == 4
            })
        },
    )
    .await;
    for (f, c) in cs.iter().enumerate() {
        let me = c.me().expect("in view");
        assert_eq!(c.view.faction(), Some(f as u32 + 1), "{}", names[f]);
        assert_eq!(me.faction, f as u32 + 1);
        assert_eq!(c.view.of_kind(Kind::Player), vec![c.entity], "no enemy");
        let towers = c
            .view
            .units
            .values()
            .filter(|r| r.kind == Kind::Tower as i32);
        assert!(towers.into_iter().all(|t| t.faction == me.faction));
    }
    handle.stop().await;
}

/// Saved characters: faction-0 players on shards 0 and 3 see each other
/// (map-wide); a faction-1 player stands 50 m from faction 0's tower on
/// shard 2 — the faction-0 players see it through that tower, 800 m and
/// more away; a faction-2 player on shard 1 sees neither. The fog holds
/// on every client for a second of snapshots, and a numbered move is
/// acked.
#[tokio::test]
async fn allies_map_wide_and_an_enemy_only_through_a_faction_tower() {
    let t = tower(Team(0), 2);
    let realm = Realm::empty()
        .with_login("a0", Team(0), Pos3::ground(-100.0, -600.0))
        .with_login("b0", Team(0), Pos3::ground(650.0, 150.0))
        .with_login("e1", Team(1), Pos3::ground(t[0] + 50.0, t[1]))
        .with_login("q2", Team(2), Pos3::ground(650.0, -150.0));
    let handle = gsb_server::start_game_server(
        Box::new(WarModule::with_realm(realm)),
        Door::Tcp.config("war"),
    )
    .await
    .expect("the war starts");
    let mut a = join(handle.addr, "a0").await;
    let mut b = join(handle.addr, "b0").await;
    let mut e = join(handle.addr, "e1").await;
    let mut q = join(handle.addr, "q2").await;
    let (ia, ib, ie, iq) = (a.entity, b.entity, e.entity, q.entity);
    let mut faction0 = vec![ia, ib, ie];
    faction0.sort_unstable();
    let mut all = [&mut a, &mut b, &mut e, &mut q];
    eventually(
        &mut all,
        Duration::from_secs(10),
        "allies and the spotted enemy in view",
        |cs| {
            fog(cs);
            cs[0].view.of_kind(Kind::Player) == faction0
                && cs[1].view.of_kind(Kind::Player) == faction0
                && cs[3].view.of_kind(Kind::Player) == vec![iq]
        },
    )
    .await;
    hold(&mut all, Duration::from_secs(1), |cs| {
        fog(cs);
        assert!(
            !cs[3].sees(ie) && !cs[3].sees(ia),
            "faction 2 has no eyes there"
        );
        assert!(
            !cs[2].sees(ia) && !cs[2].sees(ib),
            "nor faction 1 on faction 0"
        );
    })
    .await;
    a.move_to(-101.0, -600.0, 1).await;
    eventually(
        &mut [&mut a],
        Duration::from_secs(5),
        "the move acked",
        |cs| cs[0].view.acks.last() == Some(&1),
    )
    .await;
    handle.stop().await;
}

/// `[war] team_budget` from a config file reaches every shard: at one
/// record per faction per tick, shard 0 exports faction 0's tower (the
/// kit keeps members in wire order — the towers, raised on the first
/// tick, come first) and cuts the player there; the faction-0 player on
/// shard 3 then sees all four towers but not that ally. Under the
/// default budget it sees both.
///
/// The default's "sees both" is a condition, waited for (it used to be
/// read after a fixed 500 ms, which a starved run could spend before the
/// ally's export arrived). The cut is a silence, so its window must be
/// one the run really lived through: both shards demonstrably ticked in
/// it (each player's numbered move acked, shard 0's first — the ally's
/// unit would ride its exports) and at least 500 ms (≈ 15 ticks of
/// relay) passed; a starved run waits longer for that, it does not pass
/// on an empty window (BACKLOG F52).
#[tokio::test]
async fn the_war_table_budget_reaches_the_shards() {
    async fn start(budget: Option<u32>) -> (gsb_server::ServerHandle, War, War) {
        let realm = Realm::empty()
            .with_login("a0", Team(0), Pos3::ground(-100.0, -600.0))
            .with_login("b0", Team(0), Pos3::ground(650.0, 150.0));
        let table = budget.map_or(String::new(), |n| format!("[war]\nteam_budget = {n}\n"));
        let cfg = hosted::config_file(
            "war-budget",
            &format!("game = \"war\"\nbind = \"127.0.0.1:0\"\n{table}"),
        );
        let handle = gsb_server::start_game_server(Box::new(WarModule::with_realm(realm)), cfg)
            .await
            .expect("the war starts");
        let a = join(handle.addr, "a0").await;
        let b = join(handle.addr, "b0").await;
        (handle, a, b)
    }
    fn towers(c: &War) -> usize {
        c.view.of_kind(Kind::Tower).len()
    }

    // The default budget: the far ally comes into view with the towers.
    let (handle, mut a, mut b) = start(None).await;
    let ia = a.entity;
    eventually(
        &mut [&mut a, &mut b],
        Duration::from_secs(10),
        "four towers and the far ally (the default)",
        |cs| towers(cs[1]) == 4 && cs[1].sees(ia),
    )
    .await;
    handle.stop().await;

    // One record per faction per tick: the ally is cut, the towers are not.
    let (handle, mut a, mut b) = start(Some(1)).await;
    let ia = a.entity;
    let mut both = [&mut a, &mut b];
    eventually(&mut both, Duration::from_secs(10), "four towers", |cs| {
        towers(cs[1]) == 4
    })
    .await;
    let opened = Instant::now();
    both[0].move_to(-100.0, -600.0, 1).await;
    eventually(&mut both, Duration::from_secs(10), "shard 0 ticked", |cs| {
        assert!(!cs[1].sees(ia), "cut");
        cs[0].view.acks.last() == Some(&1)
    })
    .await;
    both[1].move_to(650.0, 150.0, 1).await;
    eventually(
        &mut both,
        Duration::from_secs(10),
        "shard 3 ticked after it, and 500 ms passed",
        |cs| {
            assert!(!cs[1].sees(ia), "cut");
            cs[1].view.acks.last() == Some(&1) && opened.elapsed() >= Duration::from_millis(500)
        },
    )
    .await;
    assert_eq!(towers(&b), 4, "the towers are not cut");
    handle.stop().await;
}

/// `[war] disconnect_grace_secs` reaches the shards: at 0 a dropped
/// player's unit leaves at once (its far ally loses it); under the
/// default grace it stays parked in the world (and in the ally's view).
///
/// "Leaves" is a condition, waited for. "Stays" is read only once the
/// server has PARKED the unit (the room's `gsb_room_detached` gauge) and
/// a second after that: a fixed second after the client's drop was a
/// window a starved server could spend before it even saw the drop — a
/// pass on nothing — or, for the zero grace, before it removed the unit
/// (BACKLOG F52).
#[tokio::test]
async fn the_war_table_grace_reaches_the_shards() {
    async fn ally_after_a_drop(grace: Option<f64>) -> bool {
        let realm = Realm::empty()
            .with_login("a0", Team(0), Pos3::ground(-100.0, -600.0))
            .with_login("b0", Team(0), Pos3::ground(650.0, 150.0));
        let table = grace.map_or(String::new(), |g| {
            format!("[war]\ndisconnect_grace_secs = {g}\n")
        });
        let cfg = hosted::config_file(
            "war-grace",
            &format!(
                "game = \"war\"\nbind = \"127.0.0.1:0\"\nhttp_listen = \"127.0.0.1:0\"\n{table}"
            ),
        );
        let handle = gsb_server::start_game_server(Box::new(WarModule::with_realm(realm)), cfg)
            .await
            .expect("the war starts");
        let ops = handle.http_addr.expect("ops surface");
        let a = join(handle.addr, "a0").await;
        let mut b = join(handle.addr, "b0").await;
        let ia = a.entity;
        eventually(&mut [&mut b], Duration::from_secs(10), "the ally", |cs| {
            cs[0].sees(ia)
        })
        .await;
        drop(a); // the transport dies
        let still = if grace == Some(0.0) {
            eventually(
                &mut [&mut b],
                Duration::from_secs(10),
                "the unit leaves",
                |cs| !cs[0].sees(ia),
            )
            .await;
            false
        } else {
            until_metric(
                ops,
                "gsb_room_detached",
                1,
                Duration::from_secs(10),
                "the server parks the unit",
                |n| n >= 1.0,
            )
            .await;
            hold(&mut [&mut b], Duration::from_secs(1), |_| {}).await;
            b.sees(ia)
        };
        handle.stop().await;
        still
    }
    assert!(!ally_after_a_drop(Some(0.0)).await, "no grace: gone");
    assert!(
        ally_after_a_drop(None).await,
        "parked under the default grace"
    );
}
