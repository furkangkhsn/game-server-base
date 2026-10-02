//! The client side, shared by the e2e tests and the load generator.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use gsb_protocol::FrameBody;
use tokio::net::UdpSocket;

use crate::udp::*;

/// Client-side transport statistics (returned with the client).
#[derive(Debug, Default)]
pub struct UdpClientStats {
    /// The client's own reliable retransmissions (its control frames
    /// re-sent because their retransmit timer expired before their ACK
    /// arrived).
    pub retrans_out: u64,
    /// Duplicated inbound REL frames (the SERVER's retransmissions, as
    /// observed by this client).
    pub dup_in: u64,
    /// Inbound REL frames dropped on a full out-of-order window.
    pub oob_dropped: u64,
    /// Outbound REL frames still outstanding when the reliable band was
    /// declared dead (see the module docs, "The REL liveness bound"). A
    /// frame is never abandoned on its own age — the whole band dies at
    /// once, and [`UdpClient::is_established`] flips to `false`.
    pub gave_up: u64,
    /// Game-band messages rebuilt from FRAG datagrams (server → client
    /// fragmentation; see the module docs, "MTU (feature 3)").
    pub frag_reassembled: u64,
    /// Messages dropped with a fragment still missing: superseded by a
    /// newer message in their slot, aged out, or evicted by the memory
    /// bound. The loss signal of the fragmented band.
    pub frag_dropped_incomplete: u64,
    /// FRAG datagrams refused: a malformed header, a count past the
    /// ceiling, a count that disagrees with the message's first
    /// fragment, or a fragment of a message the slot has moved past.
    pub frag_rejected: u64,
    /// Challenge requests re-sent because no challenge came back within
    /// the handshake re-send interval (see the module docs, "Handshake
    /// loss").
    pub challenge_retries: u64,
    /// Proofs re-sent because the server had not yet shown it holds the
    /// session: the proof, or the server's accept, was lost.
    pub proof_retries: u64,
}

/// A rUDP client: the mirror image of the server's demux/writer, as one
/// task (the load generator's and the e2e tests' client is a plain task,
/// not an actor, so the whole session state is task-local).
///
/// The read loop's only awaited source is the socket read, bounded by
/// the smaller of the retransmit interval and the caller's window — the
/// same single-future idiom the server uses (no multiplexing).
pub struct UdpClient {
    sock: UdpSocket,
    peer: SocketAddr,
    established: bool,
    /// Inbound reliable state (server→client).
    in_expected: u32,
    in_oob: HashMap<u32, Vec<u8>>,
    /// Ordered inbound REL frames awaiting the caller (delivered in
    /// sequence order; RAW frames bypass it — the lossy band is
    /// unordered by design).
    pending: VecDeque<FrameBody>,
    /// Outbound reliable state (client→server): the next seq, and the
    /// band's sending half — the mirror of the server writer's (the
    /// un-ACKed frames, the liveness clock, the retransmit timer the
    /// handshake seeded).
    out_seq: u32,
    rel: RelSend,
    pub stats: UdpClientStats,
    buf: Vec<u8>,
    /// A RAW frame produced by `process_datagram`, awaiting hand-back to
    /// the awaiting loop (the lossy band is unordered: it does not wait
    /// for the pending REL frames).
    raw: Option<FrameBody>,
    /// The fragmented game band's reassembly state (bounded; see
    /// `frag`).
    reasm: Reassembly,
}

impl UdpClient {
    /// Connect: bind an ephemeral local port and run the stateless
    /// handshake (challenge → proof → the server's accept). Returns only
    /// once the SERVER has shown it holds the session; every step is
    /// re-sent on loss, and a handshake the server never completes ends
    /// in `TimedOut` after `HANDSHAKE_DEADLINE`, 5 s (see the module docs,
    /// "Handshake loss").
    pub async fn connect(addr: SocketAddr) -> std::io::Result<Self> {
        Self::connect_within(addr, HANDSHAKE_DEADLINE).await
    }

