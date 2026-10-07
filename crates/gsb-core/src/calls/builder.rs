//! The builder `calls.call(..)` returns (SERVICES.md §5.2).

use crate::calls::contract::{Call, Identity, Owner, Request};
use crate::calls::route::CallSpec;
use crate::calls::table::{Answer, Calls, ReplyTo, encode_answer, entry_of};

/// The builder [`Calls::call`] returns. The call is written into the
/// table when the builder ends (the end of the statement) — there is no
/// `.send()`; every method is optional.
///
/// Who gets the result: the game (`take_resolved`) when `.owner(..)` or
/// `.with(..)` was given, or when `.answers(..)` was not; an
/// `.answers(..)`-only call answers its client and nobody else.
pub struct CallBuilder<'a, C: Call, X: Send + Sync + 'static = ()> {
    pub(crate) calls: Option<&'a mut Calls>,
    pub(crate) b: Option<Build<C, X>>,
}

pub(crate) struct Build<C: Call, X> {
    pub(crate) spec: CallSpec<C>,
    pub(crate) owner: Option<Owner>,
    pub(crate) ctx: X,
    pub(crate) has_ctx: bool,
    pub(crate) answer: Option<Answer>,
    pub(crate) player: Option<Identity>,
}

impl<'a, C: Call, X: Send + Sync + 'static> CallBuilder<'a, C, X> {
    /// Whom the result belongs to: `in_flight::<C>(owner)`, `export`, and
    /// "owner gone" all use it. Default: [`Owner::ACTOR`].
    pub fn owner(mut self, owner: Owner) -> Self {
        if let Some(b) = &mut self.b {
            b.owner = Some(owner);
        }
        self
    }

    /// Context that comes back with the result (stays in the table; never
    /// sent to the service).
    pub fn with<Y: Send + Sync + 'static>(mut self, ctx: Y) -> CallBuilder<'a, C, Y> {
        let calls = self.calls.take();
        let b = self.b.take().map(|b| Build {
            spec: b.spec,
            owner: b.owner,
            ctx,
            has_ctx: true,
            answer: b.answer,
            player: b.player,
        });
        CallBuilder { calls, b }
    }

    /// Answer this client request with the call's outcome automatically
    /// (typed: the request's answer and error must be the call's). Also
    /// makes the request's verified identity the call's `cx.player()`.
    pub fn answers<R>(mut self, to: ReplyTo<R>) -> Self
    where
        R: Request<Response = C::Response, Error = C::Error>,
    {
        if let (Some(calls), Some(b)) = (self.calls.as_deref_mut(), self.b.as_mut()) {
            match calls.open.remove(&to.id) {
                Some(o) => {
                    if b.player.is_none() {
                        b.player = Some(o.from);
                    }
                    b.answer = Some(Answer {
                        conn: o.conn,
                        wire_id: o.wire_id,
                        op: o.op,
                        encode: encode_answer::<R>,
                    });
                }
                None => calls.counts.requests_replied_late += 1,
            }
        }
        self
    }

    /// The verified player the call acts for (`cx.player()`), when there
    /// is no request to `.answers`.
    pub fn on_behalf_of(mut self, who: &Identity) -> Self {
        if let Some(b) = &mut self.b {
            b.player = Some(who.clone());
        }
        self
    }
}

impl<C: Call, X: Send + Sync + 'static> Drop for CallBuilder<'_, C, X> {
    fn drop(&mut self) {
        if let (Some(calls), Some(b)) = (self.calls.take(), self.b.take()) {
            let wants = b.owner.is_some() || b.has_ctx || b.answer.is_none();
            let ctx = if wants { Some(b.ctx) } else { None };
            let mut e = entry_of::<C, X>(b.spec, b.owner, ctx, b.player);
            e.answer = b.answer;
            calls.push(e);
        }
    }
}
