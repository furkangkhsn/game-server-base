//! The rUDP door's half of `max_handshakes_per_source` (BACKLOG B89). Its
//! cookie exchange holds nothing; what the key caps there is one source's
//! sessions established by a verified proof and not yet taken by the
//! accept loop. The accept loop takes each at once, so a source holds
//! the cap only for a moment: sessions are not capped, and a burst from
//! one source gets in by re-sending the proofs that found the place
//! taken (the demux's own rule and counter are locked in `gsb-net`'s
//! `udp::demux::tests::per_source`).

use std::net::SocketAddr;
use std::time::Duration;

use gsb_client::conn::Conn;
use gsb_client::session::{self, Credentials};
use gsb_server::{Config, TransportKind};

/// Inside the client's 5 s handshake deadline, with room for re-sends.
const PROMPT: Duration = Duration::from_secs(5);

async fn session(to: SocketAddr, name: String) -> Conn {
    let mut c = tokio::time::timeout(
        PROMPT,
        gsb_client::connect::udp(to, crate::common::rudp_pin()),
    )
    .await
    .expect("prompt")
    .expect("the rUDP handshake");
    session::auth(&mut c, &Credentials::named(&name), PROMPT, |_| {})
        .await
        .expect("auth");
    c
}

#[tokio::test]
async fn an_rudp_door_caps_pending_sessions_not_sessions() {
    let cfg = Config {
        bind: "127.0.0.1:0".into(),
        transport: TransportKind::Udp,
        room_count: 1,
        max_handshakes_per_source: Some(1),
        udp_static_key: Some(crate::common::rudp_key().0.clone()),
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg).await.expect("server starts");
    let to = handle.addr;
    // One at a time: each pending session is adopted before the next
    // proof, so a cap of one never refuses — eight sessions, all held.
    let mut held = Vec::new();
    for i in 0..8 {
        held.push(session(to, format!("seq{i}")).await);
    }
    // At once: eight handshakes from the same source; a proof that finds
    // the place taken is re-sent, and every one gets in.
    let burst: Vec<_> = (0..8)
        .map(|i| tokio::spawn(session(to, format!("burst{i}"))))
        .collect();
    for h in burst {
        held.push(h.await.expect("the burst's task"));
    }
    assert_eq!(held.len(), 16);
    drop(held);
    handle.stop().await;
}
