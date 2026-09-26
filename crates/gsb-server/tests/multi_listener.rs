//! Multi-listener end-to-end tests: SEVERAL transport doors serving ONE
//! map/room simultaneously (the composition-root contract this suite
//! locks): a TLS-TCP listener and a plain-TCP listener — plus rUDP,
//! QUIC and WebSocket where noted — accepting clients into the SAME
//! room, with one shared connection-id sequence across all doors.
//!
//! Idioms are the e2e.rs ones (real server on ephemeral ports, real
//! clients, wire-level frames). Every door is a `gsb_client` connection
//! (TCP, TLS, rUDP, QUIC — quinn against `gsb_net::quic` — and the
//! WebSocket half, one masked binary message per frame).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use gsb_client::session::{self, Credentials};
use gsb_client::{ClientError, Conn, Recv};
use prost::Message;
use tokio::net::TcpStream;

mod common;

use common::{TLS_SERVER_NAME, tls_client_connector};

/// A listener-table entry for the given door kind on an ephemeral port.
fn entry(
    transport: gsb_server::ListenerTransport,
    bind: &str,
    pki: Option<&common::TlsPki>,
) -> gsb_server::ListenerEntry {
    gsb_server::ListenerEntry {
        transport,
        bind: bind.into(),
        tls_cert: pki.map(|p| p.cert_pem_path.clone()),
        tls_key: pki.map(|p| p.key_pem_path.clone()),
    }
}

/// A test client: a `gsb_client` connection over any door (the TLS and
/// QUIC arms trust ONLY the runtime-minted test CA).
struct Client(Conn);

/// Connect a client to the right kind of door.
async fn connect(
    transport: gsb_server::ListenerTransport,
    pki: &common::TlsPki,
    addr: std::net::SocketAddr,
) -> Client {
    Client(match transport {
        gsb_server::ListenerTransport::Tcp => gsb_client::connect::tcp(addr)
            .await
            .expect("plain TCP connects"),
        gsb_server::ListenerTransport::Tls => {
            let tcp = TcpStream::connect(addr)
                .await
                .expect("TCP under TLS connects");
            let dns: rustls::pki_types::ServerName<'static> =
                TLS_SERVER_NAME.try_into().expect("dns name");
            gsb_client::tls::connect(tcp, &tls_client_connector(pki), dns)
                .await
                .expect("TLS handshake")
        }
        gsb_server::ListenerTransport::Udp => gsb_client::connect::udp(addr)
            .await
            .expect("rUDP handshake"),
        // THE v1 contract: TLS 1.3 with the gsb ALPN, one bi-stream
        // carrying the same length-prefixed frames.
        gsb_server::ListenerTransport::Quic => {
            let config = gsb_client::quic::client_config([pki.ca_der.clone()]).expect("QUIC TLS");
            gsb_client::quic::connect(addr, TLS_SERVER_NAME, config)
                .await
                .expect("QUIC handshake")
        }
        // One masked binary message per frame, the door's contract.
        gsb_server::ListenerTransport::Ws => gsb_client::connect::ws(addr)
            .await
            .expect("the WebSocket door upgrades"),
    })
}

impl Client {
    async fn write_frame(&mut self, op: u16, payload: &[u8]) -> std::io::Result<()> {
        self.0.send(op, payload).await
    }

    /// Wait up to `window` for the next frame; `None` = nothing arrived
    /// (rUDP has no EOF; the stream doors' EOF surfaces as `None` here too,
    /// which no flow in this suite relies on).
    async fn recv_frame(&mut self, window: Duration) -> std::io::Result<Option<(u16, Vec<u8>)>> {
        let c = &mut self.0;
        match c.recv(window).await {
            Ok(Recv::Frame(f)) => Ok(Some((f.op, f.payload.to_vec()))),
            Ok(Recv::Closed | Recv::Quiet) => Ok(None),
            // An rUDP socket error is the test's failure; a stream
            // door's read error ends the stream like EOF.
            Err(e) if c.is_udp() => Err(e),
            Err(_) => Ok(None),
        }
    }
}

