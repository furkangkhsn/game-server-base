//! B47: a connection that never finishes its request head does not keep
//! its task alive — after `HEAD_DEADLINE` it gets one `408` and is
//! closed; a request that arrives in time is served as before. Paused
//! clock: the connection task runs under the test runtime, over an
//! in-memory pipe.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};

use super::*;

/// The hang guard: far past every window under test, so it only turns
/// a connection that is never answered into a failure.
const GUARD: Duration = Duration::from_secs(600);

/// One connection task over a pipe; returns the peer's end and the task.
fn connect() -> (DuplexStream, JoinHandle<()>) {
    let (ops, _) = surface(&Config::default());
    let (peer, ours) = duplex(64 * 1024);
    (peer, tokio::spawn(serve_one(ours, ops)))
}

/// Everything the server sends until it closes its half.
async fn response(peer: &mut DuplexStream) -> String {
    let mut buf = Vec::new();
    timeout(GUARD, peer.read_to_end(&mut buf))
        .await
        .expect("the server answers and closes")
        .expect("read to EOF");
    String::from_utf8(buf).expect("utf-8 response")
}

/// A peer that connects and sends nothing is answered `408` exactly at
/// the deadline (not before), and its task ends after the bounded drain.
#[tokio::test(start_paused = true)]
async fn a_silent_peer_is_closed_at_the_deadline() {
    let start = Instant::now();
    let (mut peer, task) = connect();
    let text = response(&mut peer).await;
    let answered = start.elapsed();
    assert!(
        text.starts_with("HTTP/1.1 408 Request Timeout\r\n"),
        "{text}"
    );
    assert!(text.contains("Connection: close\r\n"), "{text}");
    assert!(answered >= HEAD_DEADLINE, "answered early: {answered:?}");
    assert!(
        answered < HEAD_DEADLINE + Duration::from_millis(5),
        "answered late: {answered:?}"
    );
    timeout(GUARD, task)
        .await
        .expect("the connection task ends")
        .expect("no panic");
    assert!(start.elapsed() <= HEAD_DEADLINE + DRAIN_WINDOW + Duration::from_millis(5));
    // Its end of the pipe is gone: the connection is closed.
    assert!(peer.write_all(b"GET / HTTP/1.1\r\n\r\n").await.is_err());
}

/// The deadline covers the whole head, not each read: a peer dribbling
/// one byte a second (slowloris) is cut at the same deadline.
#[tokio::test(start_paused = true)]
async fn a_dribbling_peer_is_cut_at_the_same_deadline() {
    let start = Instant::now();
    let (peer, task) = connect();
    let (mut rx, mut tx) = tokio::io::split(peer);
    tokio::spawn(async move {
        for b in b"GET /metrics HTTP/1.1\r\nHost: x\r\n".iter().cycle() {
            sleep(Duration::from_secs(1)).await;
            if tx.write_all(&[*b]).await.is_err() {
                return;
            }
        }
    });
    let mut buf = Vec::new();
    timeout(GUARD, rx.read_to_end(&mut buf))
        .await
        .expect("the server answers and closes")
        .expect("read to EOF");
    let text = String::from_utf8(buf).expect("utf-8 response");
    assert!(text.starts_with("HTTP/1.1 408 "), "{text}");
    let answered = start.elapsed();
    assert!(answered >= HEAD_DEADLINE, "answered early: {answered:?}");
    assert!(
        answered < HEAD_DEADLINE + Duration::from_millis(5),
        "{answered:?}"
    );
    timeout(GUARD, task)
        .await
        .expect("the task ends")
        .expect("no panic");
}

/// A scrape whose head arrives just inside the deadline is served, not
/// timed out — and at once.
#[tokio::test(start_paused = true)]
async fn a_scrape_inside_the_deadline_is_served() {
    for delay in [Duration::ZERO, HEAD_DEADLINE - Duration::from_millis(100)] {
        let (mut peer, task) = connect();
        sleep(delay).await;
        let sent = Instant::now();
        peer.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("request written");
        let text = response(&mut peer).await;
        let want = if cfg!(feature = "prometheus") {
            "HTTP/1.1 200 OK\r\n"
        } else {
            "HTTP/1.1 404 Not Found\r\n"
        };
        assert!(text.starts_with(want), "after {delay:?}: {text}");
        assert!(sent.elapsed() < Duration::from_millis(5), "after {delay:?}");
        timeout(GUARD, task)
            .await
            .expect("the task ends")
            .expect("no panic");
    }
}
