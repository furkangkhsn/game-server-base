//! Multi-listener end-to-end tests: SEVERAL transport doors serving ONE
//! map/room simultaneously (the composition-root contract this suite
//! locks): a TLS-TCP listener and a plain-TCP listener — plus rUDP where
//! noted — accepting clients into the SAME room, with one shared
//! connection-id sequence across all doors.
//!
//! Idioms are the e2e.rs ones (real server on ephemeral ports, real
//! clients, wire-level frames); the `Client` enum is the same three-arm
//! transport split, minus the flows this suite does not exercise.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use gsb_protocol::base::{Auth, AuthResult, Error, JoinRoom, JoinRoomResult};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

mod common;

use common::{tls_client_connector, TLS_SERVER_NAME};

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

/// Which wire a test client sits on. Same shape as e2e's `Client`; the
/// TLS arm trusts ONLY the runtime-minted test CA.
enum Client {
    Tcp(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    Udp(Box<gsb_net::udp::UdpClient>),
}

/// Connect a client to the right kind of door.
async fn connect(
    transport: gsb_server::ListenerTransport,
    pki: &common::TlsPki,
    addr: std::net::SocketAddr,
) -> Client {
    match transport {
        gsb_server::ListenerTransport::Tcp => {
            let s = TcpStream::connect(addr).await.expect("plain TCP connects");
            Client::Tcp(s)
        }
        gsb_server::ListenerTransport::Tls => {
            let tcp = TcpStream::connect(addr).await.expect("TCP under TLS connects");
            tcp.set_nodelay(true).ok();
            let connector = tls_client_connector(pki);
            let dns: rustls::pki_types::ServerName<'static> =
                TLS_SERVER_NAME.try_into().expect("dns name");
            let t = connector.connect(dns, tcp).await.expect("TLS handshake");
            Client::Tls(Box::new(t))
        }
        gsb_server::ListenerTransport::Udp => {
            let c = gsb_net::udp::UdpClient::connect(addr)
                .await
                .expect("rUDP handshake");
            Client::Udp(Box::new(c))
        }
    }
}

impl Client {
    async fn write_frame(&mut self, op: u16, payload: &[u8]) -> std::io::Result<()> {
        fn framed(op: u16, payload: &[u8]) -> Vec<u8> {
            let body = 2 + payload.len();
            let mut out = Vec::with_capacity(4 + body);
            out.extend_from_slice(&(body as u32).to_le_bytes());
            out.extend_from_slice(&op.to_le_bytes());
            out.extend_from_slice(payload);
            out
        }
        match self {
            Client::Tcp(s) => {
                s.write_all(&framed(op, payload)).await?;
                s.flush().await
            }
            Client::Tls(t) => {
                t.write_all(&framed(op, payload)).await?;
                t.flush().await
            }
            Client::Udp(c) => c.send_frame(op, payload.to_vec()).await,
        }
    }

