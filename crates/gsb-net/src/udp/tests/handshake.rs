//! The handshake under loss: every datagram of it can be dropped, and
//! the client either ends up with a session the server confirmed or
//! with a clean error — never with a session it only believes in.
//! A lossy relay sits between a real `UdpClient` and a real transport
//! and drops datagrams by rule, deterministically. Child of `tests`, so
//! `bound_transport` is shared.

use super::*;
use tokio::sync::watch;

/// A drop rule: sees each datagram of one direction, returns `true` to
/// drop it. Stateful, so "the first proof" is expressible.
type Rule = Box<dyn FnMut(&[u8]) -> bool + Send>;

/// A one-client relay: the client talks to the returned address, the
/// server sees the relay's back socket as the peer. Each direction is
/// its own task (one awaited source each).
async fn relay(server: SocketAddr, mut drop_c2s: Rule, mut drop_s2c: Rule) -> SocketAddr {
    let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("front"));
    let back = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("back"));
    let addr = front.local_addr().expect("front addr");
    let (client_tx, client_rx) = watch::channel(None::<SocketAddr>);
    let (f, b) = (Arc::clone(&front), Arc::clone(&back));
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok((n, from)) = f.recv_from(&mut buf).await {
            let _ = client_tx.send(Some(from));
            if !drop_c2s(&buf[..n]) {
                let _ = b.send_to(&buf[..n], server).await;
            }
        }
    });
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok((n, _)) = back.recv_from(&mut buf).await {
            let client = *client_rx.borrow();
            if let Some(client) = client
                && !drop_s2c(&buf[..n])
            {
                let _ = front.send_to(&buf[..n], client).await;
            }
        }
    });
    addr
}

fn keep() -> Rule {
    Box::new(|_| false)
}

/// The client's proof: a HELLO carrying a cookie.
fn is_proof(d: &[u8]) -> bool {
    d.len() >= 18 && d[0] == KIND_HELLO && d[9..17] != [0u8; 8]
}

/// Drop the first `n` datagrams that match `what`.
fn first(n: usize, what: fn(&[u8]) -> bool) -> Rule {
    let mut seen = 0;
    Box::new(move |d| {
        if what(d) && seen < n {
            seen += 1;
            return true;
        }
        false
    })
}

/// `connect`, bounded so a broken handshake fails the test, not hangs.
async fn healed(via: SocketAddr) -> UdpClient {
    tokio::time::timeout(HANDSHAKE_DEADLINE * 2, UdpClient::connect(via))
        .await
        .expect("the handshake must end")
        .expect("the handshake heals")
}

/// No endpoint arrives within `ms`.
async fn no_endpoint(eps: &mut mpsc::UnboundedReceiver<Endpoint>, ms: u64) -> bool {
    tokio::time::timeout(Duration::from_millis(ms), eps.recv())
        .await
        .is_err()
}

/// THE BUG: a proof lost on the way to the server. The client must not
/// walk away believing in a session the server never created: it
/// re-sends the proof, and only returns once the server has one.
#[tokio::test]
async fn a_lost_proof_is_retransmitted_and_the_session_exists() {
    let (_listener, addr, mut eps, _accept) = bound_transport(UdpTransportConfig::default()).await;
    let via = relay(addr, first(1, is_proof), keep()).await;

    let c = healed(via).await;
    let ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("the server must hold a session for a connected client")
        .expect("endpoint");
    assert!(c.is_established());
    assert!(ep.peer().is_some());
    assert_eq!(c.stats.challenge_retries, 0);
    assert!(c.stats.proof_retries >= 1, "the proof was re-sent");
    assert!(no_endpoint(&mut eps, 300).await, "exactly one session");
}

/// A challenge lost on the way back: the request is re-sent (this
/// already healed before the fix; pinned so the rework keeps it).
#[tokio::test]
async fn a_lost_challenge_is_requested_again() {
    let (_listener, addr, mut eps, _accept) = bound_transport(UdpTransportConfig::default()).await;
    let via = relay(addr, keep(), first(1, |d| d[0] == KIND_HELLO)).await;

    let c = healed(via).await;
    tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("one session")
        .expect("endpoint");
    assert!(c.is_established());
    assert!(c.stats.challenge_retries >= 1, "the request was re-sent");
    assert_eq!(c.stats.proof_retries, 0);
    assert!(no_endpoint(&mut eps, 300).await, "exactly one session");
}

/// The accept lost on the way back: the server HAS the session, the
/// client does not know it. The client re-sends the proof; the server
/// answers a known peer's valid proof with the accept again — one
/// session, one endpoint (the accept loop's one `ConnectionId`).
#[tokio::test]
async fn a_lost_accept_is_healed_without_a_second_session() {
    let (_listener, addr, mut eps, _accept) = bound_transport(UdpTransportConfig::default()).await;
    let via = relay(addr, keep(), first(2, |d| d[0] == KIND_ACK)).await;

    let c = healed(via).await;
    tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("one session")
        .expect("endpoint");
    assert!(c.is_established());
    assert!(
        c.stats.proof_retries >= 2,
        "two accepts lost, two proofs re-sent: {}",
        c.stats.proof_retries
    );
    assert!(
        no_endpoint(&mut eps, 300).await,
        "a re-sent proof must never become a second session"
    );
}

