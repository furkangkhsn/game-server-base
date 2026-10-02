//! B113 end to end: an unauthenticated rUDP session whose client moves
//! behind another NAT address (127.0.0.1 → 127.0.0.2, a NAT rebinding
//! onto another source) takes its place in the per-source count with
//! it. Before, the registry kept counting it at the first address: that
//! source stayed refused and the new one counted nothing. The probes are
//! plain TCP sessions, the door with no other per-source cap.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_client::conn::Conn;
use gsb_client::session::{self, Credentials};
use gsb_core::metrics::MetricReport;
use gsb_protocol::op;
use gsb_server::{Config, ListenerEntry, ListenerTransport};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{connect_from, refused, rows};

/// A one-client NAT on loopback (B3's relay): the client talks to the
/// front socket, the server sees the current back socket; `rebind_to`
/// replaces the back socket — a new public address — and drops the old
/// mapping. One awaited source per task; both aborted with the relay.
struct Nat {
    front: Arc<UdpSocket>,
    client: watch::Receiver<Option<SocketAddr>>,
    back: watch::Sender<Arc<UdpSocket>>,
    c2s: JoinHandle<()>,
    s2c: JoinHandle<()>,
}

impl Nat {
    async fn start(server: SocketAddr) -> Self {
        let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("front"));
        let first = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("back"));
        let (client_tx, client) = watch::channel(None::<SocketAddr>);
        let (back, back_rx) = watch::channel(Arc::clone(&first));
        let f = Arc::clone(&front);
        let c2s = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            while let Ok((n, from)) = f.recv_from(&mut buf).await {
                let _ = client_tx.send(Some(from));
                let out = Arc::clone(&back_rx.borrow());
                let _ = out.send_to(&buf[..n], server).await;
            }
        });
        let s2c = Self::mapping(Arc::clone(&front), client.clone(), first);
        Self {
            front,
            client,
            back,
            c2s,
            s2c,
        }
    }

    fn mapping(
        front: Arc<UdpSocket>,
        client: watch::Receiver<Option<SocketAddr>>,
        back: Arc<UdpSocket>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            while let Ok((n, _)) = back.recv_from(&mut buf).await {
                let to = *client.borrow();
                if let Some(to) = to {
                    let _ = front.send_to(&buf[..n], to).await;
                }
            }
        })
    }

    async fn rebind_to(&mut self, ip: [u8; 4]) {
        let next = Arc::new(
            UdpSocket::bind(SocketAddr::from((ip, 0)))
                .await
                .expect("back"),
        );
        self.s2c.abort();
        self.s2c = Self::mapping(
            Arc::clone(&self.front),
            self.client.clone(),
            Arc::clone(&next),
        );
        self.back.send_replace(next);
    }
}

impl Drop for Nat {
    fn drop(&mut self) {
        self.c2s.abort();
        self.s2c.abort();
    }
}

/// Drive the client (it answers the path challenge inside `recv`) until
/// the door reports the migration.
async fn until_migrated(c: &mut Conn, rx: &mut UnboundedReceiver<MetricReport>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        while let Ok(r) = rx.try_recv() {
            if r.transport.udp_migrations >= 1 {
                return;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "no migration");
        let _ = c.recv(Duration::from_millis(50)).await;
    }
}

/// A plain TCP session from `source` that the server keeps — its AUTH
/// succeeds — once it does: until then each try is refused at birth
/// (the registry has not yet moved the migrated session's count off
/// `source`). The guard only bounds a hang.
async fn served_from(source: [u8; 4], tcp: SocketAddr) -> Conn {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let mut c = connect_from(source, tcp).await;
        let creds = Credentials::named("a");
        if session::auth(&mut c, &creds, Duration::from_secs(5), |_| {})
            .await
            .is_ok()
        {
            return c;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{source:?} never got its place back"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_migrated_session_s_count_moves_to_its_new_source() {
    let door = |transport| ListenerEntry {
        transport,
        bind: "127.0.0.1:0".into(),
        tls_cert: None,
        tls_key: None,
    };
    let cfg = Config {
        room_count: 1,
        idle_timeout_secs: 0.0,
        listeners: Some(vec![
            door(ListenerTransport::Udp),
            door(ListenerTransport::Tcp),
        ]),
        udp_migration: true,
        max_unauth_conns_per_source: Some(1),
        ..Default::default()
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = gsb_server::start_server_metrics(cfg, tx)
        .await
        .expect("server starts");
    let (udp, tcp) = (handle.addrs[0], handle.addrs[1]);
    const A: [u8; 4] = [127, 0, 0, 1];
    const B: [u8; 4] = [127, 0, 0, 2];

    // An unauthenticated rUDP session behind the NAT's 127.0.0.1: it
    // holds that source's one place.
    let mut nat = Nat::start(udp).await;
    let front = nat.front.local_addr().expect("front");
    let mut c = gsb_client::connect::udp(front)
        .await
        .expect("rUDP through the NAT");
    // The client is connected once the demux sends its accept; the accept
    // loop registers the session after that. Its row, in a report, is
    // the condition — not the order of two doors' opens.
    rows(&mut rx, 1).await;
    refused(connect_from(A, tcp).await).await;

    // The NAT moves it to 127.0.0.2; its next datagram starts the path
    // validation, the client answers, the session migrates.
    nat.rebind_to(B).await;
    c.send(op::base::HEARTBEAT, &[]).await.expect("sent");
    until_migrated(&mut c, &mut rx).await;

    // Its place moves (actor → registry, after the transport's move):
    // 127.0.0.1 is served once the registry has the new source — the
    // condition, polled — and from then on 127.0.0.2 is at its cap. Had
    // the count stayed at the first address, 127.0.0.1 would never be
    // served. `a` is authenticated, so it holds no place itself.
    let a = served_from(A, tcp).await;
    refused(connect_from(B, tcp).await).await;
    drop((a, c, nat));
    handle.stop().await;
}