    /// Wait up to `window` for the next frame; `None` = nothing arrived
    /// (rUDP has no EOF; the stream doors' EOF surfaces as `None` here too,
    /// which no flow in this suite relies on).
    async fn recv_frame(&mut self, window: Duration) -> std::io::Result<Option<(u16, Vec<u8>)>> {
        match self {
            Client::Tcp(s) => Ok(tokio::time::timeout(window, read_stream_frame(s))
                .await
                .ok()
                .flatten()),
            Client::Tls(t) => Ok(tokio::time::timeout(window, read_stream_frame(t.as_mut()))
                .await
                .ok()
                .flatten()),
            Client::Udp(c) => Ok(c.recv_frame(window).await?.map(|f| (f.op, f.payload.to_vec()))),
        }
    }
}

/// Length-prefixed frame read over a stream door (the TCP wire shape,
/// which is also the TLS wire shape — that identity is part of the design).
async fn read_stream_frame<R: AsyncRead + Unpin>(stream: &mut R) -> Option<(u16, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > 4 * 1024 * 1024 {
        return None;
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.ok()?;
    if body.len() < 2 {
        return None;
    }
    let op = u16::from_le_bytes([body[0], body[1]]);
    Some((op, body[2..].to_vec()))
}

/// AUTH + JOIN coalesced onto the wire; resolves with the joiner's wire
/// entity id once the JOIN_ROOM_RESULT arrives. Interleaved frames
/// (snapshots for earlier members) are tolerated and discarded here.
async fn auth_and_join(client: &mut Client, name: &str) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    let auth = Auth {
        name: name.into(),
        ticket: vec![],
    };
    client
        .write_frame(gsb_protocol::op::base::AUTH_REQ, &auth.encode_to_vec())
        .await
        .expect("AUTH_REQ goes out");
    let join = JoinRoom { room_id: 1 };
    client
        .write_frame(
            gsb_protocol::op::base::JOIN_ROOM_REQ,
            &join.encode_to_vec(),
        )
        .await
        .expect("JOIN_ROOM_REQ goes out");

    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("{name}: timed out waiting for the join result"));
        let (op, payload) = client
            .recv_frame(remaining)
            .await
            .expect("socket works")
            .unwrap_or_else(|| panic!("{name}: connection ended before the join result"));
        match op {
            gsb_protocol::op::base::AUTH_RESULT => {
                let m: AuthResult = AuthResult::decode(&payload[..]).unwrap();
                assert!(m.ok, "{name}: auth must succeed");
            }
            gsb_protocol::op::base::JOIN_ROOM_RESULT => {
                let m: JoinRoomResult = JoinRoomResult::decode(&payload[..]).unwrap();
                assert_ne!(m.entity, 0, "{name}: entity id must be non-zero");
                return m.entity;
            }
            gsb_protocol::op::base::ERROR => {
                let m: Error = Error::decode(&payload[..]).unwrap();
                panic!("{name}: server error code={} message={}", m.code, m.message);
            }
            _ => {} // snapshots racing ahead of the join result
        }
    }
}

/// Send one MOVE_TO so the room keeps producing change-driven snapshots
/// (on top of the 1 Hz keep-alive) while the test drains other clients.
async fn nudge(client: &mut Client, x: i32, y: i32) {
    let move_to = gsb_game::game::MoveTo { x, y, seq: 0 }.encode_to_vec();
    client
        .write_frame(gsb_game::op::MOVE_TO, &move_to)
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
        if op != gsb_game::op::WORLD_SNAPSHOT {
            continue;
        }
        let m: gsb_game::game::WorldSnapshot =
            gsb_game::game::WorldSnapshot::decode(&payload[..]).unwrap();
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
    let mut tls_a = connect(
        gsb_server::ListenerTransport::Tls,
        &pki,
        handle.addrs[0],
    )
    .await;
    let mut tcp_b =
        connect(gsb_server::ListenerTransport::Tcp, &pki, handle.addrs[1]).await;

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
        connect(
            gsb_server::ListenerTransport::Tls,
            &pki,
            handle.addrs[0],
        )
        .await,
        connect(
            gsb_server::ListenerTransport::Tls,
            &pki,
            handle.addrs[0],
        )
        .await,
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
    let mut client = connect(
        gsb_server::ListenerTransport::Tls,
        &pki,
        handle.addr,
    )
    .await;
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
    let mut client = connect(
        gsb_server::ListenerTransport::Tcp,
        &pki,
        handle.addr,
    )
    .await;
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

    let mut tcp_c =
        connect(gsb_server::ListenerTransport::Tcp, &pki, handle.addrs[0]).await;
    let mut tls_c = connect(
        gsb_server::ListenerTransport::Tls,
        &pki,
        handle.addrs[1],
    )
    .await;
    let mut udp_c = connect(
        gsb_server::ListenerTransport::Udp,
        &pki,
        handle.addrs[2],
    )
    .await;

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
