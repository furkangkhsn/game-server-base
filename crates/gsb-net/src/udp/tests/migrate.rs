//! Connection migration end to end over real sockets (BACKLOG B3; module
//! `crate::udp::path`): a NAT rebinding the client never sees, the
//! client's own `rebind`, and the compatibility matrix — every pair of
//! an asking / not-asking client and a migrating / non-migrating door.
//! Child of `tests`, so `bound_transport` is shared.

use super::*;
use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::metrics::{MetricsEvent, TransportCounters};
use gsb_protocol::op::base as op;

mod identity;
mod nat;
use nat::Nat;

/// A live session: the client, the server side's two channel ends, the
/// door and its metrics.
struct Live {
    c: UdpClient,
    out_tx: Mailbox<FrameBatch>,
    in_rx: Inbox<ConnIn>,
    eps: mpsc::UnboundedReceiver<Endpoint>,
    listener: Arc<dyn Listener>,
    metrics: mpsc::Receiver<MetricsEvent>,
    /// The moves the session's actor was told of (B113), in order.
    peers: Vec<std::net::SocketAddr>,
}

/// A door (migration `on` or off) and one client (`config`) connected
/// to `via` (a NAT's front) or straight to the door.
async fn live(on: bool, config: UdpClientConfig, nat: Option<&mut Option<Nat>>) -> Live {
    let (tx, metrics) = mpsc::channel(1024);
    let cfg = UdpTransportConfig {
        migration: on,
        metrics: Some(tx),
        ..Default::default()
    };
    let (listener, addr, mut eps, _accept) = bound_transport(cfg).await;
    let to = match nat {
        Some(slot) => slot.insert(Nat::start(addr).await).front(),
        None => addr,
    };
    let c = UdpClient::connect_with(to, config).await.expect("connect");
    let mut ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("endpoint")
        .expect("endpoint");
    let (in_tx, in_rx) = ep.take_inbox(64);
    let (out_tx, out_rx) = ep.take_outbox(64);
    let _pump = ep.start_pump(
        ConnectionId(5),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    Live {
        c,
        out_tx,
        in_rx,
        eps,
        listener,
        metrics,
        peers: Vec::new(),
    }
}

impl Live {
    /// Both bands both ways, tagged `tag`: the client's control frame and
    /// game frame reach the session; the session's control frame (once)
    /// and game frames (until one lands: the lossy band is not re-sent)
    /// reach the client. Whatever path work is pending happens inside.
    async fn exchange(&mut self, tag: &'static [u8]) {
        self.c.send_frame(op::HEARTBEAT, tag).await.unwrap();
        self.c.send_frame(1000, tag).await.unwrap();
        let mut ops = Vec::new();
        while ops.len() < 2 {
            match tokio::time::timeout(Duration::from_secs(5), self.in_rx.recv()).await {
                Ok(Some(ConnIn::Frame(f))) if f.payload.as_ref() == tag => ops.push(f.op),
                Ok(Some(ConnIn::PeerChanged { peer })) => self.peers.push(peer),
                Ok(Some(_)) => {}
                other => panic!("{tag:?}: the server's half: {other:?}"),
            }
        }
        ops.sort_unstable();
        assert_eq!(ops, [op::HEARTBEAT, 1000], "{tag:?}");
        let ack = FrameBody::new(op::HEARTBEAT_ACK, Bytes::from_static(tag));
        self.out_tx.send(vec![ack]).await.unwrap();
        let (mut control, mut game) = (false, false);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !(control && game) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{tag:?}: the client's half"
            );
            let snap = FrameBody::new(1003, Bytes::from_static(tag));
            let _ = self.out_tx.try_send(vec![snap]);
            match self.c.recv_frame(Duration::from_millis(50)).await.unwrap() {
                Some(f) if f.payload.as_ref() == tag && f.op == op::HEARTBEAT_ACK => control = true,
                Some(f) if f.payload.as_ref() == tag && f.op == 1003 => game = true,
                _ => {}
            }
        }
        assert!(self.c.is_established(), "{tag:?}");
    }

    /// No second session was established (no handshake, so no endpoint).
    async fn no_new_session(&mut self) {
        let next = tokio::time::timeout(Duration::from_millis(300), self.eps.recv()).await;
        assert!(next.is_err(), "a new endpoint: a new handshake happened");
    }

    /// The door's counters, once it is closed.
    async fn counters(mut self) -> TransportCounters {
        self.listener.close();
        drop(self.out_tx);
        let mut t = TransportCounters::default();
        while let Ok(Some(ev)) =
            tokio::time::timeout(Duration::from_millis(500), self.metrics.recv()).await
        {
            if let MetricsEvent::Transport(d) = ev {
                t.add(&d);
            }
        }
        t
    }
}

