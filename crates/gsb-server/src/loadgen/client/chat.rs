//! The chat/presence scenario's client half (`--chat-rate`, `--friends`;
//! `crate::chat` is the server's): a client that announced
//! `FEATURE_PUSH` feeds every `PUSH` frame to its `gsb_client::events`
//! table — the client building block a game's client uses — and keeps
//! what the scenario measures: the table's counters, whether it saw a
//! gap, the chat lines and their push → receive latency, the friends'
//! presence events.

use bytes::Bytes;
use gsb_client::events::{EventCounters, Events};
use gsb_demo::game::{FriendOffline, FriendOnline, ItemGranted};
use gsb_protocol::FrameBody;
use gsb_protocol::op;

use crate::chat::{ChatLine, now_us};

/// One client's numbers.
#[derive(Debug, Clone, Default)]
pub(crate) struct EventTally {
    /// The table's own counters.
    pub(crate) counters: EventCounters,
    /// Whether the table reported a gap.
    pub(crate) gapped: bool,
    /// Chat lines taken.
    pub(crate) chat_lines: u64,
    /// `FriendOnline` events taken.
    pub(crate) friend_online: u64,
    /// `FriendOffline` events taken.
    pub(crate) friend_offline: u64,
    /// `ItemGranted` events taken (the economy's grant, on a run with
    /// `--rpc-rate` beside the scenario): taken so they never fill the
    /// queue.
    pub(crate) item_granted: u64,
    /// Each chat line's push → receive latency, µs.
    pub(crate) chat_lat_us: Vec<u32>,
}

impl EventTally {
    /// Add another client's.
    pub(crate) fn add(&mut self, o: &Self) {
        self.counters.add(&o.counters);
        self.gapped |= o.gapped;
        self.chat_lines += o.chat_lines;
        self.friend_online += o.friend_online;
        self.friend_offline += o.friend_offline;
        self.item_granted += o.item_granted;
        self.chat_lat_us.extend_from_slice(&o.chat_lat_us);
    }
}

/// A client's event half: the table and the tally.
pub(crate) struct EventsClient {
    table: Events,
    tally: EventTally,
}

impl EventsClient {
    pub(crate) fn new() -> Self {
        Self {
            table: Events::new(),
            tally: EventTally::default(),
        }
    }

    /// One received frame: `true` when it was a `PUSH` (consumed).
    pub(crate) fn frame(&mut self, op: u16, payload: &Bytes) -> bool {
        if op != op::base::PUSH {
            return false;
        }
        self.table.ingest(&FrameBody::new(op, payload.clone()));
        let at = now_us();
        for p in self.table.take::<ChatLine>() {
            self.tally.chat_lines += 1;
            let lat = at.saturating_sub(p.body.sent_at_us);
            self.tally
                .chat_lat_us
                .push(lat.min(u64::from(u32::MAX)) as u32);
        }
        self.tally.friend_online += self.table.take::<FriendOnline>().len() as u64;
        self.tally.friend_offline += self.table.take::<FriendOffline>().len() as u64;
        self.tally.item_granted += self.table.take::<ItemGranted>().len() as u64;
        if !self.table.take_gaps().is_empty() {
            self.tally.gapped = true;
        }
        true
    }

    /// The client's numbers at its end.
    pub(crate) fn finish(mut self) -> EventTally {
        self.tally.counters = self.table.counters();
        self.tally
    }
}

#[cfg(test)]
mod tests {
    use gsb_protocol::event::Event;
    use prost::Message;

    use super::*;

    fn push<E: Event>(seq: u64, e: &E) -> Bytes {
        gsb_protocol::base::Push {
            publisher: 1,
            seq,
            op: u32::from(E::OP),
            body: e.to_wire(),
            stream: 1,
        }
        .encode_to_vec()
        .into()
    }

    #[test]
    fn a_client_tallies_its_lines_presence_and_gaps() {
        let mut c = EventsClient::new();
        assert!(!c.frame(op::base::HEARTBEAT_ACK, &Bytes::new()));
        let line = ChatLine {
            channel: 0,
            line: 1,
            sent_at_us: now_us(),
            text: Vec::new(),
        };
        assert!(c.frame(op::base::PUSH, &push(1, &line)));
        let on = FriendOnline { who: "lg-1".into() };
        assert!(c.frame(op::base::PUSH, &push(3, &on)));
        let t = c.finish();
        assert_eq!((t.chat_lines, t.friend_online, t.friend_offline), (1, 1, 0));
        assert!(t.gapped);
        assert_eq!((t.counters.received, t.counters.missed), (2, 1));
        assert_eq!(t.chat_lat_us.len(), 1);
    }
}
