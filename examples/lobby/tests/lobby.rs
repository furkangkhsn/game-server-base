//! The B21 showcase cannot rot: the lobby's grant lets a player in over
//! the sealed rUDP door (the server key pinned from the grant) and over
//! TCP; the game spawns each character where the class its ticket
//! carries stands; a forged, an expired and a game-refused ticket are
//! refused (ERROR 10, the connection alive) and counted apart; and a
//! server built from a `[ticket]` table validates the same tickets.

use gsb_client::ClientError;
use gsb_core::auth::TicketReason;
use gsb_core::metrics::{MetricReport, TicketCounts};
use gsb_example_lobby::game::UNKNOWN_CLASS;
use gsb_example_lobby::lobby::Lobby;
use gsb_example_lobby::{client, server};
use gsb_protocol::base::ErrorCode;
use gsb_ticket::Transport;
use tokio::sync::mpsc;

fn refused_code(r: Result<client::Played, ClientError>) -> (ErrorCode, String) {
    match r {
        Ok(p) => panic!("let in as {}", p.player),
        Err(e) => {
            let s = e
                .server()
                .unwrap_or_else(|| panic!("not a server refusal: {e}"));
            (s.code, s.message.clone())
        }
    }
}

/// The ticket counts of the server's FINAL report: a connection hands
/// its counts on at its next frame after the flush interval or at its
/// end, and the stop's final report waits for every connection's last
/// sample (F35) — so the stop is what makes the count exact.
fn final_tickets(reports: &mut mpsc::UnboundedReceiver<MetricReport>) -> TicketCounts {
    let mut last = None;
    while let Ok(r) = reports.try_recv() {
        last = Some(r.net.tickets);
    }
    last.expect("at least the final report")
}

#[tokio::test]
async fn the_lobby_lets_players_in_and_the_game_spawns_them_by_their_class() {
    let key = Lobby::key();
    let (handle, mut reports) = server::start(key.trusted()).await;
    let udp_key = handle.udp_public_key.expect("a sealed door");
    let (udp, tcp) = (handle.addrs[0], handle.addrs[1]);
    let lobby = Lobby::new(key, server::AUDIENCE, udp, udp_key, tcp);
    let now = gsb_ticket::time::now();

    let ann = lobby.login("ann", now).expect("ann");
    let ann = client::play(&ann, Transport::Udp).await.expect("ann plays");
    assert_eq!(ann.player, "ann", "the identity is the ticket's");
    assert_eq!(ann.at, (-30, 0), "a mage spawns west");
    let bob = lobby.login("bob", now).expect("bob");
    let bob = client::play(&bob, Transport::Tcp).await.expect("bob plays");
    assert_eq!(bob.at, (30, 0), "a warrior spawns east");

    // The game's own check: a class the game does not know.
    let eve = lobby.login("eve", now).expect("eve");
    let (code, msg) = refused_code(client::play(&eve, Transport::Udp).await);
    assert_eq!(code, ErrorCode::TicketInvalid);
    assert!(msg.contains(UNKNOWN_CLASS.name()), "{msg}");
    // A ticket signed by a key the server does not trust.
    let forger = Lobby::new(Lobby::key(), server::AUDIENCE, udp, udp_key, tcp);
    let forged = forger.login("ann", now).expect("ann");
    let (code, msg) = refused_code(client::play(&forged, Transport::Udp).await);
    assert_eq!(
        (code, msg.contains("signature")),
        (ErrorCode::TicketInvalid, true)
    );
    // A ticket minted ten minutes ago (its life is two).
    let stale = lobby.login("ann", now - 600).expect("ann");
    let (code, msg) = refused_code(client::play(&stale, Transport::Udp).await);
    assert_eq!(
        (code, msg.contains("expired")),
        (ErrorCode::TicketInvalid, true)
    );

    drop((ann, bob));
    handle.stop().await;
    let t = final_tickets(&mut reports);
    assert_eq!(t.accepted(), 2);
    for reason in [
        TicketReason::Signature,
        TicketReason::Expired,
        TicketReason::Game,
    ] {
        assert_eq!(t.rejected(reason), 1, "{reason:?}");
    }
    assert_eq!(t.rejected_total(), 3);
    assert_eq!(t.game_slots(), &[(UNKNOWN_CLASS, 1)]);
}

#[tokio::test]
async fn a_server_built_from_a_ticket_table_validates_the_lobbys_tickets() {
    let key = Lobby::key();
    let trusted = key.trusted();
    let mut cfg = server::config();
    cfg.ticket = Some(
        toml::from_str(&format!(
            "issuer_keys = [\"{}:{}\"]\naudience = \"{}\"",
            trusted.kid(),
            gsb_ticket::keys::hex32(&trusted.public()),
            server::AUDIENCE
        ))
        .expect("a table"),
    );
    // The 2D demo, untouched: the claims pass through untyped.
    let handle = gsb_server::start_server(cfg)
        .await
        .expect("the server starts");
    let udp_key = handle.udp_public_key.expect("a sealed door");
    let (udp, tcp) = (handle.addrs[0], handle.addrs[1]);
    let lobby = Lobby::new(key, server::AUDIENCE, udp, udp_key, tcp);
    let now = gsb_ticket::time::now();
    let ann = lobby.login("ann", now).expect("ann");
    let played = client::play(&ann, Transport::Udp).await.expect("ann plays");
    assert_eq!(played.player, "ann");
    // The same lobby key, another realm's audience.
    let elsewhere = Lobby::new(Lobby::key(), "another-realm", udp, udp_key, tcp);
    let other = elsewhere.login("bob", now).expect("bob");
    let (code, _) = refused_code(client::play(&other, Transport::Tcp).await);
    assert_eq!(code, ErrorCode::TicketInvalid);
    drop(played);
    handle.stop().await;
}