/// AUTH + JOIN coalesced onto the wire; resolves with the joiner's wire
/// entity id once the JOIN_ROOM_RESULT arrives. Interleaved frames
/// (snapshots for earlier members) are tolerated and discarded here.
async fn auth_and_join(client: &mut Client, name: &str) -> u64 {
    let c = &mut client.0;
    let creds = Credentials::named(name);
    match session::auth_and_join(c, &creds, 1, Duration::from_secs(10), |_| {}).await {
        Ok(j) => {
            assert_ne!(j.entity, 0, "{name}: entity id must be non-zero");
            j.entity
        }
        Err(ClientError::Server(m)) => {
            panic!("{name}: server error code={} message={}", m.raw, m.message)
        }
        Err(ClientError::AuthRefused(_)) => panic!("{name}: auth must succeed"),
        Err(ClientError::TimedOut) => panic!("{name}: timed out waiting for the join result"),
        Err(e) => panic!("{name}: connection ended before the join result: {e}"),
    }
}

/// Send one MOVE_TO so the room keeps producing change-driven snapshots
/// (on top of the 1 Hz keep-alive) while the test drains other clients.
async fn nudge(client: &mut Client, x: i32, y: i32) {
    let move_to = gsb_demo::game::MoveTo { x, y, seq: 0 }.encode_to_vec();
    client
        .write_frame(gsb_demo::op::MOVE_TO, &move_to)
        .await
        .expect("MOVE_TO goes out");
}

/// Read frames until ONE world snapshot contains EVERY id in `wanted`
/// (the mixed-visibility assertion), or panic at the deadline. Returns
/// the entity set of that snapshot.
async fn wait_until_sees(client_name: &str, client: &mut Client, wanted: &[u64]) -> HashSet<u64> {
    let wanted: HashSet<u64> = wanted.iter().copied().collect();
    assert!(!wanted.is_empty(), "wanted must be non-empty");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("{client_name}: timed out waiting to see {wanted:?}"));
        let (op, payload) = client
            .recv_frame(remaining)
            .await
            .expect("socket works")
            .unwrap_or_else(|| panic!("{client_name}: connection ended while waiting"));
        if op != gsb_demo::op::WORLD_SNAPSHOT {
            continue;
        }
        let m: gsb_demo::game::WorldSnapshot =
            gsb_demo::game::WorldSnapshot::decode(&payload[..]).unwrap();
        let seen: HashSet<u64> = m.entities.iter().map(|e| e.entity).collect();
        if wanted.is_subset(&seen) {
            return seen;
        }
    }
}

/// THE headline proof: a TLS-TCP door and a plain-TCP door accept two
/// clients into the SAME pre-created room, and each observes the OTHER's
/// entity in its snapshots — rooms are transport-agnostic, so mixed-
/// transport visibility just falls out of both doors feeding one pipeline.
#[tokio::test]
async fn two_listeners_serve_one_room() {
    let pki = common::mint_tls_pki("multi-two");
    let cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(vec![
            entry(
                gsb_server::ListenerTransport::Tls,
                "127.0.0.1:0",
                Some(&pki),
            ),
            entry(gsb_server::ListenerTransport::Tcp, "127.0.0.1:0", None),
        ]),
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg)
        .await
        .expect("two-listener server starts");
    assert_eq!(
        handle.addrs.len(),
        2,
        "both listeners must report their bound address"
    );
    assert_eq!(handle.addr, handle.addrs[0], "addr stays addrs[0]");

    // Client A walks the TLS door (addrs[0], config order), B the plain
    // door (addrs[1]).
    let mut tls_a = connect(gsb_server::ListenerTransport::Tls, &pki, handle.addrs[0]).await;
    let mut tcp_b = connect(gsb_server::ListenerTransport::Tcp, &pki, handle.addrs[1]).await;

    let ent_a = auth_and_join(&mut tls_a, "tls-a").await;
    let ent_b = auth_and_join(&mut tcp_b, "tcp-b").await;
    assert_ne!(
        ent_a, ent_b,
        "two joins in one room are two distinct entities"
    );
    nudge(&mut tls_a, 10, 10).await;
    nudge(&mut tcp_b, -10, -10).await;

    // Mixed-transport visibility: the TLS client's snapshots contain the
    // plaintext client's entity, and vice versa.
    let seen_a = wait_until_sees("tls-a", &mut tls_a, &[ent_b]).await;
    assert!(seen_a.contains(&ent_a));
    let seen_b = wait_until_sees("tcp-b", &mut tcp_b, &[ent_a]).await;
    assert!(seen_b.contains(&ent_b));

    handle.stop().await;
}

