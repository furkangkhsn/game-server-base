//! The client's transport statistics (the counters the load generator
//! prints and an operator reads after a lossy run).

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
    /// once, and [`UdpClient::is_established`](crate::udp::UdpClient::is_established) flips to `false`.
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
    /// Game-band datagrams (RAW and FRAG) received — the count the
    /// reports carry (module docs, "Game-band feedback").
    pub game_datagrams_received: u64,
    /// The server's game-band probes received.
    pub probes_received: u64,
    /// Reports answering a probe, and announcements (reports with no
    /// probe yet), the socket took.
    pub reports_sent: u64,
    pub announces_sent: u64,
    /// Reports and announcements the socket refused (lost).
    pub reports_send_failed: u64,
    /// Probe echoes (the server's RTT sample) longer than the reliable
    /// band's liveness bound, not taken as a sample.
    pub probe_echoes_refused: u64,
    /// Connection migration (module docs of `crate::udp`, module
    /// `path`): local sockets replaced by [`UdpClient::rebind`](crate::udp::UdpClient::rebind).
    pub rebinds: u64,
    /// The server's path challenges answered, and the answers the socket
    /// refused (lost: the server re-challenges on this client's next
    /// datagram).
    pub path_challenges_answered: u64,
    pub path_responses_send_failed: u64,
    /// Path challenges ignored: this session has no connection id (a
    /// server challenges only one that has).
    pub path_challenges_ignored: u64,
}
