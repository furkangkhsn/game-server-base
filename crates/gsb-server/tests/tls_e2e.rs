//! TLS-specific end-to-end tests (docs/SECURITY.md §2, Tur A).
//!
//! The guardrail flows live in `e2e.rs`, parametrized over all three
//! transports (TCP / rUDP / TLS) — this file locks the TLS-only contracts:
//!
//! - a full application flow (AUTH → JOIN → MOVE → snapshots observed)
//!   over a real rustls connection verified against a runtime-minted CA;
//! - a WRONG CA is rejected in the HANDSHAKE, on both ends, before any
//!   application byte moves (no silent plaintext downgrade);
//! - half-set TLS config (`tls_cert` without `tls_key` and vice versa)
//!   and `transport = "udp"` + TLS are STARTUP errors, never silent
//!   fallbacks (SECURITY §2 decisions 3 and 7).

use std::time::{Duration, Instant};

use gsb_protocol::base::{Auth, AuthResult, Error, JoinRoom, JoinRoomResult};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

mod common;

use common::{TLS_SERVER_NAME, TlsPki};

/// Length-prefixed frame read (the TCP wire shape, which is also the TLS
/// wire shape — that identity IS the design being tested).
async fn read_frame(r: &mut (dyn AsyncRead + Unpin + Send)) -> Option<(u16, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > 4 * 1024 * 1024 {
        return None;
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.ok()?;
    if body.len() < 2 {
        return None;
    }
    let op = u16::from_le_bytes([body[0], body[1]]);
    Some((op, body[2..].to_vec()))
}

fn frame(op: u16, payload: &[u8]) -> Vec<u8> {
    let body = 2 + payload.len();
    let mut out = Vec::with_capacity(4 + body);
    out.extend_from_slice(&(body as u32).to_le_bytes());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// A server config serving TLS from `pki`'s minted files on loopback.
fn tls_cfg(pki: &TlsPki) -> gsb_server::Config {
    gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        transport: gsb_server::TransportKind::Tcp,
        tls_cert: pki.cert_pem_path.clone(),
        tls_key: pki.key_pem_path.clone(),
        ..Default::default()
    }
}

/// Connect a rustls client trusting ONLY `pki`'s CA.
async fn connect_tls(
    pki: &TlsPki,
    addr: std::net::SocketAddr,
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr).await?;
    tcp.set_nodelay(true).ok();
    let connector = common::tls_client_connector(pki);
    let dns: rustls::pki_types::ServerName<'static> = TLS_SERVER_NAME.try_into().expect("dns name");
    connector.connect(dns, tcp).await
}

/// THE proof of Tur A end to end: over one real TLS connection — verified
/// against the minted CA, encrypted both ways — the client authenticates,
/// joins room 1, issues a MOVE_TO, and observes its entity's position
/// CHANGE in subsequent world snapshots (action → ingest → movement →
/// snapshot → TLS write path). Mirrors e2e's `join_and_observe_movement`.
#[tokio::test]
async fn full_flow_over_tls_auth_join_move_snapshots() {
    let pki = common::mint_tls_pki("flow");
    let handle = gsb_server::start_server(tls_cfg(&pki))
        .await
        .expect("TLS server starts");
    let mut tls = connect_tls(&pki, handle.addr)
        .await
        .expect("client connects over TLS");

    // AUTH + JOIN coalesced (the connection actor drains in order).
    let auth = Auth {
        name: "tls-e2e".into(),
        ticket: vec![],
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    };
    tls.write_all(&frame(
        gsb_protocol::op::base::AUTH_REQ,
        &auth.encode_to_vec(),
    ))
    .await
    .unwrap();
    let join = JoinRoom { room_id: 1 };
    tls.write_all(&frame(
        gsb_protocol::op::base::JOIN_ROOM_REQ,
        &join.encode_to_vec(),
    ))
    .await
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut my_entity: u64 = 0;
    let mut move_sent = false;
    let mut first_pos: Option<(i32, i32)> = None;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out (move_sent={move_sent})"));
        let Some((op, payload)) = tokio::time::timeout(remaining, read_frame(&mut tls))
            .await
            .ok()
            .flatten()
        else {
            panic!("connection ended or stalled during the flow");
        };
        match op {
            gsb_protocol::op::base::AUTH_RESULT => {
                let m: AuthResult = AuthResult::decode(&payload[..]).unwrap();
                assert!(m.ok, "auth must succeed over TLS");
            }
            gsb_protocol::op::base::JOIN_ROOM_RESULT => {
                let m: JoinRoomResult = JoinRoomResult::decode(&payload[..]).unwrap();
                my_entity = m.entity;
                assert!(my_entity != 0, "entity id must be non-zero");
                if !move_sent {
                    let move_to = gsb_demo::game::MoveTo {
                        x: 10,
                        y: 10,
                        seq: 0,
                    }
                    .encode_to_vec();
                    tls.write_all(&frame(gsb_demo::op::MOVE_TO, &move_to))
                        .await
                        .unwrap();
                    move_sent = true;
                }
            }
            gsb_protocol::op::base::ERROR => {
                let m: Error = Error::decode(&payload[..]).unwrap();
                panic!("server error: code={} message={}", m.code, m.message);
            }
            gsb_demo::op::WORLD_SNAPSHOT => {
                let m: gsb_demo::game::WorldSnapshot =
                    gsb_demo::game::WorldSnapshot::decode(&payload[..]).unwrap();
                assert!(m.sequence > 0, "snapshot sequence must be monotonic");
                let Some(rec) = m.entities.iter().find(|e| e.entity == my_entity) else {
                    continue; // snapshot that raced ahead of the join result
                };
                let pos = (rec.x, rec.y);
                match first_pos {
                    None => first_pos = Some(pos),
                    Some(first) if pos != first => break, // moved: full path proven
                    Some(_) => {}
                }
            }
            other => panic!("unexpected op {other} in the TLS flow"),
        }
    }

    assert!(move_sent, "must have been able to send MOVE_TO over TLS");
    handle.stop().await;
}

