//! Demo client: connects, authenticates, joins room 1, wanders around a
//! circle, and prints the frames it receives.
//!
//! Run the server first: `cargo run -p gsb-server` (default port 7777),
//! then `cargo run -p gsb-server --example client [addr]`.
//!
//! TLS (docs/SECURITY.md §2): pass `--tls-ca <PEM>` to verify the server
//! against a custom root (a self-signed test CA works), and
//! `--tls-server-name <NAME>` for the name the certificate must carry
//! (default `localhost`). Without `--tls-ca` the client is plaintext,
//! exactly as before this turn.
//!
//! The client stays in the spirit of the architecture: no multiplexing —
//! the mover task owns the write half and the reader loop owns the read
//! half, each with exactly one thing to wait on.

use std::time::Duration;

use gsb_protocol::base::{
    Auth, AuthResult, Error, ErrorCode, HeartbeatAck, JoinRoom, JoinRoomResult, LeaveRoomResult,
};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

fn frame(op: u16, payload: &[u8]) -> Vec<u8> {
    let body = 2 + payload.len();
    let mut out = Vec::with_capacity(4 + body);
    out.extend_from_slice(&(body as u32).to_le_bytes());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Length-prefixed frame read over ANY byte source (TCP half or rustls
/// half — both are plain `AsyncRead` to this function).
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Flag scan: one positional addr + the two TLS flags (both default off
    // = plaintext).
    let mut addr = "127.0.0.1:7777".to_string();
    let mut tls_ca: Option<String> = None;
    let mut tls_server_name = "localhost".to_string();
    let mut argv = std::env::args().skip(1);
    while let Some(a) = argv.next() {
        match a.as_str() {
            "--tls-ca" => tls_ca = Some(argv.next().expect("--tls-ca needs a PEM path")),
            "--tls-server-name" => {
                tls_server_name = argv.next().expect("--tls-server-name needs a value")
            }
            other => addr = other.to_string(),
        }
    }

    // The connection's two halves behind type-erased trait objects: the
    // framing and everything below cannot tell TCP from TLS (the same seam
    // the server's Endpoint provides on its side).
    let (r, w): (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
    ) = if let Some(ca_path) = &tls_ca {
        let ca_certs = load_certs(ca_path).unwrap_or_else(|e| panic!("{e}"));
        let mut roots = rustls::RootCertStore::empty();
        for cert in ca_certs {
            roots.add(cert).expect("--tls-ca PEM is not a certificate");
        }
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("TLS protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
        let tcp = TcpStream::connect(&addr)
            .await
            .unwrap_or_else(|e| panic!("cannot connect to {addr}: {e} (is the server running?)"));
        tcp.set_nodelay(true).ok();
        let dns_name: rustls::pki_types::ServerName<'static> = tls_server_name
            .clone()
            .try_into()
            .unwrap_or_else(|_| panic!("--tls-server-name `{tls_server_name}` is not a DNS name"));
        let tls = connector
            .connect(dns_name, tcp)
            .await
            .unwrap_or_else(|e| panic!("TLS handshake with {addr} failed: {e}"));
        println!("connected to {addr} over TLS (ca={ca_path}, server-name={tls_server_name})");
        let (tr, tw) = tokio::io::split(tls);
        (Box::new(tr), Box::new(tw))
    } else {
        let stream = TcpStream::connect(&addr).await.unwrap_or_else(|e| {
            panic!("cannot connect to {addr}: {e} (is the server running?)");
        });
        stream.set_nodelay(true).ok();
        println!("connected to {addr}");
        let (tr, tw) = stream.into_split();
        (
            Box::new(tr) as Box<dyn AsyncRead + Unpin + Send>,
            Box::new(tw),
        )
    };
    let mut r = r;
    let mut w = w;

    // AUTH + JOIN in one write: the connection actor drains its mailbox in
    // order, so the join is processed after the auth.
    let auth = Auth {
        name: "client-1".into(),
        ticket: vec![],
        // A client states the wire version it was built against; the
        // server refuses a mismatch with ERROR_CODE_PROTOCOL_VERSION
        // (DESIGN §5.5). Sending 0 would be accepted too, as a legacy
        // pre-versioning client — a reference client does not.
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    };
    let join = JoinRoom { room_id: 1 };
    w.write_all(&frame(
        gsb_protocol::op::base::AUTH_REQ,
        &auth.encode_to_vec(),
    ))
    .await
    .unwrap();
    w.write_all(&frame(
        gsb_protocol::op::base::JOIN_ROOM_REQ,
        &join.encode_to_vec(),
    ))
    .await
    .unwrap();
    w.flush().await.unwrap();

    // The client's world view (the protocol's client half, `game.proto`):
    // a full replaces it, a delta applies on top of it (even across a
    // sequence gap — the stream is event-driven; the keep-alive full is
    // the convergence guarantee), a delta with no baseline is dropped
    // until the next full, a duplicate (seq <= last accepted) is
    // discarded.
    let mut view: std::collections::HashMap<u64, (i32, i32)> = std::collections::HashMap::new();
    let mut last_seq: Option<u64> = None;
    let mut acked_max: u64 = 0;

    // Mover task: every 150 ms, a MOVE_TO around a circle of radius 40,
    // numbered from 1 (the per-session input sequence). It owns the write
    // half; nothing else touches it.
    let mover = tokio::spawn(async move {
        let mut i: u32 = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(150)).await;
            i += 1;
            let angle = (i as f32) * 0.7;
            let msg = gsb_game::game::MoveTo {
                x: (angle.cos() * 40.0) as i32,
                y: (angle.sin() * 40.0) as i32,
                seq: i as u64,
            };
            if w.write_all(&frame(gsb_game::op::MOVE_TO, &msg.encode_to_vec()))
                .await
                .is_err()
                || w.flush().await.is_err()
            {
                break;
            }
        }
    });

    // Reader loop: print frames for 5 seconds.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            break;
        }
        // One wait, no multiplexing: a bounded read attempt.
        let result = tokio::time::timeout(Duration::from_millis(200), read_frame(r.as_mut())).await;
        let Some((op, payload)) = result.ok().flatten() else {
            continue;
        };
        match op {
            gsb_protocol::op::base::AUTH_RESULT => {
                let m: AuthResult = AuthResult::decode(&payload[..]).unwrap();
                println!("AUTH_RESULT ok={} reason={:?}", m.ok, m.reason);
            }
            gsb_protocol::op::base::JOIN_ROOM_RESULT => {
                let m: JoinRoomResult = JoinRoomResult::decode(&payload[..]).unwrap();
                println!("JOIN_ROOM_RESULT entity={}", m.entity);
            }
            gsb_protocol::op::base::LEAVE_ROOM_RESULT => {
                let _m: LeaveRoomResult = LeaveRoomResult::decode(&payload[..]).unwrap();
                println!("LEAVE_ROOM_RESULT");
            }
            gsb_protocol::op::base::HEARTBEAT_ACK => {
                let m: HeartbeatAck = HeartbeatAck::decode(&payload[..]).unwrap();
                println!("HEARTBEAT_ACK tick={}", m.tick);
            }
            gsb_protocol::op::base::ERROR => {
                let m: Error = Error::decode(&payload[..]).unwrap();
                // Both halves, as base.proto's forward-compatibility rule
                // asks of a client: the RAW number (preserved by the open
                // proto3 enum, so a future code is still reportable) and
                // the class this build knows it as. A code this client
                // does not know reads back as UNSPECIFIED and must be
                // handled like ERROR_CODE_OTHER.
                let class = match m.code() {
                    ErrorCode::Unspecified => "unknown to this client; treat as OTHER",
                    known => known.as_str_name(),
                };
                println!("ERROR code={} ({}) message={}", m.code, class, m.message);
            }
            gsb_game::op::WORLD_SNAPSHOT => {
                let m: gsb_game::game::WorldSnapshot =
                    gsb_game::game::WorldSnapshot::decode(&payload[..]).unwrap();
                // The client rules (`game.proto`):
                if m.sequence <= last_seq.unwrap_or(0) {
                    println!("WORLD_SNAPSHOT seq={} duplicate — discarded", m.sequence);
                } else if m.delta && last_seq.is_none() {
                    println!(
                        "WORLD_SNAPSHOT seq={} delta without baseline — dropped (next full heals)",
                        m.sequence
                    );
                } else {
                    if m.delta {
                        for &w in &m.removed {
                            view.remove(&w);
                        }
                        for c in &m.cell_exits {
                            // Forget every held entity in the exited cell
                            // (the server's formula: floor of the WIRE
                            // coordinates / cell_size — cell_size 20 here).
                            let (cx, cy) = (c.x, c.y);
                            view.retain(|_, (x, y)| {
                                // (the floor, not the truncated quotient —
                                // the server's formula)
                                (*x as f32 / 20.0).floor() as i32 != cx
                                    || (*y as f32 / 20.0).floor() as i32 != cy
                            });
                        }
                        for e in &m.entities {
                            view.insert(e.entity, (e.x, e.y));
                        }
                    } else {
                        view.clear();
                        for e in &m.entities {
                            view.insert(e.entity, (e.x, e.y));
                        }
                    }
                    last_seq = Some(m.sequence);
                    let kind = if m.delta { "delta" } else { "FULL" };
                    println!(
                        "WORLD_SNAPSHOT seq={} {kind}: now {} entities held ({})",
                        m.sequence,
                        view.len(),
                        view.iter()
                            .map(|(id, (x, y))| format!("{id}=({x}, {y})"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            gsb_game::op::PRIVATE => {
                let m: gsb_game::game::Private =
                    gsb_game::game::Private::decode(&payload[..]).unwrap();
                match m.payload {
                    Some(gsb_game::game::private::Payload::Ack(a)) => {
                        acked_max = acked_max.max(a.processed_up_to);
                        println!(
                            "PRIVATE ack: processed_up_to={} (max so far {})",
                            a.processed_up_to, acked_max
                        );
                    }
                    Some(gsb_game::game::private::Payload::Snapshot(s)) => {
                        // The one-shot private full: applied UNCONDITIONALLY
                        // (it is a different stream from the group frames and
                        // resets this connection's baseline).
                        view.clear();
                        for e in &s.entities {
                            view.insert(e.entity, (e.x, e.y));
                        }
                        last_seq = Some(s.sequence);
                        println!(
                            "PRIVATE full: {} entities (baseline reset)",
                            s.entities.len()
                        );
                    }
                    None => {}
                }
            }
            other => println!("frame op={other} ({} bytes)", payload.len()),
        }
    }

    println!("client done");
    drop(r);
    mover.abort();
}

/// Load PEM certificate(s) from `path`. Multi-cert bundles work: every
/// PEM CERTIFICATE block in the file becomes a trust anchor (rustls'
/// RootCertStore takes them one by one).
fn load_certs(path: &str) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, String> {
    let pem = std::fs::read_to_string(path).map_err(|e| format!("cannot read `{path}`: {e}"))?;
    rustls_pemfile::certs(&mut pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("malformed certificate PEM in `{path}`: {e}"))
}
