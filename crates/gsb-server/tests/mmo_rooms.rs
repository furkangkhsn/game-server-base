//! Several MMO worlds on one server (GAME-MODULE G2): every room id is a
//! whole sharded group of its own — `room_count > 1` at startup and a
//! room opened at runtime through the ops surface (`POST /rooms/open`).

#![cfg(feature = "game-mmo")]

mod common;
mod hosted;

use std::net::SocketAddr;
use std::time::Duration;

use gsb_demo_mmo::Realm;
use gsb_server::games::mmo::MmoModule;
use hosted::mmo::{MmoView, dm, ground};
use hosted::{Client, Door, eventually, hold};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

type Mmo = Client<MmoView>;

/// One raw ops-surface request; returns the response text.
async fn http(addr: SocketAddr, request: &str) -> String {
    let mut s = TcpStream::connect(addr).await.expect("ops listener");
    s.write_all(request.as_bytes()).await.expect("request");
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.expect("response");
    String::from_utf8_lossy(&buf).into_owned()
}

/// Two pre-created MMO rooms and a third opened at runtime are three
/// separate worlds: each has its own four shards (a player in one never
/// appears in another, though all of them stand on the same waystone),
/// its own wire-id space, and input moves only its own world.
#[tokio::test]
async fn every_mmo_room_is_a_whole_sharded_world() {
    let mut cfg = Door::Tcp.config("mmo");
    cfg.room_count = 2;
    cfg.http_listen = "127.0.0.1:0".into();
    let handle =
        gsb_server::start_game_server(Box::new(MmoModule::with_realm(Realm::empty())), cfg)
            .await
            .expect("the MMO starts");
    let ops = handle.http_addr.expect("ops surface");

    let mut x: Mmo = Client::join(&Door::Tcp, handle.addr, "x", 1).await;
    let mut y: Mmo = Client::join(&Door::Tcp, handle.addr, "y", 2).await;
    let reply = http(ops, "POST /rooms/open?id=3&tick_hz=30 HTTP/1.1\r\n\r\n").await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.contains("r3 running"), "{reply}");
    let mut z: Mmo = Client::join(&Door::Tcp, handle.addr, "z", 3).await;

    // Each world's shard 0 minted its first player: the same wire id in
    // three id spaces.
    assert_eq!(x.entity, y.entity);
    assert_eq!(y.entity, z.entity);
    let waystone = dm(-256.0, -256.0);
    eventually(
        &mut [&mut x, &mut y, &mut z],
        Duration::from_secs(5),
        "each player sees itself at waystone 0",
        |cs| {
            cs.iter()
                .all(|c| c.me().is_some_and(|r| ground(&r) == waystone))
        },
    )
    .await;
    x.move_to(-240.0, -256.0, 1).await;
    eventually(
        &mut [&mut x, &mut y, &mut z],
        Duration::from_secs(5),
        "X walks in its own world",
        |cs| cs[0].me().is_some_and(|r| ground(&r) == dm(-240.0, -256.0)),
    )
    .await;
    hold(
        &mut [&mut x, &mut y, &mut z],
        Duration::from_millis(500),
        |cs| {
            for c in cs {
                assert_eq!(c.view.players(), vec![c.entity], "one player per world");
            }
            assert_eq!(ground(&cs[1].me().unwrap()), waystone, "Y did not move");
            assert_eq!(ground(&cs[2].me().unwrap()), waystone, "Z did not move");
        },
    )
    .await;
    handle.stop().await;
}
