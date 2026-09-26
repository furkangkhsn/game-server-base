//! One simulated client: connect, auth, join, then move on a timer
//! while applying every snapshot into a local view.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use gsb_protocol::op;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::wire::*;
use gsb_net::udp::UdpClient;

mod stall;
use stall::tcp_connect;
pub(crate) use stall::{STALL_RCVBUF, Stall};

mod view;
pub(crate) use view::*;

/// One client's end-to-end record (task-local; returned via JoinHandle).
pub(crate) struct ClientReport {
    pub(crate) id: u64,
    pub(crate) connected: bool,
    pub(crate) connect_ms: u128,
    pub(crate) joined: bool,
    pub(crate) entity: u64,
    pub(crate) left: bool,
    pub(crate) snapshots: u64,
    pub(crate) bytes_in: u64,
    pub(crate) bytes_out: u64,
    pub(crate) moves: u64,
    pub(crate) errors: u64,
    /// Join rejections observed (`ERROR` code 8, room full): the room
    /// capacity guardrail working — the connection stays alive.
    pub(crate) join_rejected: u64,
    /// Connection-capacity rejections observed (`ERROR` code 9): the
    /// server-wide cap rejected this connection at birth.
    pub(crate) cap_rejected: u64,
    /// Protocol-violation-budget closes observed (`ERROR` code 9 whose
    /// message names the violation budget): the anti-amplification
    /// guardrail closing a connection that exceeded its budget. Same
    /// *code* as the capacity close (both are "server closed the
    /// connection"); the message string is what separates them.
    pub(crate) budget_rejected: u64,
    /// rUDP transport statistics (all zero on TCP): the client's own
    /// reliable retransmissions, duplicated inbound REL frames (the
    /// server's retransmissions), inbound drops on a full out-of-order
    /// window, and outbound control frames given up (no ACK in time).
    pub(crate) retrans_out: u64,
    pub(crate) dup_in: u64,
    pub(crate) oob_dropped: u64,
    pub(crate) gave_up: u64,
    /// rUDP fragmentation (all zero on TCP): game-band messages rebuilt
    /// from FRAG datagrams, and those dropped with a fragment missing.
    pub(crate) frag_reassembled: u64,
    pub(crate) frag_dropped: u64,
    /// rUDP handshake re-sends (all zero on TCP): challenge requests and
    /// proofs sent again because the server had not answered yet — the
    /// handshake's own loss signal.
    pub(crate) hs_retries: u64,
    /// First/last snapshot sequence with its arrival instant: the server's
    /// measured tick rate is (last_seq − first_seq) / Δt, since the
    /// snapshot sequence is the global tick index.
    pub(crate) seq_first: Option<(u64, Instant)>,
    pub(crate) seq_last: Option<(u64, Instant)>,
    /// Input acknowledgments received (Section A; the server's per-
    /// connection high-water marks).
    pub(crate) acks: u64,
    /// The highest `processed_up_to` observed over all acks.
    pub(crate) ack_processed_max: u64,
    /// The worst ack lag in ms (send instant of the acked seq → ack
    /// arrival; 0 when no numbered input was acked).
    pub(crate) ack_lag_max_ms: u128,
    /// Full snapshots applied to the client view (group frames with
    /// `delta = false`, whatever their source: a fresh group's first
    /// packet, a keep-alive full, or a one-shot private full).
    pub(crate) fulls: u64,
    /// One-shot private fulls received (`Private{snapshot}` — the late-
    /// join / group-crossing baseline; a trigger-frequency measurement).
    pub(crate) private_fulls: u64,
    /// Delta snapshots applied (`delta = true`).
    pub(crate) deltas: u64,
    /// Deltas dropped (no baseline, or a sequence gap — a lost snapshot
    /// before them; the loss-recovery counter, healed by the next full).
    pub(crate) gap_drops: u64,
    /// Entities in the client view at the end of the run.
    pub(crate) view_size: u64,
    /// Churn mode only (RECONNECT §14.5): completed connect→drop cycles.
    pub(crate) churn_cycles: u64,
    /// Churn mode only: joins that came back onto the SAME wire id (a
    /// server-accepted resume — the counter the profile exists to move).
    pub(crate) resumed: u64,
    /// Churn mode only: joins that got a DIFFERENT wire id than the
    /// previous session (the park was already gone — expiry/supersede —
    /// and the client transparently fresh-joined, §5).
    pub(crate) fresh_joins: u64,
}

/// The client-side TLS material (a cloned slice of `Args`): the CA root to
/// trust and the name to expect in the server certificate. `None` =
/// plaintext TCP.
#[derive(Clone)]
pub(crate) struct TlsOpts {
    pub(crate) ca_path: String,
    pub(crate) server_name: String,
}

