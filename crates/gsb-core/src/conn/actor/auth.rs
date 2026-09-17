//! The AUTH_REQ path: the attempt window, ticket validation, and the
//! ticket-error reply that must not look like a protocol violation.

use std::time::Instant;

use tokio::sync::oneshot;
use tracing::{debug, warn};

use gsb_protocol::op;
use gsb_protocol::{FrameBody, ProtoError, base};

use crate::conn::*;
use crate::registry::RegistryMsg;

impl super::ConnectionActor {
    /// The AUTH_REQ arm of [`Self::handle_frame`]: the attempt window,
    /// ticket validation and the state transition into `Authenticated`.
    pub(super) async fn handle_auth(&mut self, frame: FrameBody) {
        if self.state != ConnState::WaitingAuth {
            self.reply_err(ProtoError::AlreadyAuthenticated).await;
            return;
        }
        // AUTH attempt window (§3.1): admitted before ANY
        // processing. Attempts one-to-three keep the ordinary path
        // (ticket rejection = ERROR code 10, connection alive — a
        // legitimate retry is never budgeted for trying); attempt
        // four-plus in-window is a HARD violation through the same
        // funnel as every other hard error (weight 4 → four of
        // them exhaust the budget and close), so the flood case
        // adds no new enforcement machinery.
        //
        // Only ADMITTED attempts enter the window (bounded at
        // [`AUTH_ATTEMPTS_PER_WINDOW`] entries forever): a
        // violator cannot extend its own occupancy, and the budget
        // — not the window — decides when flooding stops mattering
        // (four violations close). Ten quiet seconds always fully
        // restore an honest client's allowance.
        let now = Instant::now();
        self.auth_attempts
            .retain(|t| now.duration_since(*t) < AUTH_WINDOW);
        if self.auth_attempts.len() >= AUTH_ATTEMPTS_PER_WINDOW {
            // Auth-family ERROR code 3 (the documented "auth"
            // class: unauthenticated / re-auth) — no new wire
            // vocabulary; the message carries the specificity.
            self.count_violation(
                ViolationClass::Hard,
                3,
                format!(
                    "auth attempt rate limit exceeded: max \
                     {AUTH_ATTEMPTS_PER_WINDOW} attempts per \
                     {AUTH_WINDOW:?}; wait for the window to pass"
                ),
            )
            .await;
            return;
        }
        self.auth_attempts.push_back(now);
        let auth: base::Auth = match self.decode::<base::Auth>(frame.op, frame) {
            Ok(m) => m,
            Err(e) => {
                self.reply_err(e).await;
                return;
            }
        };
        // Ticket-auth (the control-plane hook, see `crate::auth`):
        // the hook is the identity authority. Two shapes, decided
        // by the server's configuration (not per frame):
        //
        // - NO hook (or an empty ticket on a local-auth server):
        //   the legacy path — `Auth.name` is accepted as-is.
        // - HOOK configured: the ticket must be non-empty and is
        //   validated ASYNCHRONOUSLY through the common deferred-
        //   completion mechanism: a spawned worker runs the
        //   validator (bounded by the hook's timeout) and reports
        //   to this handler over a single oneshot — the same
        //   round-trip idiom as the join above. The handler's
        //   park is bounded by the timeout, so a hung validator
        //   cannot park the actor forever (it resolves to a
        //   timeout rejection). While parked, at most ONE
        //   validation is in flight (structural: there is no
        //   concurrent frame handler) — the per-connection
        //   amplification bound (see `crate::auth`).
        if let Some(hook) = self.auth.clone() {
            if auth.ticket.is_empty() {
                // A ticket-auth server with no ticket: a normal
                // rejection (the client presents one — it does not
                // hold a valid ticket, and the connection stays
                // alive to retry).
                self.reply_ticket_error(crate::auth::TicketError::Rejected(
                    "no ticket presented".into(),
                ))
                .await;
                return;
            }
            let (reply_tx, reply_rx) = oneshot::channel::<
                Result<crate::auth::ValidatedTicket, crate::auth::TicketError>,
            >();
            let ticket = auth.ticket.clone();
            tokio::spawn(async move {
                // The validator's own future (the platform's
                // adapter — a signature-service call, a cache, …)
                // wrapped in the hook's timeout: the worker cannot
                // outlive `timeout` (a resource guard), and the
                // actor's park above cannot outlive it either.
                let outcome =
                    tokio::time::timeout(hook.timeout, (hook.validator)(ticket.into())).await;
                let result = match outcome {
                    Ok(r) => r,
                    Err(_elapsed) => Err(crate::auth::TicketError::TimedOut),
                };
                // The send fails (the receiver is dropped) when the
                // connection went away while validating: the actor
                // processed its `Closed` on its next message after
                // the park, and this worker simply exits.
                let _ = reply_tx.send(result);
            });
            match reply_rx.await {
                Ok(Ok(v)) => {
                    // Success: the hook's identity is installed (it
                    // supersedes `Auth.name`) and the ticket pins
                    // the room for the next join.
                    self.ticket = Some(v.clone());
                    self.identity = v.player.clone();
                    self.state = ConnState::Authed;
                    // §4: the connection leaves the registry's
                    // unauthenticated pool (the cap lives where the
                    // connection table lives). A failed send means
                    // the registry is gone (shutdown) — ignored,
                    // like every other fire-and-forget notice.
                    let _ = self
                        .registry
                        .send(RegistryMsg::Authed { conn: self.conn })
                        .await;
                    debug!(%self.conn, player = %v.player, room = %v.room, "ticket authenticated");
                    let _ = self
                        .send_frame(
                            op::base::AUTH_RESULT,
                            &base::AuthResult {
                                ok: true,
                                reason: String::new(),
                                player: v.player,
                                room: v.room.0,
                            },
                        )
                        .await;
                }
                Ok(Err(e)) => {
                    // Rejected or timed out: a NORMAL rejection —
                    // the connection stays alive (ERROR code 10)
                    // and the state stays WaitingAuth (a fresh
                    // ticket may be re-presented). Never counted
                    // against the violation budget (see
                    // `crate::auth` and the ERROR code docs).
                    self.reply_ticket_error(e).await;
                }
                Err(_dropped) => {
                    // The worker died without reporting (a panic
                    // inside the platform's validator): a
                    // server-side condition (weight 0, like the
                    // "registry gone" arm below) — answered as a
                    // generic failure, not a violation.
                    let _ = self
                        .send_frame(
                            op::base::ERROR,
                            &base::Error {
                                code: 7,
                                message: "ticket validator unavailable".into(),
                            },
                        )
                        .await;
                }
            }
            return;
        }
        self.state = ConnState::Authed;
        // §4: same unauthenticated-pool notice as the ticket path.
        let _ = self
            .registry
            .send(RegistryMsg::Authed { conn: self.conn })
            .await;
        // Local-auth path: the name IS the resume key (demo and
        // testing only — RECONNECT §4: on this path nothing
        // authoritative stands behind the name).
        self.identity = auth.name.clone();
        debug!(%self.conn, name = %auth.name, "authenticated");
        let _ = self
            .send_frame(
                op::base::AUTH_RESULT,
                &base::AuthResult {
                    ok: true,
                    reason: String::new(),
                    player: String::new(),
                    room: 0,
                },
            )
            .await;
    }

    /// The single exit for ticket-validation failures (see `crate::auth`
    /// for the classification decision): an `ERROR` frame with code 10
    /// (ticket validation failed — rejected or timed out), the
    /// connection STAYS ALIVE, and the violation budget is NOT touched
    /// (a bad ticket is a normal rejection: the frame was well-formed
    /// and the client can fix its state; the budget is for structural
    /// protocol errors). The state machine is unchanged (still
    /// `WaitingAuth`): a fresh ticket may be re-presented.
    pub(super) async fn reply_ticket_error(&mut self, e: crate::auth::TicketError) {
        warn!(%self.conn, %self.peer, reason = %e, "ticket validation failed (normal rejection; the connection stays alive)");
        let _ = self
            .send_frame(
                op::base::ERROR,
                &base::Error {
                    code: 10,
                    message: e.to_string(),
                },
            )
            .await;
    }
}