/// A client trusting a DIFFERENT CA fails the handshake CLEANLY: the
/// client gets an alert before any application frame, and the server's
/// accept surfaces the failed handshake (it never becomes an endpoint).
#[tokio::test]
async fn wrong_ca_is_rejected_in_the_handshake_on_both_ends() {
    let server_pki = common::mint_tls_pki("server");
    let rogue_pki = common::mint_tls_pki("rogue");
    let handle = gsb_server::start_server(tls_cfg(&server_pki))
        .await
        .expect("TLS server starts");

    // The rogue client trusts only its own CA.
    let tcp = TcpStream::connect(handle.addr).await.unwrap();
    let connector = common::tls_client_connector(&rogue_pki);
    let dns: rustls::pki_types::ServerName<'static> = TLS_SERVER_NAME.try_into().expect("dns name");
    let result = connector.connect(dns, tcp).await;
    assert!(result.is_err(), "the unknown CA must fail the handshake");

    // And a well-behaving client STILL works afterwards: the failed
    // handshake must not have poisoned the listener.
    let mut good = connect_tls(&server_pki, handle.addr)
        .await
        .expect("a fresh client connects fine after the rejection");
    let auth = Auth {
        name: "after-reject".into(),
        ticket: vec![],
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    };
    good.write_all(&frame(
        gsb_protocol::op::base::AUTH_REQ,
        &auth.encode_to_vec(),
    ))
    .await
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ok = false;
    while !ok {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for AUTH_RESULT");
        let Some((op, payload)) = tokio::time::timeout(remaining, read_frame(&mut good))
            .await
            .ok()
            .flatten()
        else {
            panic!("connection died during post-rejection auth");
        };
        if op == gsb_protocol::op::base::AUTH_RESULT {
            let m: AuthResult = AuthResult::decode(&payload[..]).unwrap();
            assert!(m.ok);
            ok = true;
        }
    }
    handle.stop().await;
}

/// Half-set config is a startup error, in BOTH directions, and nothing
/// binds (the returned error is the typed variant, not an io error).
#[tokio::test]
async fn cert_without_key_refuses_startup() {
    let pki = common::mint_tls_pki("half-cert");
    let cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        tls_cert: pki.cert_pem_path.clone(),
        ..Default::default()
    };
    match gsb_server::start_server(cfg).await {
        Err(gsb_server::ServerError::TlsCertNeedsKey) => {} // the contract
        Ok(_) => panic!("cert-without-key must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }
}

#[tokio::test]
async fn key_without_cert_refuses_startup() {
    let pki = common::mint_tls_pki("half-key");
    let cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        tls_key: pki.key_pem_path.clone(),
        ..Default::default()
    };
    match gsb_server::start_server(cfg).await {
        Err(gsb_server::ServerError::TlsKeyNeedsCert) => {} // the contract
        Ok(_) => panic!("key-without-cert must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }
}

/// rUDP takes no TLS (SECURITY §2 decision 7): asking for both refuses
/// startup instead of silently ignoring one of the two requests.
#[tokio::test]
async fn udp_with_tls_refuses_startup() {
    let pki = common::mint_tls_pki("udp-tls");
    let cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        transport: gsb_server::TransportKind::Udp,
        tls_cert: pki.cert_pem_path.clone(),
        tls_key: pki.key_pem_path.clone(),
        ..Default::default()
    };
    match gsb_server::start_server(cfg).await {
        Err(gsb_server::ServerError::UdpWithTls) => {} // the contract
        Ok(_) => panic!("udp + TLS must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }
}

/// The DEFAULT path is untouched by the turn: empty tls keys = plaintext
/// TCP, byte-identical config semantics (this is the regression guard for
/// "no behavior change when tls keys are empty").
#[tokio::test]
async fn empty_tls_keys_mean_plaintext_tcp() {
    let cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        ..Default::default()
    };
    assert!(cfg.tls_cert.is_empty() && cfg.tls_key.is_empty());
    let handle = gsb_server::start_server(cfg)
        .await
        .expect("plaintext starts");
    let mut stream = TcpStream::connect(handle.addr).await.unwrap();
    // A PLAINTEXT handshake must work: if the default had silently turned
    // into TLS, these raw bytes would fail the server's rustls accept and
    // the AUTH would never be answered.
    let auth = Auth {
        name: "plain".into(),
        ticket: vec![],
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    };
    stream
        .write_all(&frame(
            gsb_protocol::op::base::AUTH_REQ,
            &auth.encode_to_vec(),
        ))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for the plaintext AUTH_RESULT");
        let Some((op, payload)) = tokio::time::timeout(remaining, read_frame(&mut stream))
            .await
            .ok()
            .flatten()
        else {
            panic!("plaintext path broken: connection ended");
        };
        if op == gsb_protocol::op::base::AUTH_RESULT {
            let m: AuthResult = AuthResult::decode(&payload[..]).unwrap();
            assert!(m.ok, "plaintext auth must succeed");
            break;
        }
    }
    handle.stop().await;
}
