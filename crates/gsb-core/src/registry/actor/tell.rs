//! The registry's notices to a connection, and the one way one can be
//! lost uncounted (BACKLOG F58).
//!
//! Every notice goes the stop-message way (`crate::channel::post`, F57):
//! in place when the connection's inbox has room — ahead of anything the
//! registry sends it later, the stop's `ConnIn::Shutdown` above all —
//! and from a spawned sender only when the inbox is full. That sender
//! waits for a slot; if the connection ends first, the send is refused.
//! At the stop that happens when the stop's own notice reaches the
//! connection first: the verdict — decided before the stop, since the
//! registry handles its mailbox in order — never reaches the client, and
//! nothing had counted it (the connection's end counts only what is
//! still in its inbox, F56).
//!
//! So a refused VERDICT is counted where it is refused, as one
//! `MetricsEvent::VerdictsLost` under its reason — when the registry has
//! stopped by then (its mailbox closed: the `Shutdown` arm closes it
//! before it notifies anyone). A refusal while the registry runs is a
//! connection that ended on its own before the notice reached it:
//! nothing is lost, as behind a client's end in its inbox. A notice that
//! is no verdict (`LeftRoom`: the registry already settled the row) costs
//! nothing when refused.
//!
//! Bounded imprecision: a connection whose end ALSO found a verdict in
//! its inbox (a pump's, behind the stop's notice) counts that one, and a
//! refused registry verdict for the same session is counted too — two
//! verdicts for one session, at the stop, with its inbox full. And a
//! session whose client ended at the stop, before a pending verdict
//! reached it, is counted though the connection's own end would not
//! count one behind the client's `Closed`.

use std::fmt::Debug;
use std::hash::Hash;

use crate::channel::Mailbox;
use crate::conn::ConnIn;
use crate::metrics::VerdictsLost;
use crate::registry::actor::Registry;

impl<W, G, St, Sp> Registry<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip payload's trait bounds (`GameLogic::Strip`) — the
    // registry never inspects payloads, but both actor shapes it spawns
    // require them.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Post the verdict `msg` to a connection's `inbox`, never awaited;
    /// refused after the registry's stop, it is counted lost (module
    /// docs).
    pub(super) fn tell(&self, inbox: &Mailbox<ConnIn>, msg: ConnIn) {
        let registry = self.self_mailbox.clone();
        let metrics = self.metrics.clone();
        crate::channel::post_or(inbox, msg, move |msg| {
            if !registry.is_closed() {
                return;
            }
            if let Some(reason) = msg.verdict() {
                let mut lost = VerdictsLost::default();
                lost.close(reason);
                crate::room::send_verdicts_lost(&metrics, &lost);
            }
        });
    }
}
