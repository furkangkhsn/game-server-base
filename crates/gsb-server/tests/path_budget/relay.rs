//! A userspace bottleneck between the client and the rUDP door: upstream
//! (client → server) passes at once; downstream a token bucket polices
//! the path — `rate` bytes per second, `burst` bytes deep — and DROPS
//! what does not fit (a policer: loss, no queue). Two tasks, one awaited
//! source each; the client's address reaches the downstream task over a
//! channel.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub struct Relay {
    /// Where the client connects.
    pub addr: SocketAddr,
    tasks: [JoinHandle<()>; 2],
}

impl Relay {
    pub async fn start(server: SocketAddr, rate: f64, burst: f64) -> Self {
        let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let back = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let addr = front.local_addr().expect("addr");
        let (client_tx, client_rx) = mpsc::unbounded_channel();
        let up = tokio::spawn(upstream(front.clone(), back.clone(), server, client_tx));
        let down = tokio::spawn(downstream(back, front, client_rx, rate, burst));
        Self {
            addr,
            tasks: [up, down],
        }
    }

    pub fn stop(self) {
        for t in self.tasks {
            t.abort();
        }
    }
}

async fn upstream(
    front: Arc<UdpSocket>,
    back: Arc<UdpSocket>,
    server: SocketAddr,
    client_tx: mpsc::UnboundedSender<SocketAddr>,
) {
    let mut buf = vec![0u8; 2048];
    let mut known = None;
    while let Ok((n, from)) = front.recv_from(&mut buf).await {
        if known != Some(from) {
            known = Some(from);
            let _ = client_tx.send(from);
        }
        let _ = back.send_to(&buf[..n], server).await;
    }
}

async fn downstream(
    back: Arc<UdpSocket>,
    front: Arc<UdpSocket>,
    mut client_rx: mpsc::UnboundedReceiver<SocketAddr>,
    rate: f64,
    burst: f64,
) {
    let mut buf = vec![0u8; 2048];
    let mut client = None;
    let (mut tokens, mut at) = (burst, Instant::now());
    while let Ok((n, _)) = back.recv_from(&mut buf).await {
        while let Ok(c) = client_rx.try_recv() {
            client = Some(c);
        }
        let now = Instant::now();
        tokens = (tokens + rate * now.duration_since(at).as_secs_f64()).min(burst);
        at = now;
        let Some(to) = client else { continue };
        if tokens >= n as f64 {
            tokens -= n as f64;
            let _ = front.send_to(&buf[..n], to).await;
        }
    }
}

/// The hang guard of every wait in the suite.
pub const GUARD: Duration = Duration::from_secs(30);
