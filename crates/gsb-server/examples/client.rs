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

use gsb_kit::client::{Apply, ClientDecoder, ClientError, ClientView, PrivateEvent, Snapshot};
use gsb_protocol::base::{
    Auth, AuthResult, Error, ErrorCode, HeartbeatAck, JoinRoom, JoinRoomResult, LeaveRoomResult,
};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// The demo's decode seam for the kit's reference client: a record is
/// kept as its wire position, in the server's cell of it (floor of the
/// WIRE coordinates / cell_size); a `CellExit` names the cell's index.
struct DemoDecoder {
    cell_size: f32,
}

impl ClientDecoder for DemoDecoder {
    type Record = (i32, i32);
    type Cell = (i32, i32);

    fn record(&self, body: &[u8]) -> Result<(u64, (i32, i32)), ClientError> {
        let e = gsb_demo::game::EntityRecord::decode(body)?;
        Ok((e.entity, (e.x, e.y)))
    }

    fn cell_of(&self, &(x, y): &(i32, i32)) -> (i32, i32) {
        (
            (x as f32 / self.cell_size).floor() as i32,
            (y as f32 / self.cell_size).floor() as i32,
        )
    }

    fn cell_exit(&self, body: &[u8]) -> Result<(i32, i32), ClientError> {
        let c = gsb_demo::game::CellExit::decode(body)?;
        Ok((c.x, c.y))
    }
}

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

    // The client's world view: the kit's reference client
    // (`gsb_kit::client`, the client rules of `kit.proto`) — a full
    // replaces it, a delta applies on top of it (even across a sequence
    // gap — the stream is event-driven; the keep-alive full is the
    // convergence guarantee), a delta with no baseline is dropped until
    // the next full, a duplicate (seq <= last accepted) is discarded, the
    // one-shot private full is applied unconditionally.
    let mut view = ClientView::new(DemoDecoder { cell_size: 20.0 });
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
            let msg = gsb_demo::game::MoveTo {
                x: (angle.cos() * 40.0) as i32,
                y: (angle.sin() * 40.0) as i32,
                seq: i as u64,
            };
            if w.write_all(&frame(gsb_demo::op::MOVE_TO, &msg.encode_to_vec()))
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
            gsb_demo::op::WORLD_SNAPSHOT => match view.apply_snapshot(&payload) {
                Ok(Snapshot { sequence, apply }) => match apply {
                    Apply::Stale => println!("WORLD_SNAPSHOT seq={sequence} duplicate — discarded"),
                    Apply::NoBaseline => println!(
                        "WORLD_SNAPSHOT seq={sequence} delta without baseline — dropped (next full heals)"
                    ),
                    Apply::Full | Apply::Delta => {
                        let kind = if apply == Apply::Delta {
                            "delta"
                        } else {
                            "FULL"
                        };
                        println!(
                            "WORLD_SNAPSHOT seq={sequence} {kind}: now {} entities held ({})",
                            view.len(),
                            view.iter()
                                .map(|(id, (x, y))| format!("{id}=({x}, {y})"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                },
                Err(e) => println!("WORLD_SNAPSHOT rejected: {e}"),
            },
            gsb_demo::op::PRIVATE => match view.apply_private(&payload) {
                Ok(PrivateEvent::Ack(up_to)) => {
                    acked_max = acked_max.max(up_to);
                    println!("PRIVATE ack: processed_up_to={up_to} (max so far {acked_max})");
                }
                // The one-shot private full: applied UNCONDITIONALLY (it
                // is a different stream from the group frames and resets
                // this connection's baseline).
                Ok(PrivateEvent::Full { .. }) => {
                    println!("PRIVATE full: {} entities (baseline reset)", view.len());
                }
                Ok(PrivateEvent::Empty) => {}
                Err(e) => println!("PRIVATE rejected: {e}"),
            },
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
