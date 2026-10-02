//! B5b: a server restart. The rUDP door goes away (rUDP has no FIN: the
//! client is not told), a second server under the same static and reset
//! keys binds the same address, and the client's next datagram brings
//! back a stateless reset with its session's token: the client's session
//! ends at once — not after the reliable band's 5 s bound — and it comes
//! back the e1 way (a new socket, a new handshake, AUTH under the same
//! name).

use std::time::{Duration, Instant};

use super::player::Player;
use super::rig::{Door, GUARD, Rig, Shape, config};

/// Start `cfg` at the address a stopped server just left: retried while
/// its socket is still going.
async fn restart(cfg: gsb_server::Config) -> Rig {
    let deadline = Instant::now() + GUARD;
    loop {
        match Rig::try_start(cfg.clone()).await {
            Ok(rig) => return rig,
            Err(e) if Instant::now() < deadline => {
                assert!(e.to_string().contains("did not start"), "{e}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("the restart never bound: {e}"),
        }
    }
}

pub async fn restart_resets_then_rejoins() {
    let (static_key, _) = gsb_server::ephemeral_udp_key().expect("entropy");
    let (reset_key, _) = gsb_server::ephemeral_udp_key().expect("entropy");
    let mut cfg = config(Door::Udp, Shape::Single, 0.0);
    cfg.udp_static_key = Some(static_key);
    cfg.udp_reset_key = Some(reset_key);
    let first = Rig::start(cfg.clone()).await;
    let addr = first.addr();
    let mut p = Player::connect(Door::Udp, addr, first.key()).await;
    p.join("ann").await;
    p.position().await;
    first.stop().await;

    cfg.bind = addr.to_string();
    let mut second = restart(cfg).await;
    let took = p.until_ended().await;
    assert!(took < Duration::from_secs(1), "ended in {took:?}");
    let s = p.udp_stats();
    assert!(s.stateless_resets_received >= 1, "{s:?}");
    assert_eq!(s.stateless_resets_invalid, 0, "the same reset key");

    // Back the e1 way: a new connection, the same name. The restarted
    // server never knew `ann`: a fresh join.
    let mut back = Player::connect(Door::Udp, addr, second.key()).await;
    assert_ne!(back.join("ann").await, 0);
    back.position().await;
    let seen = second
        .until("the reset counted, the rejoin seen", |s| {
            s.resets_sent >= 1 && s.room.joins == 1
        })
        .await;
    assert_eq!(seen.room.resumes, 0, "nothing to resume on a new server");
    second.stop().await;
}
