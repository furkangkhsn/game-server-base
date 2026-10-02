//! A userspace bottleneck for the congestion tests and the measurement
//! (loopback has none, and `tc netem` needs root): one relay socket per
//! session between its client and the server. Upstream (client → server)
//! passes at once; downstream every session's datagrams share ONE FIFO
//! link — `rate` bytes per second, a tail-drop buffer of `buffer` bytes,
//! then `delay` of propagation — the router in front of a slow path, or
//! (with several sessions) a shared uplink.
//!
//! Tasks, each with one awaited source: a reader per relay socket
//! (`recv_from`), and the link (`timeout_at` on its next departure or
//! arrival, over the reader channel).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// The downstream link.
#[derive(Debug, Clone, Copy)]
pub(super) struct Link {
    pub(super) rate: f64,
    pub(super) buffer: usize,
    pub(super) delay: Duration,
}

/// What the link did, per session.
#[derive(Debug, Clone, Default)]
pub(super) struct LinkStats {
    pub(super) delivered: Vec<u64>,
    pub(super) delivered_bytes: Vec<u64>,
    pub(super) dropped: Vec<u64>,
}

enum Event {
    /// Session `i`'s client spoke from this address.
    Client(usize, SocketAddr),
    /// A datagram from the server for session `i`.
    Down(usize, Vec<u8>),
}

/// The running relay: the address each session's client connects to.
pub(super) struct Relay {
    pub(super) addrs: Vec<SocketAddr>,
    readers: Vec<JoinHandle<()>>,
    link: JoinHandle<LinkStats>,
}

impl Relay {
    pub(super) async fn start(server: SocketAddr, sessions: usize, link: Link) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut socks = Vec::new();
        let mut readers = Vec::new();
        for i in 0..sessions {
            let s = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
            readers.push(tokio::spawn(read(i, s.clone(), server, tx.clone())));
            socks.push(s);
        }
        let addrs = socks.iter().map(|s| s.local_addr().unwrap()).collect();
        let link = tokio::spawn(run_link(socks, link, rx));
        Self {
            addrs,
            readers,
            link,
        }
    }

    /// Stop the relay; what its link did.
    pub(super) async fn finish(self) -> LinkStats {
        for r in &self.readers {
            r.abort();
        }
        for r in self.readers {
            let _ = r.await;
        }
        self.link.await.expect("the link task")
    }
}

async fn read(i: usize, s: Arc<UdpSocket>, server: SocketAddr, tx: mpsc::UnboundedSender<Event>) {
    let mut buf = vec![0u8; 2048];
    let mut client = None;
    while let Ok((n, from)) = s.recv_from(&mut buf).await {
        if from == server {
            let _ = tx.send(Event::Down(i, buf[..n].to_vec()));
            continue;
        }
        if client != Some(from) {
            client = Some(from);
            let _ = tx.send(Event::Client(i, from));
        }
        let _ = s.send_to(&buf[..n], server).await;
    }
}

async fn run_link(
    socks: Vec<Arc<UdpSocket>>,
    link: Link,
    mut rx: mpsc::UnboundedReceiver<Event>,
) -> LinkStats {
    let n = socks.len();
    let mut clients = vec![None; n];
    let mut stats = LinkStats {
        delivered: vec![0; n],
        delivered_bytes: vec![0; n],
        dropped: vec![0; n],
    };
    let mut queue: VecDeque<(usize, Vec<u8>)> = VecDeque::new();
    let mut queued = 0usize;
    // When the link finishes what it is sending, and the datagrams in
    // flight on the wire (their arrival times).
    let mut free_at = Instant::now();
    let mut wire: VecDeque<(Instant, usize, Vec<u8>)> = VecDeque::new();
    loop {
        let now = Instant::now();
        // Serialize back to back at the rate (timer lateness is caught
        // up, never lost as capacity).
        while free_at <= now {
            let Some((i, d)) = queue.pop_front() else {
                break;
            };
            queued -= d.len();
            free_at += Duration::from_secs_f64(d.len() as f64 / link.rate);
            wire.push_back((free_at + link.delay, i, d));
        }
        while wire.front().is_some_and(|w| w.0 <= now) {
            let (_, i, d) = wire.pop_front().unwrap();
            if let Some(c) = clients[i] {
                let _ = socks[i].try_send_to(&d, c);
                stats.delivered[i] += 1;
                stats.delivered_bytes[i] += d.len() as u64;
            }
        }
        let next = [
            (!queue.is_empty()).then_some(free_at),
            wire.front().map(|w| w.0),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(now + Duration::from_secs(3600));
        match tokio::time::timeout_at(next, rx.recv()).await {
            Ok(Some(Event::Client(i, a))) => clients[i] = Some(a),
            Ok(Some(Event::Down(i, d))) => {
                if queued + d.len() > link.buffer {
                    stats.dropped[i] += 1;
                    continue;
                }
                if queue.is_empty() && free_at < Instant::now() {
                    free_at = Instant::now(); // the link was idle
                }
                queued += d.len();
                queue.push_back((i, d));
            }
            Ok(None) => return stats,
            Err(_) => {}
        }
    }
}
