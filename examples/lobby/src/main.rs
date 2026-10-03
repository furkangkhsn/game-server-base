//! `cargo run -p gsb-example-lobby`: the whole flow in one process —
//! the lobby, the game server, three clients (two let in, one refused by
//! the game's check) and a forged ticket.

use gsb_example_lobby::{client, lobby::Lobby, server};
use gsb_ticket::Transport;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "error".into()))
        .init();
    let key = Lobby::key();
    let (handle, _reports) = server::start(key.trusted()).await;
    let udp_key = handle.udp_public_key.expect("a sealed door");
    let lobby = Lobby::new(
        key,
        server::AUDIENCE,
        handle.addrs[0],
        udp_key,
        handle.addrs[1],
    );
    println!(
        "game server: rUDP {} (sealed), TCP {}",
        handle.addrs[0], handle.addrs[1]
    );

    let now = gsb_ticket::time::now();
    for account in ["ann", "bob", "eve"] {
        let grant = lobby.login(account, now).expect("an account");
        match client::play(&grant, Transport::Udp).await {
            Ok(p) => println!(
                "{account}: joined as `{}`, entity {} at {:?} (its class's side)",
                p.player, p.entity, p.at
            ),
            Err(e) => println!("{account}: refused — {e}"),
        }
    }

    // A forged ticket: signed by a key the server does not trust.
    let forger = Lobby::new(
        Lobby::key(),
        server::AUDIENCE,
        handle.addrs[0],
        udp_key,
        handle.addrs[1],
    );
    let forged = forger.login("ann", now).expect("an account");
    match client::play(&forged, Transport::Udp).await {
        Ok(_) => println!("forged: let in (this must not happen)"),
        Err(e) => println!("forged: refused — {e}"),
    }
    // An expired ticket: minted ten minutes ago.
    let stale = lobby.login("ann", now - 600).expect("an account");
    match client::play(&stale, Transport::Udp).await {
        Ok(_) => println!("expired: let in (this must not happen)"),
        Err(e) => println!("expired: refused — {e}"),
    }
    handle.stop().await;
}
