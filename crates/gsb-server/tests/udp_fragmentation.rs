//! An MMO-sized snapshot over rUDP, end to end: the REAL server (the
//! MMO's four shard actors, the registry, the rUDP demux and writers)
//! and real rUDP clients, the observer applying what it receives with the
//! kit's reference client (`gsb_kit::client::ClientView`, via the hosted
//! MMO view).
//!
//! 120 players stand on the default waystone, so a late joiner's
//! one-shot private full carries ~120 records — about 2 KB, over the
//! 1472-byte datagram budget. Before the transport fragmented, that full
//! (and every keep-alive full after it) was dropped by the writer: the
//! observer never had a baseline, dropped every delta, and saw nobody.
//! Now the full travels as FRAG datagrams and the view holds every
//! player — with the kit envelope and the core's one-payload-per-group
//! contract untouched.

#![cfg(feature = "game-mmo")]

mod common;
mod hosted;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use gsb_client::session::{self, Credentials};
use gsb_client::{Conn, Recv};
use gsb_demo_mmo::Realm;
use gsb_server::games::mmo::MmoModule;
use hosted::View;
use hosted::mmo::MmoView;

const PLAYERS: usize = 120;

/// Connect, authenticate as `name`, join room 1; returns the client, its
/// wire id, and the game frames that arrived before the join result.
async fn join(addr: SocketAddr, name: String) -> (Conn, u64, Vec<(u16, Vec<u8>)>) {
    let mut c = gsb_client::connect::udp(addr)
        .await
        .expect("rUDP handshake");
    let mut early = Vec::new();
    let joined = session::auth_and_join(
        &mut c,
        &Credentials::named(name.clone()),
        1,
        Duration::from_secs(10),
        |f| {
            if f.op >= 1000 {
                early.push((f.op, f.payload.to_vec()));
            }
        },
    )
    .await
    .unwrap_or_else(|e| panic!("{name}: no join result over rUDP: {e}"));
    (c, joined.entity, early)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_mmo_sized_full_reaches_the_kit_client_view_over_rudp() {
    let cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        game: "mmo".into(),
        transport: gsb_server::TransportKind::Udp,
        ..Default::default()
    };
    let handle =
        gsb_server::start_game_server(Box::new(MmoModule::with_realm(Realm::empty())), cfg)
            .await
            .expect("the MMO starts on rUDP");
    let addr = handle.addr;

    // The crowd: joined in small concurrent waves (a handshake burst of
    // hundreds would overflow the server socket's receive queue on
    // loopback — not what this test is about). Kept alive, never read:
    // after its join nothing reliable is owed to a crowd client.
    let mut crowd = Vec::new();
    let mut ids = Vec::new();
    for wave in 0..PLAYERS / 20 {
        let joins: Vec<_> = (0..20)
            .map(|i| tokio::spawn(join(addr, format!("crowd-{wave}-{i}"))))
            .collect();
        for j in joins {
            let (c, id, _) = j.await.expect("join task");
            ids.push(id);
            crowd.push(c);
        }
    }

    // The observer: its one-shot private full is the whole crowd.
    let (mut me, my_id, early) = join(addr, "observer".into()).await;
    ids.push(my_id);
    ids.sort_unstable();
    let mut view = MmoView::default();
    for (o, p) in &early {
        view.apply(*o, p);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while view.players() != ids && Instant::now() < deadline {
        if let Recv::Frame(f) = me.recv(Duration::from_millis(100)).await.expect("recv")
            && f.op >= 1000
        {
            view.apply(f.op, &f.payload);
        }
    }
    assert!(view.has_baseline(), "the full arrived and was applied");
    assert_eq!(
        view.players(),
        ids,
        "every player is in the observer's kit view"
    );
    let stats = &me.udp_client().expect("an rUDP client").stats;
    assert!(
        stats.frag_reassembled >= 1,
        "the full travelled fragmented (else this test proves nothing)"
    );
    assert_eq!(stats.frag_rejected, 0);
    drop(crowd);
    handle.stop().await;
}
