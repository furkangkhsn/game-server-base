//! The backlog reaches `listen(2)` (B84): observed from the outside, as
//! the number of connects the kernel completes while nobody accepts.

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;

use super::*;
use crate::transport::Transport;

fn any_port() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// How many of `tries` connects to `addr`, made one after another while
/// nobody accepts, complete within 300 ms each — stopping at the first
/// that does not (its SYN was dropped: the accept queue is full, and
/// the kernel's retry comes a second later). The streams stay open
/// until the count is taken, so each one holds its queue slot.
async fn queued(addr: SocketAddr, tries: usize) -> usize {
    let mut held = Vec::with_capacity(tries);
    for _ in 0..tries {
        match tokio::time::timeout(Duration::from_millis(300), TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => held.push(stream),
            _ => break,
        }
    }
    held.len()
}

/// A backlog of one: Linux queues `backlog + 1` completed connections
/// (a connect racing the previous one's final ACK may add one more),
/// and drops the next SYN — far below the sixteen tried. A listener
/// that ignored the value would take all sixteen (the default is 128).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_backlog_of_one_queues_a_couple_of_connects() {
    let listener = bind_tcp(any_port(), 1).expect("bind");
    let n = queued(listener.local_addr().unwrap(), 16).await;
    assert!(
        (1..=4).contains(&n),
        "{n} connects queued behind a backlog of 1"
    );
}

/// The default queues every one of sixteen connects nobody accepts.
#[tokio::test]
async fn the_default_backlog_queues_sixteen_connects() {
    let listener = bind_tcp(any_port(), DEFAULT_LISTEN_BACKLOG).expect("bind");
    assert_eq!(queued(listener.local_addr().unwrap(), 16).await, 16);
}

/// Zero and a value past a C `int` never reach a socket; the bounds
/// themselves do.
#[tokio::test]
async fn zero_and_past_a_c_int_are_refused() {
    for bad in [0, MAX_LISTEN_BACKLOG + 1, u32::MAX] {
        let err = bind_tcp(any_port(), bad).expect_err("refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad}: {err}");
        assert!(listen_backlog_problem(bad).is_some(), "{bad}");
    }
    for good in [1, DEFAULT_LISTEN_BACKLOG, MAX_LISTEN_BACKLOG] {
        assert_eq!(listen_backlog_problem(good), None, "{good}");
        bind_tcp(any_port(), good).expect("bound");
    }
}

/// The transports' defaults are the builder's default: a door nobody
/// configured keeps the queue it had before the knob existed.
#[test]
fn every_tcp_door_defaults_to_the_old_backlog() {
    assert_eq!(
        crate::tcp::TcpTransport::default().listen_backlog,
        DEFAULT_LISTEN_BACKLOG
    );
    assert_eq!(
        crate::ws::WsTransport::default().listen_backlog,
        DEFAULT_LISTEN_BACKLOG
    );
}

/// The plain TCP door's field reaches its socket: the door accepts only
/// when asked, so its queue is visible from outside.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_tcp_doors_backlog_reaches_its_socket() {
    let door = Arc::new(crate::tcp::TcpTransport {
        listen_backlog: 1,
        ..Default::default()
    });
    let listener = door.bind(any_port()).await.expect("bind");
    let n = queued(listener.local_addr().unwrap(), 16).await;
    assert!(
        (1..=4).contains(&n),
        "{n} connects queued behind a backlog of 1"
    );
}

/// The doors whose intake accepts eagerly (WebSocket here, TLS in
/// `tls::tests`) show their queue to nobody; their field reaching the
/// builder shows as the builder's refusal of a zero.
#[tokio::test]
async fn the_tcp_and_ws_doors_hand_their_backlog_to_the_builder() {
    let tcp = Arc::new(crate::tcp::TcpTransport {
        listen_backlog: 0,
        ..Default::default()
    });
    let ws = Arc::new(crate::ws::WsTransport {
        listen_backlog: 0,
        ..Default::default()
    });
    for (door, t) in [("tcp", tcp as Arc<dyn Transport>), ("ws", ws)] {
        let err = t.bind(any_port()).await.map(|_| ()).expect_err(door);
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{door}: {err}");
    }
}
