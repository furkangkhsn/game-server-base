//! The reap pass on a demux driven directly (BACKLOG B6): a queued
//! address frees its session only when the session is really dead, and
//! the wake datagram is known by its source.

use super::*;

/// A live session for `peer`: both channel ends it would have in a
/// server are returned, so the test decides when each side is gone, and
/// its key (what its writer's reaper names).
fn install(
    d: &mut Demux,
    peer: SocketAddr,
) -> (
    gsb_core::channel::Inbox<ConnIn>,
    gsb_core::channel::Inbox<gsb_core::channel::FrameBatch>,
    SessionKey,
) {
    let (in_tx, in_rx) = gsb_core::channel::channel(4);
    let (out_tx, out_rx) = gsb_core::channel::channel(4);
    let now = Instant::now();
    let key = d
        .sessions
        .insert(UdpSession::new(peer, None, in_tx, out_tx, now));
    if let Some(idle) = d.idle {
        d.deadlines.insert((now + idle, key));
    }
    (in_rx, out_rx, key)
}

async fn bound() -> Arc<UdpSocket> {
    Arc::new(
        UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .expect("bind"),
    )
}

/// A signal names a session, it does not decide: a live session stays;
/// once its actor is gone the same signal frees it — its idle deadline
/// with it.
#[tokio::test]
async fn a_signal_frees_only_a_dead_session() {
    let (mut d, _end_rx) = demux_bare(bound().await);
    d.idle = Some(Duration::from_secs(30));
    let peer: SocketAddr = "127.0.0.1:40001".parse().unwrap();
    let (in_rx, _out_rx, key) = install(&mut d, peer);

    assert!(d.reaper.session(key).signal().await, "queued");
    d.reap();
    assert!(d.sessions.contains_key(&peer), "a live session stays");
    assert_eq!(d.reaped, 0);

    drop(in_rx); // the actor exits
    assert!(d.reaper.session(key).signal().await, "queued");
    d.reap();
    assert!(!d.sessions.contains_key(&peer), "the dead session is freed");
    assert!(d.deadlines.is_empty(), "with its idle deadline");
    assert_eq!(d.reaped, 1);
}

/// A session whose WRITER is gone (the REL band died) is dead too, even
/// before its actor has finished tearing down.
#[tokio::test]
async fn a_session_without_a_writer_is_freed() {
    let (mut d, _end_rx) = demux_bare(bound().await);
    let peer: SocketAddr = "127.0.0.1:40002".parse().unwrap();
    let (_in_rx, out_rx, key) = install(&mut d, peer);
    drop(out_rx);
    assert!(d.reaper.session(key).signal().await);
    d.reap();
    assert!(!d.sessions.contains_key(&peer));
}

/// The wake comes from the demux's own address and nowhere else, and
/// it reaches the socket (the demux's one awaited source).
#[tokio::test]
async fn the_wake_is_a_datagram_from_the_demux_s_own_address() {
    let sock = bound().await;
    let (d, _end_rx) = demux_bare(sock.clone());
    let peer: SocketAddr = "127.0.0.1:40003".parse().unwrap();
    assert!(d.reaper.clone().signal().await);
    let mut buf = [0u8; 16];
    let (_, from) = tokio::time::timeout(Duration::from_secs(1), sock.recv_from(&mut buf))
        .await
        .expect("the wake arrives")
        .expect("recv");
    assert!(d.is_wake(from), "known by its source");
    assert!(!d.is_wake(peer), "a peer is never a wake");
    assert_eq!(
        super::super::reap::wake_addr("0.0.0.0:7000".parse().unwrap()),
        "127.0.0.1:7000".parse().unwrap(),
        "an unspecified bind is reached on loopback"
    );
    assert_eq!(
        super::super::reap::wake_addr("[::]:7000".parse().unwrap()),
        "[::1]:7000".parse().unwrap()
    );
}