    /// [`Self::connect`] with an explicit give-up bound (the tests' seam:
    /// the give-up is exercised without waiting the full bound).
    pub(super) async fn connect_within(
        addr: SocketAddr,
        within: Duration,
    ) -> std::io::Result<Self> {
        let sock = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))
            .await
            .map_err(|e| std::io::Error::new(e.kind(), e.to_string()))?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut client = Self {
            sock,
            peer: addr,
            // Not until the server says so: `handshake` flips it.
            established: false,
            in_expected: 1,
            in_oob: HashMap::new(),
            pending: VecDeque::new(),
            out_seq: 0,
            // Replaced by the handshake, with its RTT estimate.
            rel: RelSend::new(Instant::now(), Rto::default()),
            stats: UdpClientStats::default(),
            buf: vec![0u8; 2048],
            raw: None,
            reasm: Reassembly::default(),
        };
        client.handshake(nonce, within).await?;
        Ok(client)
    }

    /// One-shot liveness probe (the UDP readiness check a supervisor or
    /// orchestrator can use in place of a TCP connect probe — UDP has no
    /// SYN to probe with): send a challenge request on `sock` and wait up
    /// to `wait` for the challenge. A true answer means the demux is
    /// alive and answering; nothing is established (no session is
    /// created by a bare challenge request).
    pub async fn challenge_probe(sock: &UdpSocket, addr: SocketAddr, wait: Duration) -> bool {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        if sock.send_to(&encode_hello(nonce, 0), addr).await.is_err() {
            return false;
        }
        let mut buf = [0u8; 32];
        tokio::time::timeout(wait, sock.recv_from(&mut buf))
            .await
            .is_ok_and(|r| {
                matches!(
                    r,
                    Ok((n, from)) if from == addr && n >= 18 && buf[0] == KIND_HELLO
                )
            })
    }

    /// The server's address.
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// The local bound address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.sock.local_addr().ok()
    }

    /// Whether the session is live: the handshake completed and the
    /// reliable band has not been declared dead. This is the client's
    /// liveness API — UDP has no EOF, so the flip from `true` to `false`
    /// is the only "the session is over" signal a caller gets (the
    /// server's equivalent is the connection actor's teardown).
    pub fn is_established(&self) -> bool {
        self.established
    }

    /// The smoothed round-trip time of the reliable band, once a sample
    /// was taken (the handshake's, or a control frame's ACK — module
    /// docs, "Retransmit timer").
    pub fn srtt(&self) -> Option<Duration> {
        self.rel.rto().srtt()
    }

    /// The reliable band's current retransmit timeout (backoff included).
    pub fn rto(&self) -> Duration {
        self.rel.rto().current()
    }

    /// Send one application frame: control band (reliable, sequenced) or
    /// game band (RAW, loss-tolerant) — the same split as the server.
    pub async fn send_frame(&mut self, op: u16, payload: impl Into<Bytes>) -> std::io::Result<()> {
        let frame = FrameBody::new(op, payload.into());
        if is_control(op) {
            if self.rel.len() >= RETRANSIT_CAP {
                // The memory bound of the module docs, mirrored: this many
                // control frames outstanding with nothing confirmed is the
                // same death as the no-ACK clock arriving early.
                self.declare_rel_dead();
                return Err(std::io::Error::other(
                    "rUDP reliable control band is dead (retransmit queue full, no ACK progress)",
                ));
            }
            // `push` starts the liveness clock when the queue was empty:
            // it measures unanswered WORK. The retransmit pass also
            // refreshes it on an empty queue, but a client busy on the
            // game band never takes that pass (every read returns a
            // datagram) — without this, its first control frame after a
            // quiet spell would inherit a clock stamped at the last ACK
            // and the band would "die" on the next pass.
            self.out_seq = self.out_seq.wrapping_add(1);
            let dg = Bytes::from(encode_rel(self.out_seq, &frame));
            self.rel.push(self.out_seq, dg.clone(), Instant::now());
            self.sock.send_to(&dg, self.peer).await.map(|_| ())
        } else {
            let dg = encode_raw(&frame);
            self.sock.send_to(&dg, self.peer).await.map(|_| ())
        }
    }

    /// Receive one application frame, waiting up to `wait` for it. While
    /// waiting the loop performs the outbound retransmit pass (on the RTO
    /// ticks and after every datagram) — reliable delivery without any
    /// multiplexing.
    ///
    /// Returns `Ok(None)` when the window elapses with no frame (NOT an
    /// error: UDP has no EOF — the caller probes liveness explicitly when
    /// it needs the "session gone" distinction).
    pub async fn recv_frame(&mut self, wait: Duration) -> std::io::Result<Option<FrameBody>> {
        // A REL frame that is already ordered is returned immediately
        // (no wait, no syscall).
        if let Some(f) = self.pending.pop_front() {
            return Ok(Some(f));
        }
        // A game-band frame the handshake received in place of the
        // server's accept (see `handshake`): it is not lost either.
        if let Some(raw) = self.raw.take() {
            return Ok(Some(raw));
        }
        let deadline = Instant::now() + wait;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(None);
            };
            // Wake for the oldest control frame's timer (at most a tick).
            let w = remaining.min(self.rel.wait(Instant::now(), RETRANSIT_TICK));
            match tokio::time::timeout(w, self.sock.recv_from(&mut self.buf)).await {
                Ok(Ok((n, from))) if from == self.peer => {
                    // Copy out first: process_datagram takes &mut self and
                    // must not borrow self.buf through the same call.
                    let data = self.buf[..n].to_vec();
                    self.process_datagram(&data);
                    // The retransmit pass runs on every datagram too, not
                    // only on a read timeout: a busy game band (a snapshot
                    // stream, fragmented or not) may never leave the read
                    // idle for a whole RTO, and a lost control frame must
                    // not wait for silence. The pass is O(1) — it looks at
                    // the oldest outstanding frame only.
                    self.retransmit_pass();
                    if let Some(raw) = self.raw.take() {
                        // A RAW frame (the lossy band, unordered): return
                        // it immediately, without waiting for ordered REL.
                        return Ok(Some(raw));
                    }
                    if let Some(f) = self.pending.pop_front() {
                        return Ok(Some(f));
                    }
                }
                Ok(Ok(_)) => {} // stray peer: ignore
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    // RTO tick (or the window's edge): retransmit pass.
                    self.retransmit_pass();
                    if let Some(f) = self.pending.pop_front() {
                        return Ok(Some(f));
                    }
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                }
            }
        }
    }
}

mod handshake;
mod io;

#[cfg(test)]
pub(in crate::udp) use handshake::step_interval;

#[cfg(test)]
mod tests;