/// Everything one client task needs besides its own id. (One struct
/// rather than eight scalars — the profile work kept adding fields.)
#[derive(Clone)]
pub(crate) struct ClientParams {
    /// TLS material for TCP clients (`None` = plaintext, the default).
    pub(crate) tls: Option<TlsOpts>,
    pub(crate) addr: SocketAddr,
    pub(crate) room: u64,
    pub(crate) move_ms: Duration,
    pub(crate) stagger_ms: f64,
    /// The game's bot: what this client sends and how it reads the
    /// game's frames (`bot/`).
    pub(crate) bot: std::sync::Arc<dyn crate::bot::LoadBot>,
    pub(crate) deadline: Instant,
    /// Flood mode (the `--flood-id` client): after joining, write the
    /// bot's flood input in a tight loop until the deadline — the input-flood behaviour
    /// probe for the per-connection pull budget and the drop attribution.
    pub(crate) flood: bool,
    /// The client's transport (TCP or rUDP; see the `Wire` below).
    pub(crate) kind: gsb_server::TransportKind,
    /// `--capture`: this client's capture file and the game's name
    /// (`None` = not captured — every client of a run without the flag).
    pub(crate) capture: Option<(std::path::PathBuf, &'static str)>,
    /// `--stall-ms`: the slow-reader cycle (`None` = reads as it can).
    pub(crate) stall: Option<Stall>,
}

/// Build a rustls connector trusting ONLY the CA PEM at `ca_path` (the
/// `--tls-ca` root; a self-signed test CA works — docs/SECURITY.md §2).
pub(crate) fn tls_connector(ca_path: &str) -> tokio_rustls::TlsConnector {
    let pem = std::fs::read_to_string(ca_path)
        .unwrap_or_else(|e| panic!("cannot read --tls-ca `{ca_path}`: {e}"));
    let certs = rustls_pemfile::certs(&mut pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|e| panic!("malformed certificate PEM in `{ca_path}`: {e}"));
    let mut roots = rustls::RootCertStore::empty();
    for c in certs {
        roots.add(c).expect("--tls-ca PEM is not a certificate");
    }
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
}

/// Connect one wire of the given kind (the transport-specific half of a
/// client session's birth; shared by the plain and the churn client —
/// TCP gets `nodelay` + a split into boxed halves so plaintext and TLS
/// share one wire shape, rUDP runs its cookie handshake). With TLS
/// material, `connect_ms` includes the rustls handshake.
pub(crate) async fn connect_wire(
    kind: gsb_server::TransportKind,
    addr: SocketAddr,
    tls: &Option<TlsOpts>,
    rcvbuf: Option<u32>,
) -> std::io::Result<Wire> {
    Ok(match kind {
        gsb_server::TransportKind::Udp => Wire::Udp(Box::new(UdpClient::connect(addr).await?)),
        gsb_server::TransportKind::Tcp => match tls {
            None => {
                let stream = tcp_connect(addr, rcvbuf).await?;
                stream.set_nodelay(true).ok();
                let (r, w) = tokio::io::split(stream);
                Wire::Tcp {
                    r: Box::new(r),
                    w: Box::new(w),
                }
            }
            Some(opts) => {
                let stream = tcp_connect(addr, rcvbuf).await?;
                stream.set_nodelay(true).ok();
                let connector = tls_connector(&opts.ca_path);
                let dns: rustls::pki_types::ServerName<'static> =
                    opts.server_name.clone().try_into().map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!("--tls-server-name `{}` is not a DNS name", opts.server_name),
                        )
                    })?;
                // The handshake happens HERE: connect_ms covers it (the
                // same convention as the rUDP cookie handshake above).
                let tls_stream = connector.connect(dns, stream).await?;
                let (r, w) = tokio::io::split(tls_stream);
                Wire::Tcp {
                    r: Box::new(r),
                    w: Box::new(w),
                }
            }
        },
    })
}

/// What one bounded receive on the wire found. TCP distinguishes death
/// (EOF) from quiet; rUDP has no EOF, so "quiet" is all it can report —
/// the deadline ends those runs.
pub(crate) enum Got {
    Frame(u16, Vec<u8>),
    Quiet,
    Dead,
}

pub(crate) async fn recv_wire(wire: &mut Wire, timeout: Duration) -> Got {
    match wire {
        Wire::Tcp { r, .. } => {
            match tokio::time::timeout(timeout, read_frame(r.as_mut())).await {
                Ok(Some((op, payload))) => Got::Frame(op, payload),
                Ok(None) => Got::Dead, // EOF / bad frame
                Err(_) => Got::Quiet,
            }
        }
        Wire::Udp(c) => match c.recv_frame(timeout).await {
            Ok(Some(f)) => Got::Frame(f.op, f.payload.to_vec()),
            _ => Got::Quiet,
        },
    }
}

pub(crate) async fn send_wire(wire: &mut Wire, op: u16, payload: Vec<u8>) -> std::io::Result<()> {
    match wire {
        Wire::Tcp { w, .. } => {
            let f = frame(op, &payload);
            w.write_all(&f).await?;
            w.flush().await
        }
        Wire::Udp(c) => c.send_frame(op, payload).await,
    }
}

/// The wire to the server (the only place TCP and rUDP diverge inside
/// the client loop — see `run_client`).
pub(crate) enum Wire {
    /// Length-prefixed frames over a per-connection socket, split: the
    /// main loop owns the read half, the flood path the write half.
    /// Type-erased halves so plaintext TCP and TLS-over-TCP share this
    /// one variant (the framing below cannot tell them apart).
    Tcp {
        r: Box<dyn AsyncRead + Unpin + Send>,
        w: Box<dyn AsyncWrite + Unpin + Send>,
    },
    /// One shared socket in one task: read and write interleave (UDP has
    /// no connection to split). Boxed: `UdpClient` carries a 2 KB read
    /// buffer + queues (keeps the enum small — clippy's
    /// `large_enum_variant`).
    Udp(Box<UdpClient>),
}

/// The rUDP datagram size of one frame (client-side byte accounting
/// mirrors the bytes actually sent: RAW = kind + op + payload; REL =
/// kind + seq + op + payload).
pub(crate) fn wire_in_bytes(op: u16, payload_len: usize) -> u64 {
    let header = if (1..=64).contains(&op) && op != op::base::UDP_ACK {
        5
    } else {
        1
    };
    (header + 2 + payload_len) as u64
}