/// Give-up: a path that never delivers a proof ends the handshake with
/// a clean `TimedOut` at the bound — not with a client that believes it
/// is connected — and the server never allocated anything.
///
/// "At the bound, not sooner or much later" without a wall-clock window
/// (BACKLOG F34): not sooner is the elapsed time (a stall only lengthens
/// it); not later is the proofs the relay swallowed. The proof step's
/// timer starts no lower than the floor and doubles per re-send up to
/// the ceiling (B2, "Retransmit timer"), and every proof leaves before
/// the deadline, so a client that gives up at the bound sends at most
/// as many as that schedule fits in `HANDSHAKE_DEADLINE` (nine) — a
/// stall can only make it fewer; a client that kept re-sending past the
/// bound (a deadline restarted per step, a last step overrunning it), or
/// on a timer that never backed off (a hundred), sends more. The outer
/// timeout is the hang guard.
#[tokio::test]
async fn a_proof_that_never_lands_gives_up_with_a_clean_error() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (_listener, addr, mut eps, _accept) = bound_transport(UdpTransportConfig::default()).await;
    let proofs = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&proofs);
    let swallow: Rule = Box::new(move |d| {
        let proof = is_proof(d);
        if proof {
            seen.fetch_add(1, Ordering::SeqCst);
        }
        proof
    });
    let via = relay(addr, swallow, keep()).await;

    let t0 = std::time::Instant::now();
    let bounded = tokio::time::timeout(HANDSHAKE_DEADLINE * 2, UdpClient::connect(via));
    let err = match bounded.await.expect("the handshake must give up, not hang") {
        Ok(_) => panic!("a client whose proof never landed must not be connected"),
        Err(e) => e,
    };
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
    let took = t0.elapsed();
    assert!(
        took >= HANDSHAKE_DEADLINE,
        "gave up before the bound: {took:?}"
    );
    assert!(err.to_string().contains("never accepted"), "{err}");
    // Counted as the relay reads them: read after the window below, which
    // gives the relay's task a turn to count the last one.
    assert!(no_endpoint(&mut eps, 100).await, "no session, no zombie");
    let sent = proofs.load(Ordering::SeqCst);
    // The fastest schedule: sends at 0, then after 50, 100, 200 … ms.
    let (mut most, mut at, mut step) = (0u128, Duration::ZERO, crate::udp::rel::MIN_RTO);
    while at < HANDSHAKE_DEADLINE {
        most += 1;
        at += step;
        step = (step * 2).min(MAX_RTO);
    }
    assert!(
        sent >= 1 && sent as u128 <= most,
        "{sent} proofs in {took:?}: at least one, at most {most} before the bound"
    );
}

/// Give-up on the OTHER side of the table: every accept is lost, so the
/// server holds a session the client will never believe in. The client
/// still ends with a clean `TimedOut` at its bound (the seam shortens
/// it), and the server's session is not a zombie: nothing ever arrives
/// for it, so the idle sweep ends it through the actor's mailbox.
#[tokio::test]
async fn an_accept_that_never_lands_gives_up_and_the_server_session_is_swept() {
    let cfg = UdpTransportConfig {
        idle_timeout: Some(Duration::from_millis(1500)),
        ..Default::default()
    };
    let (_listener, addr, mut eps, _accept) = bound_transport(cfg).await;
    let via = relay(addr, keep(), Box::new(|d| d[0] == KIND_ACK)).await;

    let within = Duration::from_millis(700);
    let bounded = tokio::time::timeout(within * 3, UdpClient::connect_within(via, within));
    let err = match bounded.await.expect("the handshake must give up, not hang") {
        Ok(_) => panic!("a client that never saw an accept must not be connected"),
        Err(e) => e,
    };
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
    assert!(err.to_string().contains("never accepted"), "{err}");

    let mut ep = tokio::time::timeout(Duration::from_secs(1), eps.recv())
        .await
        .expect("the proof did land: one session")
        .expect("endpoint");
    let (_in_tx, mut in_rx) = ep.take_inbox(16);
    match tokio::time::timeout(Duration::from_secs(3), in_rx.recv()).await {
        Ok(Some(gsb_core::conn::ConnIn::ServerClosed { cause, .. })) => {
            assert_eq!(cause, gsb_core::conn::ServerClose::IdleTimeout)
        }
        other => panic!("the orphaned session must be swept, got {other:?}"),
    }
    assert!(no_endpoint(&mut eps, 100).await, "and never re-created");
}

mod evidence;
mod rtt;
