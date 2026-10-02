//! The demux's half of the game band's feedback (module
//! `crate::udp::feedback`): a client's report goes to its session's
//! writer. A child of [`super`], so the demux's state stays private.

use std::net::SocketAddr;

use bytes::Bytes;
use gsb_protocol::{FrameBody, op};

impl super::Demux {
    /// A game-band report (module `crate::udp::feedback`): handed to the
    /// session's writer through its outbound channel, the way an ACK is —
    /// the writer owns the probes it answers. It is no sign of life for
    /// the idle window (neither is an ACK): liveness stays the client's
    /// own traffic.
    pub(super) fn handle_report(&mut self, peer: SocketAddr) {
        let Some(s) = self.sessions.get(&peer) else {
            self.no_session += 1;
            return;
        };
        let fb = FrameBody::new(
            op::base::UDP_REPORT,
            Bytes::copy_from_slice(&self.buf[1..9]),
        );
        match s.out_tx.try_send(vec![fb]) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.reports_not_forwarded += 1;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.reports_not_forwarded += 1;
                self.removed_actor_gone += 1;
                self.remove_session(peer);
            }
        }
    }
}
