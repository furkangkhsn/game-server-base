//! Who a login is (K4, `docs/GAME-MODULE.md`), on the real actors: a
//! saved character appears where it was saved, on its side, on the shard
//! that owns that spot; an unsaved player on its hashed faction's side
//! at that faction's base; and every session is told its faction once,
//! in the kit's session payload (`Welcome`, `Private.game`) — a late
//! joiner into a playing faction with its one-shot full.

mod common;

use common::{War, realm};
use gsb_demo_war::Pos3;
use gsb_demo_war::codec::to_dm;
use gsb_demo_war::realm::faction_of;
use gsb_demo_war::war::Welcome;
use gsb_demo_war::world::{BASES, home_shard};

#[tokio::test(start_paused = true)]
async fn logins_appear_where_the_realm_places_them_and_learn_their_faction() {
    let r = realm(&[("ann", 2, 300.0, 250.0), ("cid", 2, 310.0, 250.0)]);
    let mut war = War::new(&r).await;
    let ann = war.join(1, "ann").await;
    let bob = war.join(2, "bob").await;
    war.steps(5).await;

    let bob_faction = faction_of("bob");
    let bob_shard = usize::from(bob_faction.0);
    let mut expected = [0u32; 4];
    expected[3] += 1;
    expected[bob_shard] += 1;
    assert_eq!(war.members(), expected, "ann on shard 3, bob at his base");

    let me = *war.clients[ann].me().expect("ann sees herself");
    assert_eq!((me.x, me.z, me.faction, me.hp), (3_000, 2_500, 3, 100));
    let me = *war.clients[bob].me().expect("bob sees himself");
    let [bx, bz] = BASES[bob_shard];
    let at = Pos3::ground(me.x as f32 / 10.0, me.z as f32 / 10.0);
    assert_eq!(home_shard(&at), bob_shard);
    assert!((me.x - to_dm(bx)).abs() <= 100 && (me.z - to_dm(bz)).abs() <= 100);
    assert_eq!(u32::from(bob_faction.0) + 1, me.faction);

    let welcome = |f: u32| Welcome {
        faction: f,
        factions: 3,
    };
    assert_eq!(war.clients[ann].welcomes(), [welcome(3)], "once");
    assert_eq!(war.clients[bob].welcomes(), [welcome(me.faction)]);

    // A late joiner into ann's playing faction: its one-shot full (ann's
    // group is long established) carries the welcome.
    let cid = war.join(3, "cid").await;
    war.steps(3).await;
    assert_eq!(war.clients[cid].welcomes(), [welcome(3)]);
    assert!(war.clients[cid].sees(war.wire(ann)), "the one-shot full");
    assert_eq!(war.clients[cid].view.counters().private_fulls, 1);
    war.steps(40).await;
    for c in [ann, bob, cid] {
        assert_eq!(war.clients[c].welcomes().len(), 1, "once per session");
    }
}
