//! The write-stall bound, end to end: a peer that connects, joins, and
//! then never reads again must lose its session — and the registry row
//! must actually be released.
//!
//! The gap this locks sits BETWEEN the two outbound cases the server
//! already handled. A FULL outbound channel is tolerated by design (the
//! room drops the batch and counts it: a slow client costs itself
//! staleness, nothing more). A CLOSED one tears the session down
//! (`w_closing`). Neither describes a peer that stops reading: its
//! receive window closes, the writer pump parks inside its socket write,
//! the channel stays FULL forever, the room drops every snapshot — and
//! nothing anywhere notices. Not even the reader's idle window, which
//! watches INBOUND silence, and which a peer that has merely stopped
//! READING need not produce at all.
//!
//! The bound is on PROGRESS: nothing written successfully for
//! `write_stall_secs` means the direction is dead, and the verdict
//! travels the connection actor's mailbox — an in-process channel, never
//! the socket, which is precisely the thing that is stuck. The actor then
//! runs the ORDINARY teardown, so this test asserts what
//! `udp_rel_liveness.rs` asserts for the rUDP band's sibling bound: the
//! close reached the registry (`closes`) and the row of a session nothing
//! parks is released (`conns`).
//!
//! WHY the QUIC door for a stream-transport guardrail: the clock lives in
//! the shared pump (`gsb_net::pump`), so every stream door — tcp, tls,
//! ws, quic — runs exactly this code. QUIC is the one whose receive
//! window the CLIENT sets, so "the peer stopped reading" can be forced in
//! kilobytes instead of waiting for megabytes of kernel socket buffer to
//! fill. The `gsb-net` suite (`tcp::tests::stall`) forces the same
//! condition on a real TCP socket at the pump level.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gsb_core::conn::ServerClose;
use gsb_core::metrics::{MetricReport, NetReport, RegistryReport};
use gsb_protocol::base::{Auth, JoinRoom};
use prost::Message;
use tokio::sync::mpsc;

mod common;

use common::TLS_SERVER_NAME;

/// The client's per-stream flow-control window. Small on purpose: once
/// the application stops reading, the server may write at most this much
/// more before its writes stop completing.
const CLIENT_WINDOW: u32 = 2048;

/// The stall window the server is configured with.
const STALL_SECS: f64 = 1.0;

/// A QUIC server whose ONLY session-death mechanism is the write stall:
/// the reader's idle window is disabled (and would watch the wrong
/// direction anyway).
fn cfg(pki: &common::TlsPki) -> gsb_server::Config {
    gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        idle_timeout_secs: 0.0,
        write_stall_secs: STALL_SECS,
        // No parking: the disconnect policy REFUSES to hold the entity
        // (`gsb-game`'s `park_on_disconnect` returns no-park at zero
        // grace), so the registry row is released outright instead of
        // waiting out a grace window. That makes `conns` the direct
        // evidence the row is gone.
        disconnect_grace_secs: 0.0,
        listeners: Some(vec![gsb_server::ListenerEntry {
            transport: gsb_server::ListenerTransport::Quic,
            bind: "127.0.0.1:0".into(),
            tls_cert: Some(pki.cert_pem_path.clone()),
            tls_key: Some(pki.key_pem_path.clone()),
        }]),
        ..Default::default()
    }
}

/// A QUIC client that advertises a tiny receive window, so it can become
/// a peer that "stops reading" within a couple of kilobytes.
async fn connect(
    pki: &common::TlsPki,
    addr: std::net::SocketAddr,
) -> (quinn::SendStream, quinn::RecvStream) {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(pki.ca_der.clone()).expect("CA parses");
    // A fresh provider instance per connector (never a global install).
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![gsb_net::quic::ALPN_PROTOCOL.to_vec()];
    let crypto =
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC client TLS setup");
    let mut client_config = quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    // THE knob this test exists for: the peer's advertised window. It
    // only moves as the application reads, so an application that stops
    // reading wedges the server's writes after this many more bytes.
    transport.stream_receive_window(CLIENT_WINDOW.into());
    transport.receive_window((CLIENT_WINDOW * 4).into());
    client_config.transport_config(Arc::new(transport));
    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("client endpoint");
    endpoint.set_default_client_config(client_config);
    // The endpoint handle is dropped on return ON PURPOSE (the idiom the
    // multi_listener suite documents): quinn's driver keeps serving the
    // connection until its last stream handle is gone.
    let conn = endpoint
        .connect(addr, TLS_SERVER_NAME)
        .expect("connect setup")
        .await
        .expect("QUIC handshake");
    conn.open_bi().await.expect("bi-stream open")
}

/// Write one length-prefixed frame onto the bi-stream.
async fn send(stream: &mut quinn::SendStream, op: u16, payload: Vec<u8>) {
    let mut body = Vec::with_capacity(2 + payload.len());
    body.extend_from_slice(&op.to_le_bytes());
    body.extend_from_slice(&payload);
    let mut out = (body.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&body);
    stream.write_all(&out).await.expect("frame written");
}

