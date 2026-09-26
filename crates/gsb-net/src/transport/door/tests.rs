//! The door's contract, and each in-tree listener's `close` honouring it:
//! a pending accept ends with the closed error, and so does every later
//! one.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::transport::{Listener, Transport};

/// Well above the time a close takes to reach a parked accept.
const PROMPT: Duration = Duration::from_secs(2);

#[tokio::test]
async fn a_pending_admit_ends_with_the_closed_error() {
    let door = Arc::new(Door::new());
    let d = door.clone();
    let pending =
        tokio::spawn(async move { d.admit(std::future::pending::<io::Result<()>>()).await });
    tokio::task::yield_now().await;
    assert!(!pending.is_finished(), "an open door waits");
    door.close();
    let e = tokio::time::timeout(PROMPT, pending)
        .await
        .expect("the close ended the pending accept")
        .expect("no panic")
        .expect_err("a closed error");
    assert!(is_listener_closed(&e));
    assert!(door.is_closed());
    let later = door.admit(async { Ok(()) }).await.expect_err("closed");
    assert!(is_listener_closed(&later), "every later accept too");
}

#[test]
fn only_the_marker_is_a_close() {
    assert!(is_listener_closed(&listener_closed()));
    let same_kind = io::Error::new(io::ErrorKind::NotConnected, "socket gone");
    assert!(!is_listener_closed(&same_kind));
}

/// `close()` on a bound listener ends the accept parked on it (shared
/// with the TLS and QUIC suites, which own their test certificates).
pub(crate) async fn close_ends_a_parked_accept(listener: Arc<dyn Listener>) {
    let l = listener.clone();
    let parked = tokio::spawn(async move { l.accept().await.map(|_| ()) });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!parked.is_finished(), "nothing to accept yet");
    listener.close();
    let e = tokio::time::timeout(PROMPT, parked)
        .await
        .expect("close ended the parked accept")
        .expect("no panic")
        .expect_err("the closed error");
    assert!(is_listener_closed(&e), "got {e}");
    let again = listener.clone().accept().await.map(|_| ());
    assert!(again.is_err_and(|e| is_listener_closed(&e)));
}

fn any_port() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

#[tokio::test]
async fn tcp_close_ends_the_parked_accept() {
    let t = Arc::new(crate::tcp::TcpTransport::default());
    close_ends_a_parked_accept(t.bind(any_port()).await.unwrap()).await;
}

#[tokio::test]
async fn ws_close_ends_the_parked_accept() {
    let t = Arc::new(crate::ws::WsTransport::default());
    close_ends_a_parked_accept(t.bind(any_port()).await.unwrap()).await;
}

#[tokio::test]
async fn udp_close_ends_the_parked_accept() {
    let t = Arc::new(crate::udp::UdpTransport::default());
    close_ends_a_parked_accept(t.bind(any_port()).await.unwrap()).await;
}

/// A door whose accept includes a handshake over TCP (WebSocket, TLS):
/// a peer that connects and never sends a byte holds the accept in the
/// handshake — the close ends that too, long before the handshake's own
/// deadline.
pub(crate) async fn close_ends_an_accept_in_its_handshake(listener: Arc<dyn Listener>) {
    let addr = listener.local_addr().unwrap();
    let l = listener.clone();
    let parked = tokio::spawn(async move { l.accept().await.map(|_| ()) });
    let _silent = tokio::net::TcpStream::connect(addr).await.expect("connect");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!parked.is_finished(), "held in the handshake");
    listener.close();
    let e = tokio::time::timeout(PROMPT, parked)
        .await
        .expect("close ended the handshake")
        .expect("no panic")
        .expect_err("the closed error");
    assert!(is_listener_closed(&e), "got {e}");
}

#[tokio::test]
async fn ws_close_ends_an_accept_stuck_in_its_handshake() {
    let t = Arc::new(crate::ws::WsTransport::default());
    close_ends_an_accept_in_its_handshake(t.bind(any_port()).await.unwrap()).await;
}
