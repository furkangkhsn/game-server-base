//! The client side, shared by the e2e tests and the load generator.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use gsb_protocol::FrameBody;
use tokio::net::UdpSocket;

use crate::udp::*;

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
    /// The game band's feedback: the count and the announcements.
    reports: report::Reports,
    /// Connection migration: the CID asked for and granted (module
    /// `migrate`).
    path: migrate::Migration,
    /// The record layer (module `seal`, B5a): the pinned server key and,
    /// once the handshake finished, the two record halves.
    seal: seal::Seal,
}

impl UdpClient {
    /// Connect: bind an ephemeral local port and run the stateless
    /// handshake (challenge → proof → the server's accept). Returns only
    /// once the SERVER has shown it holds the session; every step is
    /// re-sent on loss, and a handshake the server never completes ends
    /// in `TimedOut` after `HANDSHAKE_DEADLINE`, 5 s (see the module docs,
    /// "Handshake loss").
    ///
    /// This is a PLAINTEXT client (no pinned server key): it reaches only
    /// a door configured `udp_security = "plaintext"`. A sealed door — the
    /// server's default since B5a — refuses it; connect to one with
    /// [`Self::connect_with`] and [`UdpClientConfig::server_key`].
    pub async fn connect(addr: SocketAddr) -> std::io::Result<Self> {
        Self::connect_with(addr, UdpClientConfig::default()).await
    }

    /// [`Self::connect`] with a [`UdpClientConfig`] (game-band reports
    /// off: the client as it was before they existed).
    pub async fn connect_with(addr: SocketAddr, config: UdpClientConfig) -> std::io::Result<Self> {
        Self::connect_within_with(addr, HANDSHAKE_DEADLINE, config).await
    }

    /// [`Self::connect`] with an explicit give-up bound (the tests' seam:
    /// the give-up is exercised without waiting the full bound).
    #[cfg(test)]
    pub(super) async fn connect_within(
        addr: SocketAddr,
        within: Duration,
    ) -> std::io::Result<Self> {
        Self::connect_within_with(addr, within, UdpClientConfig::default()).await
    }

    pub(in crate::udp) async fn connect_within_with(
        addr: SocketAddr,
        within: Duration,
        config: UdpClientConfig,
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
            reports: report::Reports::new(config),
            path: migrate::Migration::new(config.migration),
            seal: seal::Seal::new(config.server_key),
        };
        client.handshake(nonce, within).await?;
        Ok(client)
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

    /// The reliable band's estimator (the tests read its RTTVAR).
    #[cfg(test)]
    pub(in crate::udp) fn band_rto(&self) -> &Rto {
        self.rel.rto()
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
            // The band keeps the INNER datagram: every (re)send goes
            // through `wire` — on a sealed session a fresh record each time.
            self.out_seq = self.out_seq.wrapping_add(1);
            let inner = encode_rel(self.out_seq, &frame);
            self.rel
                .push(self.out_seq, Bytes::from(inner.clone()), Instant::now());
            let dg = self.wire(inner).ok_or_else(seal::exhausted)?;
            self.sock.send_to(&dg, self.peer).await.map(|_| ())
        } else {
            let dg = self.wire(encode_raw(&frame)).ok_or_else(seal::exhausted)?;
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
mod migrate;
mod report;
mod seal;
mod stats;
pub use report::UdpClientConfig;
pub use stats::UdpClientStats;

#[cfg(test)]
pub(in crate::udp) use handshake::step_interval;

#[cfg(test)]
mod tests;
