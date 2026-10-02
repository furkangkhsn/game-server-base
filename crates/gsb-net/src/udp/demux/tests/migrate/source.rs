//! A migration carries the session's source (BACKLOG B113), on a demux
//! driven directly: the actor is told the new address with the writer —
//! both or neither — and a pending session's place in the per-source
//! cap (B89) follows it to the new source when that has room, and stays
//! otherwise (counted). Child of `migrate`, so its rig is shared.

use super::*;

/// A candidate at 127.0.0.2: another source than the rig's sockets.
async fn elsewhere() -> UdpSocket {
    UdpSocket::bind("127.0.0.2:0").await.expect("127.0.0.2")
}

/// Validate `to` for the rig's session: a datagram from it, the
/// challenge read where it went, the matching response.
async fn migrate_to(r: &mut Rig, to: &UdpSocket) {
    let at = addr(to);
    feed(&mut r.d, at, &raw(b"moving"));
    let nonce = nonce_of(&got(to).await.expect("challenged"));
    feed(&mut r.d, at, &encode_path_response(CID, nonce));
}

/// The rig's next actor message that is not a frame.
fn notice(r: &mut Rig) -> Option<ConnIn> {
    loop {
        match r.inbox.try_recv() {
            Ok(ConnIn::Frame(_)) => continue,
            Ok(other) => return Some(other),
            Err(_) => return None,
        }
    }
}

/// The moved session's actor hears its new address, behind the frames
/// that came before the move; the writer hears it too.
#[tokio::test]
async fn a_migration_tells_the_actor_the_new_peer() {
    let mut r = rig(4).await;
    let b = elsewhere().await;
    migrate_to(&mut r, &b).await;
    assert_eq!(r.d.sessions.key_at(&addr(&b)), Some(r.key), "moved");
    assert_eq!(
        r.frame().as_deref(),
        Some(&b"moving"[..]),
        "the frame first"
    );
    match notice(&mut r) {
        Some(ConnIn::PeerChanged { peer }) => assert_eq!(peer, addr(&b)),
        other => panic!("expected the peer change, got {other:?}"),
    }
    assert!(r.outbox.try_recv().is_ok(), "and the writer's notice");
    assert_eq!(r.d.mig.migrations_port_only, 0, "another IP");
}

/// An actor inbox that is full holds the move back like a full writer
/// channel: neither is told, nothing moves (counted), and the next
/// response, once there is room, moves it and tells both.
#[tokio::test]
async fn a_full_actor_inbox_defers_the_move() {
    let mut r = rig(4).await;
    let b = elsewhere().await;
    let at = addr(&b);
    feed(&mut r.d, at, &raw(b"x"));
    let nonce = nonce_of(&got(&b).await.unwrap());
    let inbox = r.d.sessions.get(r.key).unwrap().in_tx.clone();
    while inbox.try_send(ConnIn::Shutdown).is_ok() {} // the actor is behind
    feed(&mut r.d, at, &encode_path_response(CID, nonce));
    assert_eq!(r.d.mig.changes_not_forwarded, 1);
    assert_eq!(r.d.sessions.key_at(&at), None, "not moved");
    assert!(
        r.outbox.try_recv().is_err(),
        "the writer was not told either"
    );
    while r.inbox.try_recv().is_ok() {}
    feed(&mut r.d, at, &encode_path_response(CID, nonce));
    assert_eq!(r.d.sessions.key_at(&at), Some(r.key));
    assert!(matches!(notice(&mut r), Some(ConnIn::PeerChanged { .. })));
    assert!(r.outbox.try_recv().is_ok());
}

/// A pending session's place moves to the new source; given back, it
/// leaves the new source's count.
#[tokio::test]
async fn a_pending_session_s_place_follows_it() {
    let mut r = rig(4).await;
    r.d.per_source = crate::udp::demux::source::PerSource::new(Some(1));
    let home = addr(&r.a).ip();
    let pending = r.d.per_source.claim(r.key, home);
    let b = elsewhere().await;
    migrate_to(&mut r, &b).await;
    assert_eq!(
        r.d.per_source.pending_from(home),
        0,
        "the old source is free"
    );
    assert_eq!(r.d.per_source.pending_from(addr(&b).ip()), 1);
    drop(pending);
    assert_eq!(r.d.per_source.sources(), 0, "given back at the new source");
    assert_eq!(r.d.per_source.moves_kept, 0);
}

/// Into a source at its cap, the move happens but the place stays where
/// it was (counted): no source ever holds more than the cap.
#[tokio::test]
async fn into_a_full_source_the_place_stays() {
    let mut r = rig(4).await;
    r.d.per_source = crate::udp::demux::source::PerSource::new(Some(1));
    let home = addr(&r.a).ip();
    let b = elsewhere().await;
    let _mine = r.d.per_source.claim(r.key, home);
    let _theirs = r.d.per_source.claim(SessionKey(u64::MAX), addr(&b).ip());
    migrate_to(&mut r, &b).await;
    assert_eq!(r.d.sessions.key_at(&addr(&b)), Some(r.key), "moved anyway");
    assert_eq!(r.d.per_source.moves_kept, 1);
    assert_eq!(r.d.per_source.pending_from(home), 1, "kept at home");
    assert_eq!(r.d.per_source.pending_from(addr(&b).ip()), 1);
}
