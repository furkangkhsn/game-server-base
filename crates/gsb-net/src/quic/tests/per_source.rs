//! The per-source handshake cap at the QUIC door (BACKLOG D11). A QUIC
//! address is unproven until a Retry token proves it, so a source at its
//! cap is first asked to prove its address (stateless, no slot): the real
//! owner comes back proven and is counted apart from its unproven
//! connections; proven and still at the cap, it is refused. Another
//! source is unaffected.

use super::*;
use std::time::Duration;

/// Far below the 10 s handshake deadline, far above a local handshake.
const PROMPT: Duration = Duration::from_secs(2);

#[tokio::test]
async fn a_source_at_its_cap_is_retried_then_refused_another_is_served() {
    let pki = mint_pki("per-source");
    let mut transport = transport_for(&pki);
    transport.config.max_handshakes_per_source = Some(1);
    let listener = Arc::new(transport)
        .bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind");
    let bound = listener.local_addr().unwrap();
    let ca = pki.ca_pem.as_bytes();
    let stats = || listener.handshake_stats().expect("a handshaking door");
    let from = |ip: [u8; 4]| SocketAddr::from((ip, 0));

    // Unproven: the first holds its source's unproven slot (no stream:
    // the slot is held to the deadline).
    let _first = tokio::time::timeout(
        PROMPT,
        handshake_from(from([127, 0, 0, 1]), bound, "localhost", ca),
    )
    .await
    .expect("prompt")
    .expect("the first connects");
    assert_eq!(stats().retried_per_source, 0, "{:?}", stats());

    // The second is asked to prove its address, and once proven it is
    // counted apart: it connects.
    let _second = tokio::time::timeout(
        PROMPT,
        handshake_from(from([127, 0, 0, 1]), bound, "localhost", ca),
    )
    .await
    .expect("prompt")
    .expect("the proven second connects");
    let s = stats();
    assert_eq!(
        (s.retried_per_source, s.refused_per_source, s.in_flight),
        (1, 0, 2),
        "{s:?}"
    );

    // The third: retried, proven, and refused at the proven cap.
    let third = tokio::time::timeout(
        PROMPT,
        handshake_from(from([127, 0, 0, 1]), bound, "localhost", ca),
    )
    .await
    .expect("refused at once, not held to the handshake deadline");
    assert!(third.is_err(), "the third is refused");
    let s = stats();
    assert_eq!(
        (s.retried_per_source, s.refused_per_source, s.refused),
        (2, 1, 0),
        "{s:?}"
    );

    // Another source is served while the first holds its cap.
    let _other = tokio::time::timeout(
        PROMPT,
        handshake_from(from([127, 0, 0, 2]), bound, "localhost", ca),
    )
    .await
    .expect("prompt")
    .expect("another source connects");
    let s = stats();
    assert_eq!(
        (s.retried_per_source, s.refused_per_source, s.in_flight),
        (2, 1, 3),
        "{s:?}"
    );
}
