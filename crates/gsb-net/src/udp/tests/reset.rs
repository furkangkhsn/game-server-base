//! A restart over real sockets (B5b): a sealed door goes away without a
//! word (no FIN in UDP), a new door under the same static key binds the
//! same address, and the client's next datagram brings back a stateless
//! reset carrying its session's token — the client's session ends at
//! once, not after the reliable band's 5 s bound, and the restarted door
//! takes the client's new session (the resume path's first step).

use std::time::Instant;

use super::sealed::{pinned, sealed_config};
use super::*;
use gsb_protocol::op;

/// Bind `config` at `addr` — retried while the previous door's socket is
/// still going (its tasks release it as they end).
async fn bind_at(config: UdpTransportConfig, addr: SocketAddr) -> Arc<dyn Listener> {
    let transport = Arc::new(UdpTransport { config });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Arc::clone(&transport).bind(addr).await {
            Ok(l) => return l,
            Err(e) if Instant::now() < deadline => {
                assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse, "{e}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("the old door never let go of {addr}: {e}"),
        }
    }
}

#[tokio::test]
async fn a_restarted_door_resets_the_lost_session_at_once() {
    let (cfg, key) = sealed_config();
    let (listener, addr, mut eps, accept) = bound_transport(cfg.clone()).await;
    let mut c = UdpClient::connect_with(addr, pinned(key)).await.unwrap();
    let ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("an endpoint")
        .expect("an endpoint");
    // The server goes away: no word to the client.
    drop(ep);
    listener.close();
    accept.abort();
    let _ = accept.await;
    drop((listener, eps));
    let restarted = bind_at(cfg, addr).await;

    let t0 = Instant::now();
    c.send_frame(op::base::HEARTBEAT, Bytes::from_static(b"up"))
        .await
        .unwrap();
    while c.is_established() {
        assert!(t0.elapsed() < Duration::from_secs(3), "no reset arrived");
        c.recv_frame(Duration::from_millis(20)).await.unwrap();
    }
    let took = t0.elapsed();
    assert!(took < REL_NO_ACK_FATAL / 5, "ended in {took:?}");
    assert_eq!(c.stats.stateless_resets_received, 1);
    assert_eq!(c.stats.stateless_resets_invalid + c.stats.seal_forged, 0);

    // The client starts over: a new session at the restarted door.
    let c2 = UdpClient::connect_with(addr, pinned(key))
        .await
        .expect("a new session at the restarted door");
    assert!(c2.is_established() && c2.sealed());
    restarted.close();
}
