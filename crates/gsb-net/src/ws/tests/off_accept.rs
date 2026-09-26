//! The upgrade runs off the accept loop (BACKLOG B31): a peer that
//! never finishes its upgrade holds one handshake slot, never the door,
//! and a failed upgrade is not an accept error.

use super::*;
use crate::transport::Listener;
use crate::transport::intake::tests::until;

/// Far below the 10 s upgrade deadline, far above a local upgrade.
const PROMPT: Duration = Duration::from_secs(2);

async fn bind(transport: WsTransport) -> Arc<dyn Listener> {
    let transport: Arc<dyn Transport> = Arc::new(transport);
    transport
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind")
}

/// One accept in the background (the server's accept loop).
fn accept_one(listener: &Arc<dyn Listener>) -> tokio::task::JoinHandle<io::Result<SocketAddr>> {
    let l = listener.clone();
    tokio::spawn(async move { l.accept().await.map(|e| e.peer().expect("a peer")) })
}

/// The B29 probe in miniature: a socket that never sends its upgrade
/// request is accepted first, and a real client behind it still
/// upgrades — and reaches the accept loop — at once.
#[tokio::test]
async fn an_idle_peer_does_not_hold_the_door() {
    let listener = bind(WsTransport::default()).await;
    let addr = listener.local_addr().unwrap();
    let _idle = FakeWsClient::raw(addr).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let accepted = accept_one(&listener);
    let client = tokio::time::timeout(PROMPT, FakeWsClient::connect(addr))
        .await
        .expect("the upgrade answers while the idle peer holds its own");
    let peer = tokio::time::timeout(PROMPT, accepted)
        .await
        .expect("the accept loop gets the upgraded peer")
        .expect("no panic")
        .expect("an endpoint, not an error");
    assert_eq!(peer, client.into_stream().local_addr().unwrap());
}

/// A failed upgrade is the client's, not the door's: the accept loop
/// never sees it (no error, no back-off) and gets the next good peer.
#[tokio::test]
async fn a_failed_upgrade_is_not_an_accept_error() {
    let listener = bind(WsTransport::default()).await;
    let addr = listener.local_addr().unwrap();
    let accepted = accept_one(&listener);
    let mut bad = FakeWsClient::raw(addr).await;
    bad.write_all(b"NONSENSE\r\n\r\n").await.unwrap();
    let head = read_http_head(&mut bad).await;
    assert!(head.starts_with("HTTP/1.1 400"), "got: {head}");
    let good = tokio::time::timeout(PROMPT, FakeWsClient::connect(addr))
        .await
        .expect("the good peer upgrades");
    let peer = tokio::time::timeout(PROMPT, accepted)
        .await
        .expect("the accept returns")
        .expect("no panic")
        .expect("the good peer, not the bad one's error");
    assert_eq!(peer, good.into_stream().local_addr().unwrap());
}

/// `close` cuts every upgrade in flight: the silent peer's socket is
/// closed at once, not at the upgrade deadline — with no accept
/// pending at all.
#[tokio::test]
async fn close_cuts_the_upgrades_in_flight() {
    let listener = bind(WsTransport::default()).await;
    let addr = listener.local_addr().unwrap();
    let mut idle = FakeWsClient::raw(addr).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    listener.close();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(PROMPT, idle.read(&mut byte))
        .await
        .expect("the close ended the upgrade in flight");
    assert!(
        matches!(read, Ok(0)) || read.is_err(),
        "EOF or reset, got {read:?}"
    );
}

/// Upgrades run side by side, each in its own task: five silent peers
/// are five upgrades in flight at once.
#[tokio::test]
async fn upgrades_run_side_by_side() {
    let listener = bind(WsTransport::default()).await;
    let addr = listener.local_addr().unwrap();
    let mut silent = Vec::new();
    for _ in 0..5 {
        silent.push(FakeWsClient::raw(addr).await);
    }
    let stats = || listener.handshake_stats().expect("a handshaking door");
    until("five in flight", || stats().in_flight == 5).await;
}

/// Over the bound a connection is closed at once, unupgraded, and
/// counted; a slot its holder gives back serves the next client.
#[tokio::test]
async fn over_the_bound_a_connection_is_refused_and_counted() {
    let listener = bind(WsTransport {
        max_pending_handshakes: 2,
        ..WsTransport::default()
    })
    .await;
    let addr = listener.local_addr().unwrap();
    let stats = || listener.handshake_stats().expect("a handshaking door");
    let first = FakeWsClient::raw(addr).await;
    let _second = FakeWsClient::raw(addr).await;
    until("both in flight", || stats().in_flight == 2).await;

    let mut refused = FakeWsClient::raw(addr).await;
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(PROMPT, refused.read(&mut byte))
        .await
        .expect("refused at once, not held to the upgrade deadline");
    assert!(matches!(read, Ok(0)) || read.is_err(), "got {read:?}");
    let s = stats();
    assert_eq!((s.refused, s.in_flight), (1, 2), "{s:?}");

    // The first holder hangs up: its upgrade fails, its slot is free.
    drop(first);
    until("the slot given back", || stats().in_flight == 1).await;
    let accepted = accept_one(&listener);
    let good = tokio::time::timeout(PROMPT, FakeWsClient::connect(addr))
        .await
        .expect("the freed slot serves the next client");
    let peer = tokio::time::timeout(PROMPT, accepted)
        .await
        .expect("the accept returns")
        .expect("no panic")
        .expect("the good peer");
    assert_eq!(peer, good.into_stream().local_addr().unwrap());
    assert_eq!(stats().failed, 1, "the hang-up counted as a failure");
}
