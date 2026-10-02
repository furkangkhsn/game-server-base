//! Connection migration, writer side (module `crate::udp::path`): the
//! demux validated a new client address and says so through the
//! outbound channel (`UDP_PATH`, like a piggybacked ACK — one awaited
//! source, no lock). Everything after the notice goes to the new
//! address; everything before it went to the old one (decision 10: the
//! old path until the new one is validated). A CHILD of [`super`], so the
//! writer's state stays private.

use std::time::Instant;

use gsb_protocol::FrameBody;
use tracing::debug;

use crate::udp::path::decode_addr;

impl super::UdpWriter {
    /// Apply the demux's path change. On a new IP the path estimate
    /// starts over (RFC 9000 §9.4): the reliable band's RTT estimator,
    /// the game band's windowed RTT and estimate, and the congestion
    /// response (open, unpaced — what the pacer still holds goes out on
    /// its next pass). A new port alone (a NAT rebinding) is the same
    /// path: the estimate is kept.
    pub(super) fn apply_path(&mut self, frame: &FrameBody) {
        let Some(new) = decode_addr(&frame.payload) else {
            return; // the demux sends only whole addresses
        };
        let reset = new.ip() != self.peer.ip();
        debug!(conn = %self.conn, old = %self.peer, %new, reset, "rUDP: writer path changed");
        self.peer = new;
        self.path_changes += 1;
        if reset {
            self.path_resets += 1;
            let now = Instant::now();
            self.rel.new_path();
            self.feedback.new_path(now);
            self.pace.control.new_path(now);
            self.feedback
                .set_interval(self.pace.control.probe_interval());
        }
    }

    /// The writer's current peer (tests read where it sends).
    #[cfg(test)]
    pub(in crate::udp) fn peer(&self) -> std::net::SocketAddr {
        self.peer
    }
}
