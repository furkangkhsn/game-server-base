//! One simulated client: connect, auth, join, then move on a timer
//! while applying every snapshot into a local view.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use gsb_client::{Conn, Recv};

mod accounting;
mod connect;
mod errors;
mod rpc;
mod stall;
mod wait;
pub(crate) use accounting::{Dir, frame_bytes, wire_in_bytes, ws_message_bytes};
pub(crate) use connect::{TlsOpts, connect_wire};
pub(crate) use errors::{ClientErrors, ERROR_REASONS};
pub(crate) use rpc::{RpcClient, RpcPlan, RpcTally};
pub(crate) use stall::{STALL_RCVBUF, Stall};
pub(crate) use wait::{PROTOCOL_WAIT, Wait};

mod view;
pub(crate) use view::*;

#[cfg(test)]
mod tests;

/// One client's end-to-end record (task-local; returned via JoinHandle).
#[derive(Default)]
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
    /// What the client counted as an error, by reason (B88); the
    /// `errors=` key is their sum.
    pub(crate) errors: ClientErrors,
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
    /// The RPC traffic mode's numbers (`--rpc-rate`; all zero without
    /// it — and on a churn or orchestrated run, which refuse the mode).
    pub(crate) rpc: RpcTally,
}

impl ClientReport {
    /// Client `id`'s record before anything happened.
    pub(crate) fn new(id: u64) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }
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
    /// The client's transport (TCP, rUDP or WebSocket; see
    /// `connect_wire`).
    pub(crate) kind: crate::Transport,
    /// `--capture`: this client's capture file and the game's name
    /// (`None` = not captured — every client of a run without the flag).
    pub(crate) capture: Option<(std::path::PathBuf, &'static str)>,
    /// `--stall-ms`: the slow-reader cycle (`None` = reads as it can).
    pub(crate) stall: Option<Stall>,
    /// `--rpc-rate`: the RPC traffic beside the inputs (`None` = none).
    pub(crate) rpc: Option<RpcPlan>,
}

/// What one bounded receive on the wire found. TCP distinguishes death
/// (EOF, or a frame the reader refuses) from quiet; rUDP has no EOF, so
/// "quiet" is all it can report — the deadline ends those runs.
pub(crate) enum Got {
    Frame(u16, Bytes),
    Quiet,
    Dead,
}

pub(crate) async fn recv_wire(wire: &mut Conn, timeout: Duration) -> Got {
    match wire.recv(timeout).await {
        Ok(Recv::Frame(f)) => Got::Frame(f.op, f.payload),
        Ok(Recv::Quiet) => Got::Quiet,
        Ok(Recv::Closed) => Got::Dead,
        // rUDP: a socket error reads as a quiet window (no EOF exists).
        Err(_) if wire.is_udp() => Got::Quiet,
        Err(_) => Got::Dead,
    }
}

pub(crate) async fn send_wire(wire: &mut Conn, op: u16, payload: Vec<u8>) -> std::io::Result<()> {
    wire.send(op, &payload).await
}