/// Connection ids are unique ACROSS listeners: four clients (two per door)
/// join one room and every client eventually sees ALL FOUR distinct
/// entities. If two doors ever minted the same id, the registry's tables
/// would key both connections to one row — the joins would collapse into
/// fewer entities and/or a client would stop receiving snapshots; four
/// simultaneously-visible distinct entities rules that out end to end.
#[tokio::test]
async fn connection_ids_are_unique_across_listeners() {
    let pki = common::mint_tls_pki("multi-ids");
    let cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(vec![
            entry(
                gsb_server::ListenerTransport::Tls,
                "127.0.0.1:0",
                Some(&pki),
            ),
            entry(gsb_server::ListenerTransport::Tcp, "127.0.0.1:0", None),
        ]),
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg)
        .await
        .expect("two-listener server starts");

    let mut via_tls = [
        connect(gsb_server::ListenerTransport::Tls, &pki, handle.addrs[0]).await,
        connect(gsb_server::ListenerTransport::Tls, &pki, handle.addrs[0]).await,
    ];
    let mut via_tcp = [
        connect(gsb_server::ListenerTransport::Tcp, &pki, handle.addrs[1]).await,
        connect(gsb_server::ListenerTransport::Tcp, &pki, handle.addrs[1]).await,
    ];

    let mut entities = Vec::new();
    for (i, c) in via_tls.iter_mut().enumerate() {
        entities.push(auth_and_join(c, &format!("tls-{i}")).await);
    }
    for (i, c) in via_tcp.iter_mut().enumerate() {
        entities.push(auth_and_join(c, &format!("tcp-{i}")).await);
    }
    let unique: HashSet<u64> = entities.iter().copied().collect();
    assert_eq!(
        unique.len(),
        4,
        "four joins across two doors yield four DISTINCT entities, got {entities:?}"
    );

    // Every client converges on the full four-entity world (drain order
    // does not matter: keep-alive + nudges keep snapshots flowing while
    // the others are being drained).
    for (i, c) in via_tls.iter_mut().enumerate() {
        let others: Vec<u64> = entities.to_vec();
        nudge(c, 5 + i as i32, 0).await;
        wait_until_sees(&format!("tls-{i}"), c, &others).await;
    }
    for (i, c) in via_tcp.iter_mut().enumerate() {
        let others: Vec<u64> = entities.to_vec();
        nudge(c, 0, 5 + i as i32).await;
        wait_until_sees(&format!("tcp-{i}"), c, &others).await;
    }

    handle.stop().await;
}

/// Backward compatibility: WITHOUT `[[listeners]]`, the legacy scalar keys
/// derive exactly one door with identical behavior — a scalar-TLS config
/// serves a verified rustls client, and the default (plaintext) config
/// still answers a raw TCP client; both report exactly one bound address.
#[tokio::test]
async fn legacy_single_transport_config_still_works() {
    // Scalar TLS: transport = "tcp" + both tls files (the legacy spelling).
    let pki = common::mint_tls_pki("multi-legacy");
    let legacy_tls = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        transport: gsb_server::TransportKind::Tcp,
        tls_cert: pki.cert_pem_path.clone(),
        tls_key: pki.key_pem_path.clone(),
        ..Default::default()
    };
    let handle = gsb_server::start_server(legacy_tls)
        .await
        .expect("legacy TLS config starts");
    assert_eq!(
        handle.addrs.len(),
        1,
        "no [[listeners]] means exactly one derived door"
    );
    assert_eq!(handle.addr, handle.addrs[0]);
    let mut client = connect(gsb_server::ListenerTransport::Tls, &pki, handle.addr).await;
    let ent = auth_and_join(&mut client, "legacy-tls").await;
    wait_until_sees("legacy-tls", &mut client, &[ent]).await;
    handle.stop().await;

    // Scalar plaintext default: byte-for-byte the pre-multi-listener path.
    let handle = gsb_server::start_server(gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        ..Default::default()
    })
    .await
    .expect("legacy plaintext config starts");
    assert_eq!(handle.addrs.len(), 1);
    let mut client = connect(gsb_server::ListenerTransport::Tcp, &pki, handle.addr).await;
    let ent = auth_and_join(&mut client, "legacy-plain").await;
    wait_until_sees("legacy-plain", &mut client, &[ent]).await;
    handle.stop().await;
}