/// A NAT rebinding mid-session (a new public port; the client sees
/// nothing): both bands keep flowing, the session migrates after the
/// client answers the challenge — no new handshake, no new session — and
/// the door counts one migration, port only.
#[tokio::test]
async fn a_nat_rebinding_migrates_the_session() {
    let mut nat = None;
    let mut l = live(true, UdpClientConfig::default(), Some(&mut nat)).await;
    let nat = nat.as_mut().unwrap();
    assert!(l.c.migratable());
    l.exchange(b"before").await;
    let old = nat.public();
    assert_ne!(nat.rebind().await, old);
    l.exchange(b"after").await;
    l.exchange(b"again").await;
    l.no_new_session().await;
    assert_eq!(l.peers, [nat.public()], "the actor is told");
    assert!(l.c.stats.path_challenges_answered >= 1, "{:?}", l.c.stats);
    assert_eq!(l.c.stats.rebinds, 0, "the client did nothing");
    let t = l.counters().await;
    assert_eq!((t.udp_cids_assigned, t.udp_migrations), (1, 1), "{t:?}");
    assert_eq!(t.udp_migrations_port_only, 1);
    assert_eq!(t.udp_path_validations_started, 1);
    assert_eq!(t.udp_cid_unknown + t.udp_path_responses_unmatched, 0);
}

/// A NAT rebinding onto another public address (127.0.0.2: another
/// source, a phone moving behind a carrier NAT): the session moves, and
/// its actor is told the new address — what carries the per-source count
/// along (B113). A new IP: no port-only move.
#[tokio::test]
async fn a_rebinding_to_another_source_tells_the_actor() {
    let mut nat = None;
    let mut l = live(true, UdpClientConfig::default(), Some(&mut nat)).await;
    let nat = nat.as_mut().unwrap();
    l.exchange(b"before").await;
    let moved = nat.rebind_to([127, 0, 0, 2]).await;
    l.exchange(b"after").await;
    l.exchange(b"again").await;
    l.no_new_session().await;
    assert_eq!(l.peers, [moved]);
    let t = l.counters().await;
    assert_eq!(
        (t.udp_migrations, t.udp_migrations_port_only),
        (1, 0),
        "{t:?}"
    );
}

/// `UdpClient::rebind` (Wi-Fi ↔ cellular, as a new local socket): the
/// same session, from the new socket, both bands — no handshake.
#[tokio::test]
async fn rebind_keeps_the_session() {
    let mut l = live(true, UdpClientConfig::default(), None).await;
    l.exchange(b"before").await;
    let before = l.c.local_addr().unwrap();
    let after = l.c.rebind().await.expect("rebind");
    assert_ne!(before, after);
    l.exchange(b"after").await;
    l.no_new_session().await;
    assert_eq!(l.c.stats.rebinds, 1);
    let t = l.counters().await;
    assert_eq!(t.udp_migrations, 1, "{t:?}");
}

/// The matrix's other corners keep today's behaviour: a client that
/// asks, on a door with migration off (an older server, as far as the
/// client can tell), gets no CID, sends untagged, and cannot rebind; a
/// client that does not ask (an older client), on a migrating door, is
/// the same — and behind a rebinding NAT it falls back to today's path:
/// its datagrams from the new address reach no session.
#[tokio::test]
async fn without_both_sides_nothing_migrates() {
    let old_client = UdpClientConfig {
        migration: false,
        ..Default::default()
    };
    for (on, config) in [(false, UdpClientConfig::default()), (true, old_client)] {
        let mut l = live(on, config, None).await;
        assert!(!l.c.migratable(), "door {on}");
        l.exchange(b"plain").await;
        let e = l.c.rebind().await.expect_err("no CID, no rebind");
        assert_eq!(e.kind(), std::io::ErrorKind::Unsupported);
        l.exchange(b"still").await;
        let t = l.counters().await;
        assert_eq!(t.udp_cids_assigned + t.udp_migrations, 0, "door {on}");
        assert_eq!(t.udp_datagrams_malformed, 0, "nothing tagged was sent");
    }
    let mut nat = None;
    let mut l = live(true, old_client, Some(&mut nat)).await;
    l.exchange(b"before").await;
    nat.as_mut().unwrap().rebind().await;
    for _ in 0..5 {
        l.c.send_frame(1000, &b"lost"[..]).await.unwrap();
        let _ = l.c.recv_frame(Duration::from_millis(50)).await;
    }
    let t = l.counters().await;
    assert!(t.udp_datagrams_no_session >= 1, "{t:?}");
    assert_eq!(t.udp_migrations, 0);
}
