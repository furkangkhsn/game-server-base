//! Multi-listener end-to-end tests: SEVERAL transport doors serving ONE
//! map/room simultaneously (the composition-root contract this suite
//! locks): a TLS-TCP listener and a plain-TCP listener — plus rUDP,
//! QUIC and WebSocket where noted — accepting clients into the SAME
//! room, with one shared connection-id sequence across all doors.
//!
//! Idioms are the e2e.rs ones (real server on ephemeral ports, real
//! clients, wire-level frames); the `Client` enum is the same five-arm
//! transport split, minus the flows this suite does not exercise. The
//! QUIC and WS arms speak real handshakes to real doors: quinn against
//! `gsb_net::quic`, and a raw-TCP RFC 6455 client (adapted from the
//! gsb-net ws.rs suite) against `gsb_net::ws`.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use gsb_protocol::base::{Auth, AuthResult, Error, JoinRoom, JoinRoomResult};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
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

/// Which wire a test client sits on. Same shape as e2e's `Client`; the
/// TLS arm trusts ONLY the runtime-minted test CA (the QUIC arm too).
enum Client {
    Tcp(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    Udp(Box<gsb_net::udp::UdpClient>),
    Quic(QuicClient),
    Ws(FakeWsClient),
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
            let tcp = TcpStream::connect(addr)
                .await
                .expect("TCP under TLS connects");
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
        gsb_server::ListenerTransport::Quic => Client::Quic(connect_quic(pki, addr).await),
        gsb_server::ListenerTransport::Ws => Client::Ws(FakeWsClient::connect(addr).await),
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
            // QUIC v1 contract: the same length-prefixed frame, written
            // onto THE single bi-stream.
            Client::Quic(c) => {
                c.send.write_all(&framed(op, payload)).await?;
                c.send.flush().await
            }
            Client::Ws(c) => c.send_game(op, payload).await,
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
            Client::Udp(c) => Ok(c
                .recv_frame(window)
                .await?
                .map(|f| (f.op, f.payload.to_vec()))),
            Client::Quic(c) => Ok(tokio::time::timeout(window, read_quic_frame(&mut c.recv))
                .await
                .ok()
                .flatten()),
            Client::Ws(c) => Ok(tokio::time::timeout(window, c.recv_game())
                .await
                .ok()
                .flatten()),
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

// ── minimal QUIC test client ────────────────────────────────────────────

/// A minimal QUIC client speaking THE v1 wire contract (`gsb_net::quic`):
/// trust ONLY the runtime-minted CA, agree on ALPN, open exactly ONE
/// bidirectional stream, then treat its halves as a plain length-prefixed
/// frame pipe — byte-for-byte what the server-side pumps see.
struct QuicClient {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

async fn connect_quic(pki: &common::TlsPki, addr: std::net::SocketAddr) -> QuicClient {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(pki.ca_der.clone()).expect("CA parses");
    // A fresh provider instance per connector (never a global install):
    // several clients/servers are built across one test binary's run.
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![gsb_net::quic::ALPN_PROTOCOL.to_vec()];
    let crypto =
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC client TLS setup");
    let client_config = quinn::ClientConfig::new(std::sync::Arc::new(crypto));
    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("client endpoint");
    endpoint.set_default_client_config(client_config);
    // The endpoint handle is dropped on return ON PURPOSE (the same
    // reasoning gsb-net's quic.rs documents for its own test connector):
    // quinn's driver keeps serving the connection until its last stream
    // handle is gone, so the held send/recv pair stays fully usable.
    let conn = endpoint
        .connect(addr, TLS_SERVER_NAME)
        .expect("connect setup")
        .await
        .expect("QUIC handshake");
    // THE v1 contract: one bi-stream, opened immediately.
    let (send, recv) = conn.open_bi().await.expect("bi-stream open");
    QuicClient { send, recv }
}

/// One length-prefixed frame off the QUIC bi-stream (same wire shape as
/// [`read_stream_frame`]; quinn's native `read_exact` instead of the
/// `AsyncRead` adapter).
async fn read_quic_frame(recv: &mut quinn::RecvStream) -> Option<(u16, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > 4 * 1024 * 1024 {
        return None;
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await.ok()?;
    if body.len() < 2 {
        return None;
    }
    Some((u16::from_le_bytes([body[0], body[1]]), body[2..].to_vec()))
}

// ── fake WS client (raw TCP, masked frames; adapted from the gsb-net
//    ws.rs suite, which owns this protocol's unit tests) ────────────────

const RFC_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
/// base64(SHA-1(RFC_KEY + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")) — the
/// RFC 6455 §1.3 vector, so the accept-key check needs no hashing here.
const RFC_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
const WS_OP_BIN: u8 = 0x2;

/// Deterministic mask keys (RFC masking defeats proxy caching, not tests).
struct MaskGen(u32);
impl MaskGen {
    fn next(&mut self) -> [u8; 4] {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0.to_be_bytes()
    }
}

/// One masked client frame: single FIN binary message carrying `payload`.
fn encode_client_ws_frame(payload: &[u8], key: [u8; 4]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.push(0x80 | WS_OP_BIN);
    let mask_bit = 0x80;
    let l7 = payload.len();
    if l7 < 126 {
        frame.push(mask_bit | l7 as u8);
    } else if l7 <= u16::MAX as usize {
        frame.push(mask_bit | 126);
        frame.extend_from_slice(&(l7 as u16).to_be_bytes());
    } else {
        frame.push(mask_bit | 127);
        frame.extend_from_slice(&(l7 as u64).to_be_bytes());
    }
    frame.extend_from_slice(&key);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i & 3]));
    frame
}

struct FakeWsClient {
    stream: TcpStream,
    masks: MaskGen,
}

impl FakeWsClient {
    /// Real RFC 6455 opening handshake against the server door; the 101
    /// response's Sec-WebSocket-Accept is verified against the RFC vector.
    async fn connect(addr: std::net::SocketAddr) -> Self {
        let mut stream = TcpStream::connect(addr).await.expect("ws tcp connect");
        let request = format!(
            "GET /gsb HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {RFC_KEY}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("handshake write");
        let head = read_http_head(&mut stream).await;
        assert!(
            head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
            "want 101, got: {head}"
        );
        let accept = header_value(&head, "sec-websocket-accept")
            .unwrap_or_else(|| panic!("101 must carry Sec-WebSocket-Accept, got: {head}"));
        assert_eq!(accept, RFC_ACCEPT, "accept key must match the RFC vector");
        Self {
            stream,
            masks: MaskGen(42),
        }
    }

    /// Send one game frame inside ONE masked binary WS message (each
    /// message carries exactly one length-prefixed game frame — the wire
    /// contract the ws door enforces).
    async fn send_game(&mut self, op: u16, payload: &[u8]) -> std::io::Result<()> {
        let body = 2 + payload.len();
        let mut msg = Vec::with_capacity(4 + body);
        msg.extend_from_slice(&(body as u32).to_le_bytes());
        msg.extend_from_slice(&op.to_le_bytes());
        msg.extend_from_slice(payload);
        let frame = encode_client_ws_frame(&msg, self.masks.next());
        self.stream.write_all(&frame).await?;
        self.stream.flush().await
    }

    /// Read the next binary game frame. Single-read ON PURPOSE: the ws
    /// door never originates control frames (pongs ride the same queue
    /// only as ping REPLIES, and these flows send no pings), so one
    /// server message = one binary game frame; anything else is a flow
    /// break worth panicking on, not skipping.
    async fn recv_game(&mut self) -> Option<(u16, Vec<u8>)> {
        let (fin, opcode, payload) = self.read_frame().await?;
        assert_ne!(opcode, 0x8, "unexpected close mid-flow");
        assert_eq!(
            opcode, WS_OP_BIN,
            "game frames ride binary messages, got {opcode:#x}"
        );
        assert!(fin, "server messages are single-frame in these flows");
        let declared =
            u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        assert_eq!(
            payload.len(),
            4 + declared,
            "exactly one length-prefixed game frame per message"
        );
        let op = u16::from_le_bytes([payload[4], payload[5]]);
        Some((op, payload[6..].to_vec()))
    }

    /// Read one unmasked server frame: `(fin, opcode, payload)`.
    async fn read_frame(&mut self) -> Option<(bool, u8, Vec<u8>)> {
        let mut head = [0u8; 2];
        self.stream.read_exact(&mut head).await.ok()?;
        let fin = head[0] & 0x80 != 0;
        assert_eq!(head[1] & 0x80, 0, "server must never mask");
        let l7 = (head[1] & 0x7f) as usize;
        let len = match l7 {
            0x7e => {
                let mut ext = [0u8; 2];
                self.stream.read_exact(&mut ext).await.ok()?;
                u16::from_be_bytes(ext) as usize
            }
            0x7f => {
                let mut ext = [0u8; 8];
                self.stream.read_exact(&mut ext).await.ok()?;
                u64::from_be_bytes(ext) as usize
            }
            n => n,
        };
        let mut payload = vec![0u8; len];
        if len > 0 {
            self.stream.read_exact(&mut payload).await.ok()?;
        }
        Some((fin, head[0] & 0x0f, payload))
    }
}

async fn read_http_head(stream: &mut TcpStream) -> String {
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while buf.windows(4).position(|w| w == b"\r\n\r\n").is_none() {
        let n = stream.read(&mut byte).await.expect("http head read");
        assert!(n > 0, "EOF before the HTTP response head ended");
        buf.push(byte[0]);
        assert!(buf.len() < 16 * 1024, "response head runaway");
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (n, v) = line.split_once(':')?;
        n.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// AUTH + JOIN coalesced onto the wire; resolves with the joiner's wire
/// entity id once the JOIN_ROOM_RESULT arrives. Interleaved frames
/// (snapshots for earlier members) are tolerated and discarded here.
async fn auth_and_join(client: &mut Client, name: &str) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    let auth = Auth {
        name: name.into(),
        ticket: vec![],
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    };
    client
        .write_frame(gsb_protocol::op::base::AUTH_REQ, &auth.encode_to_vec())
        .await
        .expect("AUTH_REQ goes out");
    let join = JoinRoom { room_id: 1 };
    client
        .write_frame(gsb_protocol::op::base::JOIN_ROOM_REQ, &join.encode_to_vec())
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