/// Duplicate CONCRETE addresses are rejected at STARTUP, before any
/// socket exists — regardless of door kinds (the second bind could never
/// succeed anyway; naming it at config time points at the entry, not at a
/// bind syscall). Port 0 is deliberately exempt (each `:0` asks the OS for
/// its own free port), and every other test here relies on that.
#[tokio::test]
async fn duplicate_bind_addresses_rejected() {
    // Same port twice, same kind.
    let dup_same_kind = gsb_server::Config {
        listeners: Some(vec![
            entry(gsb_server::ListenerTransport::Tcp, "127.0.0.1:24680", None),
            entry(gsb_server::ListenerTransport::Tcp, "127.0.0.1:24680", None),
        ]),
        ..Default::default()
    };
    match gsb_server::start_server(dup_same_kind).await {
        Err(gsb_server::ServerError::DuplicateBind(a)) => {
            assert_eq!(a, "127.0.0.1:24680")
        }
        Ok(_) => panic!("duplicate bind must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }

    // Same concrete address across DIFFERENT kinds: still a duplicate —
    // the rule is over the address, not the kind.
    let pki = common::mint_tls_pki("multi-dup");
    let dup_mixed = gsb_server::Config {
        listeners: Some(vec![
            entry(gsb_server::ListenerTransport::Tcp, "127.0.0.1:24681", None),
            entry(
                gsb_server::ListenerTransport::Tls,
                "127.0.0.1:24681",
                Some(&pki),
            ),
        ]),
        ..Default::default()
    };
    match gsb_server::start_server(dup_mixed).await {
        Err(gsb_server::ServerError::DuplicateBind(a)) => {
            assert_eq!(a, "127.0.0.1:24681")
        }
        Ok(_) => panic!("duplicate bind across kinds must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }
}

/// An explicitly EMPTY `[[listeners]]` table refuses startup: zero doors
/// is never a valid deployment, and silently falling back to the scalar
/// keys would hide a half-edited config.
#[tokio::test]
async fn empty_listeners_table_refuses_startup() {
    let cfg = gsb_server::Config {
        bind: "127.0.0.1:7777".into(),
        listeners: Some(Vec::new()),
        ..Default::default()
    };
    match gsb_server::start_server(cfg).await {
        Err(gsb_server::ServerError::EmptyListeners) => {} // the contract
        Ok(_) => panic!("an empty listener table must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }
}

/// rUDP mixes into the table structurally: it is a Transport like any
/// other, so a third (udp) door accepts its cookie-handshaken sessions
/// into the SAME room as the TCP and TLS doors — three wires, one world,
/// every client ends up seeing both other entities.
#[tokio::test]
async fn three_transports_serve_one_room_including_rudp() {
    let pki = common::mint_tls_pki("multi-three");
    let cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(vec![
            entry(gsb_server::ListenerTransport::Tcp, "127.0.0.1:0", None),
            entry(
                gsb_server::ListenerTransport::Tls,
                "127.0.0.1:0",
                Some(&pki),
            ),
            entry(gsb_server::ListenerTransport::Udp, "127.0.0.1:0", None),
        ]),
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg)
        .await
        .expect("three-listener server starts");
    assert_eq!(handle.addrs.len(), 3);

    let mut tcp_c = connect(gsb_server::ListenerTransport::Tcp, &pki, handle.addrs[0]).await;
    let mut tls_c = connect(gsb_server::ListenerTransport::Tls, &pki, handle.addrs[1]).await;
    let mut udp_c = connect(gsb_server::ListenerTransport::Udp, &pki, handle.addrs[2]).await;

    let ent_tcp = auth_and_join(&mut tcp_c, "door-tcp").await;
    let ent_tls = auth_and_join(&mut tls_c, "door-tls").await;
    let ent_udp = auth_and_join(&mut udp_c, "door-udp").await;
    let all: HashSet<u64> = [ent_tcp, ent_tls, ent_udp].into_iter().collect();
    assert_eq!(all.len(), 3, "three joins are three distinct entities");

    nudge(&mut tcp_c, 7, 7).await;
    nudge(&mut tls_c, -7, 7).await;
    nudge(&mut udp_c, 7, -7).await;

    // Each door's client converges on the other two (and itself — the
    // snapshot is the whole group's view either way).
    wait_until_sees("tcp", &mut tcp_c, &[ent_tls, ent_udp]).await;
    wait_until_sees("tls", &mut tls_c, &[ent_tcp, ent_udp]).await;
    wait_until_sees("udp", &mut udp_c, &[ent_tcp, ent_tls]).await;

    handle.stop().await;
}

/// THE QUIC door joins the table structurally: a real quinn client (own
/// mini-PKI, ALPN agreed, one bi-stream) walks the "quic" door while a
/// plain-TCP client walks a second door of the SAME room — and each
/// observes the OTHER's entity. This locks the whole chain this turn was
/// about: `[[listeners]]` grammar ("quic" + both PEM files) → per-entry
/// bind → accept task → shared pipeline → one transport-agnostic room.
#[tokio::test]
async fn quic_and_tcp_doors_serve_one_room() {
    let pki = common::mint_tls_pki("multi-quic");
    let cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(vec![
            entry(
                gsb_server::ListenerTransport::Quic,
                "127.0.0.1:0",
                Some(&pki),
            ),
            entry(gsb_server::ListenerTransport::Tcp, "127.0.0.1:0", None),
        ]),
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg)
        .await
        .expect("quic+tcp server starts");
    assert_eq!(handle.addrs.len(), 2);

    let mut quic_c = connect(gsb_server::ListenerTransport::Quic, &pki, handle.addrs[0]).await;
    let mut tcp_c = connect(gsb_server::ListenerTransport::Tcp, &pki, handle.addrs[1]).await;

    let ent_quic = auth_and_join(&mut quic_c, "door-quic").await;
    let ent_tcp = auth_and_join(&mut tcp_c, "door-tcp").await;
    assert_ne!(
        ent_quic, ent_tcp,
        "two joins in one room are two distinct entities"
    );
    nudge(&mut quic_c, 12, 12).await;
    nudge(&mut tcp_c, -12, -12).await;

    // Mixed-transport visibility across a QUIC door and a TCP door.
    let seen_quic = wait_until_sees("quic", &mut quic_c, &[ent_tcp]).await;
    assert!(seen_quic.contains(&ent_quic));
    let seen_tcp = wait_until_sees("tcp", &mut tcp_c, &[ent_quic]).await;
    assert!(seen_tcp.contains(&ent_tcp));

    handle.stop().await;
}

/// The WebSocket door mixes with rUDP the same way: a raw-TCP RFC 6455
/// client (real upgrade handshake, masked binary messages carrying exactly
/// one game frame each) and a cookie-handshaken rUDP client join ONE room
/// and converge on each other's entity.
#[tokio::test]
async fn websocket_and_rudp_doors_serve_one_room() {
    let pki = common::mint_tls_pki("multi-ws");
    let cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(vec![
            entry(gsb_server::ListenerTransport::Ws, "127.0.0.1:0", None),
            entry(gsb_server::ListenerTransport::Udp, "127.0.0.1:0", None),
        ]),
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg)
        .await
        .expect("ws+udp server starts");
    assert_eq!(handle.addrs.len(), 2);

    let mut ws_c = connect(gsb_server::ListenerTransport::Ws, &pki, handle.addrs[0]).await;
    let mut udp_c = connect(gsb_server::ListenerTransport::Udp, &pki, handle.addrs[1]).await;

    let ent_ws = auth_and_join(&mut ws_c, "door-ws").await;
    let ent_udp = auth_and_join(&mut udp_c, "door-udp").await;
    assert_ne!(ent_ws, ent_udp);
    nudge(&mut ws_c, 14, -14).await;
    nudge(&mut udp_c, -14, 14).await;

    let seen_ws = wait_until_sees("ws", &mut ws_c, &[ent_udp]).await;
    assert!(seen_ws.contains(&ent_ws));
    let seen_udp = wait_until_sees("udp", &mut udp_c, &[ent_ws]).await;
    assert!(seen_udp.contains(&ent_udp));

    handle.stop().await;
}

/// A "tls" entry carries the same both-files rule, locked in ITS corrected
/// direction too (the QUIC arm copied this family's convention): each
/// half-set entry is named by the message that describes IT.
#[tokio::test]
async fn tls_entry_without_both_tls_files_refuses_startup() {
    let pki = common::mint_tls_pki("multi-tls-cfg");

    // Cert without key → the message says exactly that.
    let cert_only = gsb_server::Config {
        listeners: Some(vec![gsb_server::ListenerEntry {
            transport: gsb_server::ListenerTransport::Tls,
            bind: "127.0.0.1:0".into(),
            tls_cert: Some(pki.cert_pem_path.clone()),
            tls_key: None,
        }]),
        ..Default::default()
    };
    match gsb_server::start_server(cert_only).await {
        Err(gsb_server::ServerError::ListenerTlsCertNeedsKey { .. }) => {}
        Ok(_) => panic!("tls entry with cert only must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }

    // Key without cert → likewise.
    let key_only = gsb_server::Config {
        listeners: Some(vec![gsb_server::ListenerEntry {
            transport: gsb_server::ListenerTransport::Tls,
            bind: "127.0.0.1:0".into(),
            tls_cert: None,
            tls_key: Some(pki.key_pem_path.clone()),
        }]),
        ..Default::default()
    };
    match gsb_server::start_server(key_only).await {
        Err(gsb_server::ServerError::ListenerTlsKeyNeedsCert { .. }) => {}
        Ok(_) => panic!("tls entry with key only must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }
}

/// A "quic" entry carries the SAME both-files rule as "tls": cert without
/// key, key without cert, and NEITHER file all refuse STARTUP with the
/// error naming the offending entry (QUIC is TLS 1.3 underneath; there is
/// no anonymous or half-configured spelling to silently fall back to).
#[tokio::test]
async fn quic_entry_without_both_tls_files_refuses_startup() {
    let pki = common::mint_tls_pki("multi-quic-cfg");

    let cert_only = gsb_server::Config {
        listeners: Some(vec![gsb_server::ListenerEntry {
            transport: gsb_server::ListenerTransport::Quic,
            bind: "127.0.0.1:0".into(),
            tls_cert: Some(pki.cert_pem_path.clone()),
            tls_key: None,
        }]),
        ..Default::default()
    };
    match gsb_server::start_server(cert_only).await {
        Err(gsb_server::ServerError::ListenerQuicCertNeedsKey { bind }) => {
            assert_eq!(bind, "127.0.0.1:0")
        }
        Ok(_) => panic!("quic entry with cert only must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }

    let key_only = gsb_server::Config {
        listeners: Some(vec![gsb_server::ListenerEntry {
            transport: gsb_server::ListenerTransport::Quic,
            bind: "127.0.0.1:0".into(),
            tls_cert: None,
            tls_key: Some(pki.key_pem_path.clone()),
        }]),
        ..Default::default()
    };
    match gsb_server::start_server(key_only).await {
        Err(gsb_server::ServerError::ListenerQuicKeyNeedsCert { .. }) => {}
        Ok(_) => panic!("quic entry with key only must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }

    // Neither file: the FIRST missing file is reported (the cert — the
    // same one-check-per-missing-file order the "tls" arm locks above).
    let neither = gsb_server::Config {
        listeners: Some(vec![entry(
            gsb_server::ListenerTransport::Quic,
            "127.0.0.1:0",
            None,
        )]),
        ..Default::default()
    };
    match gsb_server::start_server(neither).await {
        Err(gsb_server::ServerError::ListenerQuicKeyNeedsCert { .. }) => {}
        Ok(_) => panic!("quic entry with no files must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }
}

/// A "ws" entry takes NO tls files: the WebSocket door upgrades PLAIN TCP
/// today, and files attached to it are a config mistake refused at startup
/// — never a silent reinterpretation into some wss:// door.
#[tokio::test]
async fn ws_entry_with_tls_files_refuses_startup() {
    let pki = common::mint_tls_pki("multi-ws-cfg");
    let cfg = gsb_server::Config {
        listeners: Some(vec![gsb_server::ListenerEntry {
            transport: gsb_server::ListenerTransport::Ws,
            bind: "127.0.0.1:0".into(),
            tls_cert: Some(pki.cert_pem_path.clone()),
            tls_key: Some(pki.key_pem_path.clone()),
        }]),
        ..Default::default()
    };
    match gsb_server::start_server(cfg).await {
        Err(gsb_server::ServerError::ListenerWsWithTls { bind }) => {
            assert_eq!(bind, "127.0.0.1:0")
        }
        Ok(_) => panic!("ws entry with tls files must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }
}
