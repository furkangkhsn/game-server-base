//! The connection actor's hop of a path state (BACKLOG B103,
//! [`crate::path`]): into the room's per-connection action channel, as
//! the internal marker, without ever waiting on it.
//!
//! A full channel (the member's own input is ahead of it) keeps the
//! state owed; the actor retries on every message it reads and a newer
//! state replaces the owed one — latest wins, nothing queues. A closed
//! channel means the room already ended the membership: the state stays
//! owed and the next room joined gets the newest one anyway (a join
//! resets the signal). Nothing here is a loss to count: the room always
//! ends up with the newest state while the membership lasts, and a
//! membership that ended has no one left to tell.

use crate::channel::{Mailbox, TrySend, try_send};
use crate::id::ConnectionId;
use crate::path::{PathSignal, path_action};
use crate::room::Action;

/// Offer the owed state, if any, to `actions` (`conn`'s room channel).
/// `None` when nothing was owed; otherwise what the channel did.
pub(crate) fn deliver(
    signal: &mut PathSignal,
    conn: ConnectionId,
    actions: &Mailbox<Action>,
) -> Option<TrySend> {
    let state = signal.owed()?;
    let sent = try_send(actions, path_action(conn, &state));
    if sent == TrySend::Sent {
        signal.delivered();
    }
    Some(sent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::channel;
    use crate::path::{PathPhase, PathState, read_path};

    fn paced(rate: u32) -> PathState {
        PathState {
            phase: PathPhase::Paced,
            rate: Some(rate),
            ..Default::default()
        }
    }

    /// A full channel keeps the state owed; a newer state replaces it;
    /// once the channel has room the NEWEST arrives, once.
    #[test]
    fn a_full_channel_defers_and_the_newest_state_wins() {
        let conn = ConnectionId(3);
        let (tx, mut rx) = channel::<Action>(1);
        let mut s = PathSignal::default();
        assert_eq!(deliver(&mut s, conn, &tx), None, "nothing owed");
        // The member's own input fills the channel.
        tx.try_send(Action {
            conn,
            player: crate::id::PlayerId(0),
            op: gsb_protocol::op::GAME_BAND_START,
            payload: bytes::Bytes::new(),
        })
        .expect("room for one");
        s.offer(paced(60_000));
        assert_eq!(deliver(&mut s, conn, &tx), Some(TrySend::Full));
        s.offer(paced(30_000));
        assert_eq!(deliver(&mut s, conn, &tx), Some(TrySend::Full));
        // The room pulls the input; the retry delivers the newest.
        assert!(read_path(&rx.try_recv().expect("the input")).is_none());
        assert_eq!(deliver(&mut s, conn, &tx), Some(TrySend::Sent));
        let got = rx.try_recv().expect("the path marker");
        assert_eq!(got.conn, conn);
        assert_eq!(read_path(&got), Some(Some(paced(30_000))));
        assert!(rx.try_recv().is_err(), "the superseded state never goes");
        assert_eq!(deliver(&mut s, conn, &tx), None, "delivered: nothing owed");
    }

    /// A closed channel (the membership ended) delivers nothing; the next
    /// membership, once reset, is owed the newest state.
    #[test]
    fn a_new_membership_gets_the_newest_state() {
        let conn = ConnectionId(4);
        let mut s = PathSignal::default();
        let (old, old_rx) = channel::<Action>(4);
        drop(old_rx);
        s.offer(paced(20_000));
        assert_eq!(deliver(&mut s, conn, &old), Some(TrySend::Closed));
        let (new, mut new_rx) = channel::<Action>(4);
        s.reset();
        assert_eq!(deliver(&mut s, conn, &new), Some(TrySend::Sent));
        assert_eq!(
            read_path(&new_rx.try_recv().expect("marker")),
            Some(Some(paced(20_000)))
        );
        // A delivered state is owed again to the NEXT room only.
        assert_eq!(deliver(&mut s, conn, &new), None);
        let (third, mut third_rx) = channel::<Action>(4);
        s.reset();
        assert_eq!(deliver(&mut s, conn, &third), Some(TrySend::Sent));
        assert!(third_rx.try_recv().is_ok());
    }
}
