//! A one-client NAT on loopback: the client talks to the NAT's front
//! socket, the server sees the NAT's current back socket. `rebind`
//! replaces the back socket — a new public port, as a NAT does when its
//! mapping expires — and drops the old mapping: what the server still
//! sends there is lost. The client never learns of it. One awaited
//! source per task; every task is aborted with the NAT.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub(in crate::udp::tests) struct Nat {
    front: Arc<UdpSocket>,
    client: watch::Receiver<Option<SocketAddr>>,
    back: watch::Sender<Arc<UdpSocket>>,
    c2s: JoinHandle<()>,
    s2c: JoinHandle<()>,
}

impl Nat {
    pub(in crate::udp::tests) async fn start(server: SocketAddr) -> Self {
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

    /// The server → client half of one mapping (one back socket).
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

    /// Where the client sends.
    pub(in crate::udp::tests) fn front(&self) -> SocketAddr {
        self.front.local_addr().unwrap()
    }

    /// The address the server sees now.
    pub(in crate::udp::tests) fn public(&self) -> SocketAddr {
        self.back.borrow().local_addr().unwrap()
    }

    /// A new mapping: a new back socket (a new public port), the old one
    /// gone. Returns the new public address.
    pub(in crate::udp::tests) async fn rebind(&mut self) -> SocketAddr {
        self.rebind_to([127, 0, 0, 1]).await
    }

    /// [`Self::rebind`] onto a public address at `ip` — another source
    /// when it is not the old one's (127.0.0.2: the carrier NAT a phone
    /// moves behind, B113).
    pub(in crate::udp::tests) async fn rebind_to(&mut self, ip: [u8; 4]) -> SocketAddr {
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
        self.public()
    }
}

impl Drop for Nat {
    fn drop(&mut self) {
        self.c2s.abort();
        self.s2c.abort();
    }
}
