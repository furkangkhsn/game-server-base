//! K4 (`docs/GAME-MODULE.md`): the hosted MMO places a player by the
//! identity it AUTHENTICATED as — the ticket's validated player when the
//! server has a ticket hook (whatever name the client claims), the
//! claimed `Auth.name` on the local-auth path (`mmo_e2e.rs`) — and a
//! parked character resumes where it was parked, not at its save.
//! Real server, real registry, real TCP clients.

#![cfg(feature = "game-mmo")]

mod common;
mod hosted;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::auth::{TicketAuth, TicketError, TicketValidator, ValidatedTicket};
use gsb_core::id::RoomId;
use gsb_core::shard::minting_shard;
use gsb_demo_mmo::{Pos3, Realm};
use gsb_server::games::mmo::MmoModule;
use gsb_server::{ServerHandle, ServerHooks};
use hosted::mmo::{MmoView, dm, ground};
use hosted::{Client, Door, eventually, hold};

type Mmo = Client<MmoView>;

/// The shard that minted a wire id (interleaved minting).
fn minted_by(id: u64) -> u64 {
    minting_shard(id, gsb_demo_mmo::world::SHARDS) as u64
}

/// The platform's validator: ticket `t-NAME` is player `NAME`, pinned to
/// room 1.
fn tickets() -> TicketValidator {
    type Validation = Pin<Box<dyn Future<Output = Result<ValidatedTicket, TicketError>> + Send>>;
    Arc::new(|t: bytes::Bytes| -> Validation {
        Box::pin(async move {
            match std::str::from_utf8(&t)
                .ok()
                .and_then(|t| t.strip_prefix("t-"))
            {
                Some(player) => Ok(ValidatedTicket {
                    player: player.to_string(),
                    room: RoomId(1),
                    extra: None,
                }),
                None => Err(TicketError::Rejected("not a ticket".into())),
            }
        })
    })
}

/// An MMO server over `realm` with the `[mmo]` table `table`, and the
/// ticket hook when `ticketed`.
async fn start(realm: Realm, table: &str, ticketed: bool) -> ServerHandle {
    let mut cfg = Door::Tcp.config("mmo");
    cfg.raw = toml::from_str(table).expect("table parses");
    let hooks = ServerHooks {
        ticket: ticketed.then(|| TicketAuth {
            validator: tickets(),
            timeout: Duration::from_secs(2),
        }),
    };
    let module = Box::new(MmoModule::with_realm(realm));
    gsb_server::start_game_server_with(module, cfg, hooks, None)
        .await
        .expect("the MMO starts")
}

/// Every client sees itself.
async fn spawned(cs: &mut [&mut Mmo]) {
    eventually(cs, Duration::from_secs(20), "everyone sees itself", |cs| {
        cs.iter().all(|c| c.me().is_some())
    })
    .await;
}

/// With a ticket hook the ticket's player picks the character: a client
/// claiming to be `bob` with ann's ticket gets ann's character on ann's
/// shard; bob's ticket gets bob's; a ticketed player with no save lands
/// on the default waystone's shard, at the waystone.
#[tokio::test]
async fn the_ticket_player_picks_the_character_not_the_claimed_name() {
    let realm = Realm::empty()
        .with_login("ann", Pos3::new(200.0, 0.0, -200.0)) // shard 1
        .with_login("bob", Pos3::new(-200.0, 0.0, 200.0)); // shard 2
    let handle = start(realm, "", true).await;
    let at = |name: &'static str, ticket: &'static str| {
        Client::join_with_ticket(&Door::Tcp, handle.addr, name, ticket.as_bytes(), 1)
    };
    let mut claims_bob: Mmo = at("bob", "t-ann").await;
    let mut bob: Mmo = at("", "t-bob").await;
    let mut carl: Mmo = at("ann", "t-carl").await;
    spawned(&mut [&mut claims_bob, &mut bob, &mut carl]).await;
    let expected = [
        (&claims_bob, 1, dm(200.0, -200.0)),
        (&bob, 2, dm(-200.0, 200.0)),
        (&carl, 0, dm(-256.0, -256.0)),
    ];
    for (c, shard, spot) in expected {
        assert_eq!(minted_by(c.entity), shard, "{} on shard {shard}", c.entity);
        assert_eq!(ground(&c.me().unwrap()), spot, "at its save / waystone 0");
    }
    handle.stop().await;
}

/// Ann's save is on shard 1. She travels to waystone 2 (shard 2) and
/// drops there: her next session RESUMES her parked character on shard 2
/// (same wire id, where she stood — not at her save), and it plays. Once
/// a later drop has logged her out, a new session is a fresh join the
/// router sends back to her save on shard 1.
///
/// The logout timer is the deadline her reconnect must beat (drop, 300 ms,
/// a whole new connect-auth-join): 5 s, not 1 s — a starved run's
/// reconnect can take a second, and then she was logged out and the
/// "resume" was a fresh join (BACKLOG F52). The logout is waited for as a
/// condition; its bound is only a hang guard.
#[tokio::test]
async fn a_resume_lands_on_the_parked_character_and_a_logout_returns_to_the_save() {
    let realm = Realm::empty()
        .with_login("ann", Pos3::new(200.0, 0.0, -200.0)) // shard 1
        .with_login("obs", Pos3::new(-240.0, 0.0, 256.0)); // shard 2, by waystone 2
    let handle = start(realm, "[mmo]\nlogout_grace_secs = 5", false).await;
    let join = |name: &'static str| Client::join(&Door::Tcp, handle.addr, name, 1);
    let mut ann: Mmo = join("ann").await;
    let mut obs: Mmo = join("obs").await;
    let id = ann.entity;
    assert_eq!(minted_by(id), 1, "routed to her save's shard");
    spawned(&mut [&mut ann, &mut obs]).await;
    let waystone = dm(-256.0, 256.0);
    ann.travel(2, 1).await;
    eventually(
        &mut [&mut ann, &mut obs],
        Duration::from_secs(20),
        "ann arrives at waystone 2",
        |cs| cs[1].sees(id).map(|r| ground(&r)) == Some(waystone),
    )
    .await;

    drop(ann);
    hold(&mut [&mut obs], Duration::from_millis(300), |cs| {
        assert!(cs[0].sees(id).is_some(), "the parked character stays");
    })
    .await;
    let mut ann = join("ann").await;
    assert_eq!(ann.entity, id, "the resume found her parked on shard 2");
    spawned(&mut [&mut ann]).await;
    assert_eq!(ground(&ann.me().unwrap()), waystone, "where she was parked");
    ann.move_to(-240.0, 240.0, 1).await;
    eventually(
        &mut [&mut ann, &mut obs],
        Duration::from_secs(20),
        "the resumed character plays on shard 2",
        |cs| {
            cs[1].sees(id).map(|r| ground(&r)) == Some(dm(-240.0, 240.0))
                && cs[0].view.acks.last() == Some(&1)
        },
    )
    .await;

    drop(ann);
    eventually(
        &mut [&mut obs],
        Duration::from_secs(30),
        "ann logs out after the grace",
        |cs| cs[0].sees(id).is_none(),
    )
    .await;
    let mut ann = join("ann").await;
    assert_ne!(ann.entity, id, "a fresh character");
    assert_eq!(minted_by(ann.entity), 1, "the router sends her to her save");
    spawned(&mut [&mut ann]).await;
    assert_eq!(ground(&ann.me().unwrap()), dm(200.0, -200.0), "at her save");
    handle.stop().await;
}