/// Read frames until `want` arrives — the LAST reading this client ever
/// does.
async fn read_until(recv: &mut quinn::RecvStream, want: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf).await.expect("length prefix");
        let len = u32::from_le_bytes(len_buf) as usize;
        assert!((2..=65536).contains(&len), "implausible frame length {len}");
        let mut body = vec![0u8; len];
        recv.read_exact(&mut body).await.expect("frame body");
        if u16::from_le_bytes([body[0], body[1]]) == want {
            return;
        }
    }
    panic!("never saw op {want}");
}

/// Drain metric reports until one carries a registry section satisfying
/// `done` (the `udp_rel_liveness.rs` idiom). Returns that section and the
/// report's net scope (read off the SAME report: the connection actor's
/// final sample is queued before its `ConnClosed` reaches the registry,
/// so a report that shows the close also carries its verdict).
async fn registry_until(
    rx: &mut mpsc::UnboundedReceiver<MetricReport>,
    what: &str,
    deadline: Instant,
    done: impl Fn(&RegistryReport) -> bool,
) -> (RegistryReport, NetReport) {
    let mut last: Option<RegistryReport> = None;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for {what}; last registry: {last:?}"));
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(report)) => {
                if let Some(reg) = report.registry {
                    if done(&reg) {
                        return (reg, report.net);
                    }
                    last = Some(reg);
                }
            }
            Ok(None) => panic!("the metrics channel closed while waiting for {what}"),
            Err(_) => panic!("timed out waiting for {what}; last registry: {last:?}"),
        }
    }
}

/// THE PROPERTY: a peer that connects, authenticates and then stops
/// reading loses its session through the ordinary teardown, and the
/// registry row it was holding is released. Before the bound existed this
/// session lived until process death: the channel was full, never closed,
/// and no inbound clock had anything to look at.
#[tokio::test]
async fn a_peer_that_stops_reading_loses_its_session() {
    let pki = common::mint_tls_pki("write-stall");
    let (report_tx, mut reports) = mpsc::unbounded_channel();
    let handle = gsb_server::start_server_metrics(cfg(&pki), report_tx)
        .await
        .expect("server starts");

    // The deaf peer: joins room 1 so the room keeps producing snapshots
    // for it, then never reads another byte.
    let (mut send_a, mut recv_a) = connect(&pki, handle.addr).await;
    send(
        &mut send_a,
        gsb_protocol::op::base::AUTH_REQ,
        Auth {
            name: "deaf".into(),
            ticket: vec![],
            protocol_version: gsb_protocol::PROTOCOL_VERSION,
        }
        .encode_to_vec(),
    )
    .await;
    send(
        &mut send_a,
        gsb_protocol::op::base::JOIN_ROOM_REQ,
        JoinRoom { room_id: 1 }.encode_to_vec(),
    )
    .await;
    read_until(&mut recv_a, gsb_protocol::op::base::JOIN_ROOM_RESULT).await;

    // The shape of the bug, exactly: the peer stops READING but keeps
    // SENDING. Its moves keep the room producing a snapshot every tick
    // (a motionless world would fall back to the 1 Hz keep-alive), and
    // its inbound traffic is what makes any inbound-side clock useless
    // here — an idle window would keep resetting even if one were armed.
    let mover = tokio::spawn(async move {
        let mut seq = 0u64;
        loop {
            let (x, y) = if seq.is_multiple_of(2) {
                (20, 20)
            } else {
                (-20, -20)
            };
            send(
                &mut send_a,
                gsb_game::op::MOVE_TO,
                gsb_game::game::MoveTo { x, y, seq }.encode_to_vec(),
            )
            .await;
            seq += 1;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    let (before, _) = registry_until(
        &mut reports,
        "the connection to be registered",
        Instant::now() + Duration::from_secs(15),
        |r| r.conns >= 1,
    )
    .await;
    assert_eq!(before.closes, 0, "nothing has closed yet: {before:?}");

    // From here the client reads NOTHING. Its advertised window stops
    // moving, the server's writes stop completing, and the stall window
    // is the only thing left that can end this.
    let (after, net) = registry_until(
        &mut reports,
        "the stalled session to end through the ordinary teardown",
        Instant::now() + Duration::from_secs(40),
        |r| r.closes >= 1,
    )
    .await;
    assert_eq!(
        after.closes, 1,
        "the connection actor must reach RegistryMsg::ConnClosed — the \
         single exit of ConnectionActor::run: {after:?}"
    );
    assert_eq!(
        after.conns, 0,
        "the registry row must be released (nothing parks this session — \
         the default disconnect policy despawns): {after:?}"
    );
    // And the close is COUNTED, under its own reason: a capacity run
    // whose clients stop draining must see the server shed them — the
    // stalled socket cannot carry the ERROR notice, so no client-side
    // counter ever moves.
    for (reason, n) in net.server_closes.iter() {
        assert_eq!(
            n,
            u64::from(reason == ServerClose::WriteStall),
            "{}: the one close is a write stall: {}",
            reason.label(),
            net.server_closes.nonzero_summary()
        );
    }

    // Keep the read half alive to the very end: a dropped handle would
    // let quinn close the connection and prove nothing.
    mover.abort();
    drop(recv_a);
    handle.stop().await;
}
