//! A sealed door over real sockets (B5a): the client that pinned the
//! server's key gets a sealed session both ways; the old/new matrix —
//! a plaintext client at a sealed door, a sealed client at a plaintext
//! door, a client pinning another key — each ends in a clear, counted
//! refusal; a sealed session migrates.

use super::*;
use crate::seal::StaticKey;
use gsb_protocol::op;

/// A sealed transport config, and the public key its clients pin.
pub(super) fn sealed_config() -> (UdpTransportConfig, [u8; 32]) {
    let key = Arc::new(StaticKey::generate().expect("a key"));
    let public = key.public();
    let cfg = UdpTransportConfig {
        security: UdpSecurity::Sealed(key),
        migration: true,
        ..UdpTransportConfig::default()
    };
    (cfg, public)
}

/// A client config pinning `key`.
pub(super) fn pinned(key: [u8; 32]) -> UdpClientConfig {
    UdpClientConfig {
        server_key: Some(key),
        ..UdpClientConfig::default()
    }
}

/// Drive the endpoint's writer: its inbox and outbox.
fn pump(
    mut ep: Endpoint,
) -> (
    gsb_core::channel::Inbox<gsb_core::conn::ConnIn>,
    gsb_core::channel::Mailbox<gsb_core::channel::FrameBatch>,
) {
    let (in_tx, in_rx) = ep.take_inbox(16);
    let (out_tx, out_rx) = ep.take_outbox(16);
    let (_r, _w) = ep.start_pump(
        ConnectionId(9),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    (in_rx, out_tx)
}

/// The sealed round trip: the client's control and game frames reach the
/// server's actor; the server's control frame reaches the client; the
/// client is sealed and, holding the CID from message 2, migratable.
#[tokio::test]
async fn a_pinned_client_gets_a_sealed_session_both_ways() {
    let (cfg, key) = sealed_config();
    let (listener, addr, mut eps, _accept) = bound_transport(cfg).await;
    let mut c = UdpClient::connect_with(addr, pinned(key))
        .await
        .expect("the sealed handshake");
    assert!(c.sealed() && c.migratable());
    let ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("an endpoint")
        .expect("an endpoint");
    let (mut inbox, outbox) = pump(ep);

    c.send_frame(op::base::HEARTBEAT, Bytes::from_static(b"up"))
        .await
        .unwrap();
    c.send_frame(1000, Bytes::from_static(b"move"))
        .await
        .unwrap();
    for want in [&b"up"[..], &b"move"[..]] {
        match tokio::time::timeout(Duration::from_secs(3), inbox.recv()).await {
            Ok(Some(gsb_core::conn::ConnIn::Frame(f))) => assert_eq!(f.payload.as_ref(), want),
            other => panic!("expected a frame, got {other:?}"),
        }
    }
    let fb = FrameBody::new(op::base::HEARTBEAT_ACK, Bytes::from_static(b"down"));
    outbox.send(vec![fb]).await.unwrap();
    let got = c
        .recv_frame(Duration::from_secs(3))
        .await
        .unwrap()
        .expect("the server's frame");
    assert_eq!(got.payload.as_ref(), b"down");
    assert_eq!(c.stats.seal_forged + c.stats.unsealed_dropped, 0);
    listener.close();
}

/// A plaintext client at a sealed door: refused at the handshake (the
/// server counts `udp_proofs_refused_plaintext`), no endpoint — the
/// client's handshake times out.
#[tokio::test]
async fn a_plaintext_client_at_a_sealed_door_is_refused() {
    let (cfg, _key) = sealed_config();
    let (listener, addr, mut eps, _accept) = bound_transport(cfg).await;
    let e = UdpClient::connect_within(addr, Duration::from_millis(600))
        .await
        .err()
        .expect("refused");
    assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
    assert!(eps.try_recv().is_err(), "no session");
    listener.close();
}

/// A sealed client at a plaintext door: it pinned a key and got a
/// plaintext accept (no message 2) — refused, counted, and the give-up
/// says what happened: `ConnectionRefused`.
#[tokio::test]
async fn a_sealed_client_at_a_plaintext_door_is_refused() {
    let (listener, addr, _eps, _accept) = bound_transport(UdpTransportConfig::default()).await;
    let key = StaticKey::generate().unwrap().public();
    let e = UdpClient::connect_within_with(addr, Duration::from_millis(600), pinned(key))
        .await
        .err()
        .expect("refused");
    assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused, "{e}");
    listener.close();
}

/// A client pinning another server's key: the server's DH fails (counted
/// `udp_handshakes_failed_decrypt`), nothing answers, the client times out.
#[tokio::test]
async fn a_client_pinning_another_key_times_out() {
    let (cfg, _key) = sealed_config();
    let (listener, addr, mut eps, _accept) = bound_transport(cfg).await;
    let other = StaticKey::generate().unwrap().public();
    let e = UdpClient::connect_within_with(addr, Duration::from_millis(600), pinned(other))
        .await
        .err()
        .expect("refused");
    assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
    assert!(eps.try_recv().is_err());
    listener.close();
}

/// A sealed session survives the client's new socket: the sealed nudge
/// from the new address is authenticated and the newest, the server's
/// sealed challenge is answered, and the server's next frame arrives at
/// the new socket.
#[tokio::test]
async fn a_sealed_session_migrates_to_a_new_socket() {
    let (cfg, key) = sealed_config();
    let (listener, addr, mut eps, _accept) = bound_transport(cfg).await;
    let mut c = UdpClient::connect_with(addr, pinned(key)).await.unwrap();
    let ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .unwrap()
        .unwrap();
    let (_inbox, outbox) = pump(ep);
    let old = c.local_addr().unwrap();
    let new = c.rebind().await.expect("rebind");
    assert_ne!(old, new);
    // Read until the challenge is answered and the server sends to us.
    let fb = FrameBody::new(1000, Bytes::from_static(b"here"));
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut got = None;
    while got.is_none() && std::time::Instant::now() < deadline {
        outbox.send(vec![fb.clone()]).await.unwrap();
        got = c.recv_frame(Duration::from_millis(100)).await.unwrap();
    }
    assert_eq!(
        got.expect("a frame at the new socket").payload.as_ref(),
        b"here"
    );
    assert!(c.stats.path_challenges_answered >= 1);
    listener.close();
}
