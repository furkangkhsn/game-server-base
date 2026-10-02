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
//!
//! The HTTP ops surface's accept loop has the same door (BACKLOG B33):
//! it was the last accept loop `stop` aborted; the third test pins that
//! it now ends on its closed door, counted with the other accept loops.
//!
//! Every report here also says the collector's final report is complete
//! (BACKLOG F35) — a silent session's pump does not hold it either.

use std::time::{Duration, Instant};

use gsb_server::{ListenerEntry, ListenerTransport, StopReport};
use tokio::net::TcpStream;

mod common;

/// The bound on `stop` these tests can claim on any machine: below the
/// handshake deadlines (TLS/WS: 10 s) — `stop` did not wait out a held
/// handshake. That no loop overran the stop's own one-second grace is
/// the report's `accept_loops_aborted == 0` (a loop still in its
/// handshake at the grace is aborted and counted), and that `stop` does
/// not sit out the grace once every loop has ended is pinned on the
/// paused clock (`boot::stop::tests`). The old 900 ms wall-clock bound
/// measured the machine's scheduler as well (BACKLOG F25).
const STOP_WITHIN: Duration = gsb_net::tls::HANDSHAKE_TIMEOUT;
const _: () = assert!(
    gsb_net::ws::WS_HANDSHAKE_TIMEOUT.as_millis() >= STOP_WITHIN.as_millis(),
    "the bound is below every held handshake's deadline"
);

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
        udp_static_key: Some(common::rudp_key().0.clone()),
        ..Default::default()
    };
    gsb_server::start_server(cfg).await.expect("server starts")
}

/// Stop, bounded, and return the report and how long it took.
async fn stop(handle: gsb_server::ServerHandle) -> (StopReport, Duration) {
    let started = Instant::now();
    let report = tokio::time::timeout(STOP_WITHIN * 3, handle.stop())
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
    // Every session producer ended before the final report (F35).
    final_report_complete: true,
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

/// A session whose peer stays silent past the stop — connected, never
/// closing its socket, as a half-open client does — keeps its reader pump
/// waiting on the socket until the idle window (30 s). The pump reports
/// its losses on the transport channel, whose close the collector's final
/// report does not wait for (BACKLOG F35): the report is complete as soon
/// as the session side has ended, and `stop` does not sit out the
/// collector's grace.
#[tokio::test]
async fn a_silent_peer_does_not_hold_the_final_report() {
    let pki = common::mint_tls_pki("accept-stop-silent");
    let handle = server(&pki).await;
    let silent = TcpStream::connect(handle.addrs[0]).await.expect("to TCP");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (report, took) = stop(handle).await;
    assert_eq!(
        report, ALL_ENDED,
        "the final report did not wait for the pump"
    );
    assert!(took < STOP_WITHIN, "stop took {took:?}");
    drop(silent);
}

/// The ops HTTP surface's accept loop ends on its closed door too (B33):
/// `stop` counts it with the game listeners' loops, aborts none, and its
/// listener is gone once `stop` returns. A silent scraper — connected,
/// not one byte of request head — sits in its own connection task, not
/// in the accept loop, so it does not hold the stop either.
#[tokio::test]
async fn the_ops_http_accept_loop_ends_on_its_closed_door() {
    let cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(vec![ListenerEntry {
            transport: ListenerTransport::Tcp,
            bind: "127.0.0.1:0".into(),
            tls_cert: None,
            tls_key: None,
        }]),
        http_listen: "127.0.0.1:0".into(),
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg).await.expect("server starts");
    let ops = handle.http_addr.expect("the ops surface is on");
    let scraper = TcpStream::connect(ops).await.expect("to the ops surface");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (report, took) = stop(handle).await;
    assert_eq!(
        report,
        StopReport {
            // The game listener's loop and the ops surface's.
            accept_loops_ended: 2,
            ..ALL_ENDED
        },
        "the ops accept loop ended by itself, no abort"
    );
    assert!(took < STOP_WITHIN, "stop took {took:?}");
    assert!(
        TcpStream::connect(ops).await.is_err(),
        "the ops listener closed with its loop"
    );
    drop(scraper);
}
