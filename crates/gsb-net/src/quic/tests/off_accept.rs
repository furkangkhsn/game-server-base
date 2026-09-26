//! The QUIC handshake — and the wait for the client's bi-stream — run
//! off the accept loop (BACKLOG B31): a peer that connects and never
//! opens its stream holds one handshake slot, never the door.

use super::*;
use std::time::Duration;

/// Far below the 10 s handshake deadline, far above a local handshake.
const PROMPT: Duration = Duration::from_secs(2);

/// A peer that completes the QUIC handshake but never opens the stream
/// is accepted first; a real client behind it still connects, and its
/// endpoint reaches the accept loop, at once.
#[tokio::test]
async fn a_peer_without_its_stream_does_not_hold_the_door() {
    let pki = mint_pki("off-accept");
    let listener = Arc::new(transport_for(&pki))
        .bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind");
    let bound = listener.local_addr().unwrap();
    let ca = pki.ca_pem.as_bytes();
    let l = listener.clone();
    let accepted = tokio::spawn(async move { l.accept().await.map(|e| e.peer()) });
    let _silent = tokio::time::timeout(PROMPT, handshake_only(bound, "localhost", ca))
        .await
        .expect("the handshake itself is prompt")
        .expect("a verified connection");
    let (_reader, mut writer) = tokio::time::timeout(PROMPT, connect(bound, "localhost", ca))
        .await
        .expect("the connect completes while the silent peer holds its slot")
        .expect("connect");
    // QUIC opens a stream lazily: the first frame is what puts it on
    // the wire, so the door can only see it once something is written.
    writer
        .send(FrameBody::new(7, b"hello".as_slice()))
        .await
        .expect("client send");
    let peer = tokio::time::timeout(PROMPT, accepted)
        .await
        .expect("the accept loop gets the streaming peer")
        .expect("no panic")
        .expect("an endpoint, not an error");
    assert!(peer.is_some());
}

/// Over the bound a new connection is refused on the spot (quinn's
/// `refuse`: no handshake) and counted.
#[tokio::test]
async fn over_the_bound_a_connection_is_refused_and_counted() {
    let pki = mint_pki("bound");
    let mut transport = transport_for(&pki);
    transport.config.max_pending_handshakes = 1;
    let listener = Arc::new(transport)
        .bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind");
    let bound = listener.local_addr().unwrap();
    let ca = pki.ca_pem.as_bytes();
    let _holder = tokio::time::timeout(PROMPT, handshake_only(bound, "localhost", ca))
        .await
        .expect("prompt")
        .expect("the slot holder connects");
    let refused = tokio::time::timeout(PROMPT, handshake_only(bound, "localhost", ca))
        .await
        .expect("refused at once, not held to the handshake deadline");
    assert!(refused.is_err(), "the second connection is refused");
    let s = listener.handshake_stats().expect("a handshaking door");
    assert_eq!((s.refused, s.in_flight), (1, 1), "{s:?}");
}
