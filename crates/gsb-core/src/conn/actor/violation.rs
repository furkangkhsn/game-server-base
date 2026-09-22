//! The protocol-violation budget: the error reply, the weighted count
//! that silences the funnel and eventually closes, and the pre-auth
//! frame cap that shares the same close path.

use tracing::{debug, warn};

use gsb_protocol::op;
use gsb_protocol::{ProtoError, base};

use crate::conn::*;

impl super::ConnectionActor {
    /// The single funnel for protocol errors: every `ERROR` frame this
    /// actor produces as a *response to a violation* passes through here,
    /// which classifies the error and hands it to [`Self::count_violation`]
    /// — where the violation budget lives (module docs).
    pub(super) async fn reply_err(&mut self, e: ProtoError) {
        // The number lives with the error type, not here:
        // `ProtoError::wire_code` is an exhaustive match in
        // `gsb-protocol`, so a new variant fails to compile there instead
        // of silently landing in the `_ =>` arm this site used to have.
        let code = e.wire_code();
        let message = e.to_string();
        let class = violation_class(&e);
        self.count_violation(class, code, message).await;
    }

    /// The weighted-budget machinery shared by every counted violation:
    /// `reply_err` after classifying a decoded protocol error, and the
    /// §3.1 AUTH-attempt window directly (which raises a hard violation
    /// without a `ProtoError` to classify). Behaviour per violation:
    ///
    /// 1. counted violations add their weight to the lifetime score and
    ///    increment the event count (this also feeds the metrics delta);
    /// 2. if fewer than [`VIOLATION_ANSWER_LIMIT`] violations have been
    ///    answered so far, send the `ERROR` frame (the diagnosis);
    ///    otherwise stay silent (amplification is bounded here);
    /// 3. if the score just reached [`VIOLATION_BUDGET`], send the close
    ///    notice ([`base::ErrorCode::ServerClosed`], the reason in the
    ///    message — same class as idle timeout / connection capacity: a
    ///    *server* decision, the message carries the specificity), emit
    ///    the structured close signal with the peer address, and flag the
    ///    run loop to tear the connection down.
    pub(super) async fn count_violation(
        &mut self,
        class: ViolationClass,
        code: base::ErrorCode,
        message: String,
    ) {
        let weight = class.weight();
        if weight == 0 {
            // Server-side condition: answered exactly as before the
            // budget existed, never counted (a client cannot fix the
            // registry being gone, and shutdown is transient).
            let _ = self
                .send_frame(op::base::ERROR, &base::Error::new(code, message))
                .await;
            return;
        }
        self.v_events += 1;
        self.v_score = self.v_score.saturating_add(weight);
        self.m_violations += 1;
        if self.v_answered < VIOLATION_ANSWER_LIMIT {
            self.v_answered += 1;
            debug!(
                %self.conn,
                %self.peer,
                code = code as i32,
                ?class,
                score = self.v_score,
                "protocol violation answered (one of the first \
                 {VIOLATION_ANSWER_LIMIT}; later ones are silent)"
            );
            let _ = self
                .send_frame(op::base::ERROR, &base::Error::new(code, message))
                .await;
        }
        if self.v_score >= VIOLATION_BUDGET && !self.v_closing {
            self.v_closing = true;
            self.server_closing(ServerClose::ViolationBudget);
            let reason = format!(
                "protocol violation budget exhausted: {} violations ({} \
                 answered) in this connection's lifetime",
                self.v_events, self.v_answered
            );
            // The close signal for a layer outside the server (firewall,
            // fail2ban, future auth): conn id, PEER ADDRESS, the violation
            // counts, and the reason — self-contained, structured, on the
            // tracing path every operator already collects.
            warn!(
                %self.conn,
                %self.peer,
                violations = self.v_events,
                answered = self.v_answered,
                score = self.v_score,
                "closing connection: protocol violation budget exhausted"
            );
            let _ = self
                .send_frame(
                    op::base::ERROR,
                    &base::Error::new(base::ErrorCode::ServerClosed, reason),
                )
                .await;
        }
    }

    /// The §3.3 pre-auth frame-budget close: an immediate
    /// [`base::ErrorCode::ServerClosed`] naming the policy (same
    /// server-decision class as the capacity and budget closes), then the
    /// ordinary teardown cascade via `p_closing`.
    pub(super) async fn close_preauth_budget(&mut self) {
        self.p_closing = true;
        self.server_closing(ServerClose::PreauthBudget);
        warn!(
            %self.conn,
            %self.peer,
            frames = self.preauth_frames,
            budget = PREAUTH_FRAME_BUDGET,
            "closing connection: pre-auth frame budget exhausted"
        );
        let _ = self
            .send_frame(
                op::base::ERROR,
                &base::Error::new(
                    base::ErrorCode::ServerClosed,
                    format!(
                        "pre-auth frame budget exhausted: more than \
                         {PREAUTH_FRAME_BUDGET} frames received before \
                         authentication"
                    ),
                ),
            )
            .await;
    }
}
