//! The game band's half of the writer: one RAW datagram when the frame
//! fits the budget (the bytes it has always been), FRAG datagrams when it
//! does not, and the drop+count rule past [`FRAG_MAX_COUNT`]. A child of
//! [`super`], so the writer's state stays private to its module tree.

use gsb_protocol::FrameBody;
use tracing::warn;

use crate::udp::*;

impl super::UdpWriter {
    /// Send one game-band frame. No retransmission, no state beyond the
    /// message id: a lost fragment loses its message, and the band is
    /// self-healing (the next full snapshot, at the latest the keep-alive,
    /// replaces it).
    pub(super) async fn send_game(&mut self, frame: &FrameBody) {
        let datagram = encode_raw(frame);
        if datagram.len() <= self.max_datagram {
            self.send(&datagram, false).await;
            return;
        }
        // The message is what the RAW datagram carries after its kind
        // byte: `[u16 op][payload]`, cut into budget-sized chunks.
        let Some(fragments) = split(self.frag_id, &datagram[1..], self.max_datagram) else {
            self.dropped_oversized += 1;
            if !self.oversized_warned {
                self.oversized_warned = true;
                warn!(
                    conn = %self.conn,
                    peer = %self.peer,
                    op = frame.op,
                    size = datagram.len(),
                    budget = self.max_datagram,
                    max_fragments = FRAG_MAX_COUNT,
                    "rUDP: frame exceeds the fragmentation ceiling; such frames \
                     are dropped and counted (split the snapshot group or lower \
                     its emission rate — the room-side max_snapshot_bytes \
                     counter is the signal)"
                );
            }
            return;
        };
        self.frag_id = self.frag_id.wrapping_add(1);
        for d in &fragments {
            self.send(d, false).await;
        }
        self.frag_messages += 1;
        self.frag_datagrams += fragments.len() as u64;
    }
}
