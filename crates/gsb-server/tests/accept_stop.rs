//! `ServerHandle::stop` ends every accept loop without aborting it
//! (BACKLOG B16), end to end, on all five doors at once.
//!
//! Before: `stop` closed each listener and then aborted its accept task
//! — the one hard abort left in the shutdown cascade (DESIGN §9). Now
//! `close` ends the listener's pending accept with the listener-closed
//! error and the loop returns; `stop` waits for that (bounded) and
//! reports how many loops it had to abort instead. The second test pins
//! an accept that is IN FLIGHT — a TLS and a WebSocket handshake a silent
//! peer holds open — which the close ends too, so `stop` does not wait
//! for the handshakes' own deadlines.

use std::time::{Duration, Instant};

use gsb_server::{ListenerEntry, ListenerTransport, StopReport};
use tokio::net::TcpStream;

mod common;

/// Well below the handshake deadlines (TLS/WS: seconds) and below the
/// stop's own grace for an overrunning loop.
const STOP_WITHIN: Duration = Duration::from_millis(900);

const DOORS: [ListenerTransport; 5] = [
    ListenerTransport::Tcp,
    ListenerTransport::Tls,
    ListenerTransport::Ws,
    ListenerTransport::Udp,
    ListenerTransport::Quic,
];

/// A one-room server with one listener per door kind, in [`DOORS`] order.
async fn server(pki: &common::TlsPki) -> gsb_server::ServerHandle {
    let listeners = DOORS
        .iter()
        .map(|&door| {
            let tls = matches!(door, ListenerTransport::Tls | ListenerTransport::Quic);
            ListenerEntry {
                transport: door,
                bind: "127.0.0.1:0".into(),
                tls_cert: tls.then(|| pki.cert_pem_path.clone()),
                tls_key: tls.then(|| pki.key_pem_path.clone()),
            }
        })
        .collect();
    let cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(listeners),
        ..Default::default()
    };
    gsb_server::start_server(cfg).await.expect("server starts")
}

/// Stop, bounded, and return the report and how long it took.
async fn stop(handle: gsb_server::ServerHandle) -> (StopReport, Duration) {
    let started = Instant::now();
    let report = tokio::time::timeout(Duration::from_secs(10), handle.stop())
        .await
        .expect("stop() completes");
    (report, started.elapsed())
}

const ALL_ENDED: StopReport = StopReport {
    accept_loops_ended: DOORS.len(),
    accept_loops_aborted: 0,
    rooms_finished: true,
    // The demo's economy service, stopped after the rooms (BACKLOG F5).
    services_ended: 1,
    services_aborted: 0,
};

#[tokio::test]
async fn every_accept_loop_ends_on_its_closed_listener() {
    let pki = common::mint_tls_pki("accept-stop");
    let handle = server(&pki).await;
    let (report, took) = stop(handle).await;
    assert_eq!(report, ALL_ENDED, "no loop needed the abort");
    assert!(took < STOP_WITHIN, "stop took {took:?}");
}

#[tokio::test]
async fn an_accept_held_in_a_handshake_does_not_hold_the_stop() {
    let pki = common::mint_tls_pki("accept-stop-held");
    let handle = server(&pki).await;
    // Silent peers: TCP connected, not one byte of TLS hello or HTTP
    // upgrade — each door's accept waits in its handshake.
    let tls = TcpStream::connect(handle.addrs[1]).await.expect("to TLS");
    let ws = TcpStream::connect(handle.addrs[2]).await.expect("to WS");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (report, took) = stop(handle).await;
    assert_eq!(report, ALL_ENDED, "the close ended the handshakes too");
    assert!(took < STOP_WITHIN, "stop took {took:?}");
    drop((tls, ws));
}
