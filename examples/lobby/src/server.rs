//! The game server: a sealed rUDP door and a TCP door, the class game,
//! and the ticket hook — the lobby's key trusted, the game's check on.

use std::time::Duration;

use gsb_core::metrics::MetricReport;
use gsb_server::{Config, ListenerEntry, ListenerTransport, ServerHandle, ServerHooks};
use gsb_ticket::{TrustedKey, Validator};
use tokio::sync::mpsc;

use crate::game::{self, ClassModule, Loadout};

/// The realm the lobby mints tickets for.
pub const AUDIENCE: &str = "example-realm";

/// The ticket hook's deadline (validation is local: an Ed25519 check).
pub const TICKET_TIMEOUT: Duration = Duration::from_secs(2);

fn door(transport: ListenerTransport) -> ListenerEntry {
    ListenerEntry {
        transport,
        bind: "127.0.0.1:0".into(),
        tls_cert: None,
        tls_key: None,
    }
}

/// The server's config: a sealed rUDP door under a key of its own (its
/// public half is `ServerHandle::udp_public_key`, what the lobby hands
/// out) and a plain TCP door, one room.
pub fn config() -> Config {
    let (key, _public) = gsb_server::ephemeral_udp_key().expect("OS entropy");
    Config {
        listeners: Some(vec![
            door(ListenerTransport::Udp),
            door(ListenerTransport::Tcp),
        ]),
        udp_static_key: Some(key),
        room_count: 1,
        ..Default::default()
    }
}

/// Start the server trusting `lobby`'s key; the metric reports come out
/// on the returned receiver.
pub async fn start(lobby: TrustedKey) -> (ServerHandle, mpsc::UnboundedReceiver<MetricReport>) {
    let validator = Validator::<Loadout>::new(AUDIENCE, [lobby])
        .expect("one trusted key")
        .with_check(game::check);
    let hooks = ServerHooks {
        ticket: Some(validator.into_auth(TICKET_TIMEOUT)),
    };
    let (reports_tx, reports) = mpsc::unbounded_channel();
    let handle = gsb_server::start_game_server_with(
        Box::new(ClassModule),
        config(),
        hooks,
        Some(reports_tx),
    )
    .await
    .expect("the server starts");
    (handle, reports)
}
