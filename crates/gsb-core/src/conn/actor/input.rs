//! The input gate's seat in the actor (see `crate::conn` "input gate"):
//! a join tunes it, a game-band frame asks it.

use tracing::warn;

use crate::room::InputRate;

impl super::ConnectionActor {
    /// A join succeeded into a room limiting input at `rate` (`None` =
    /// no limit): re-tune the connection's one bucket.
    pub(super) fn enter_input_rate(&mut self, rate: Option<InputRate>) {
        self.input.enter(rate, crate::ticker::now());
    }

    /// Whether this game-band action (`op`) is over the room's input
    /// rate: `true` = dropped here — counted, never queued, NOT a
    /// violation (docs/SECURITY.md, "post-auth input volume").
    ///
    /// The clock is the tick clock (`crate::ticker::now`,
    /// docs/TICK-ARCHITECTURE.md "Tick saati"): the rate is actions per
    /// second of the room's time, so a paused-clock test sees a virtual
    /// second refill a second's worth — on the wall clock it would see
    /// microseconds and refuse an honest client. In production the two
    /// are the same instant. Off (the default) reads no clock at all.
    pub(super) fn over_input_rate(&mut self, op: u16) -> bool {
        if !self.input.is_on() || self.input.admit(crate::ticker::now()) {
            return false;
        }
        self.m_input_limited += 1;
        if !self.m_input_limited_warned {
            self.m_input_limited_warned = true;
            warn!(
                %self.conn,
                peer = %self.peer,
                op,
                "input over the room's rate limit; this connection's excess \
                 game input is being dropped (counted as input_rate_limited \
                 in its metrics sample; not a violation — warned once)"
            );
        }
        true
    }
}
