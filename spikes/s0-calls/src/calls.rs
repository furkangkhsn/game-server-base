//! The room side of a call (SERVICES.md §4.2–4.4): the call table the
//! tick context lends to the logic, and the RESPONSES-phase reconcile.
//!
//! Interior state is `Cell` take/set (the kick queue's idiom): the hooks
//! hold `&TickCtx`, so calling is a push, never a re-entry.

use std::any::Any;
use std::cell::Cell;
use std::collections::HashMap;

use tokio::sync::mpsc;

use crate::request::Request;
use crate::service::{Body, Call, Completion, Reply, ServiceRef};

/// A call's identity: `origin` (the calling actor incarnation, unique per
/// process start) in the high half, a sequence in the low half. Unique,
/// so it doubles as the service's idempotency key; stable, because a
/// re-issue (after a migration or a restart) passes the SAME key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CallKey(pub u128);

impl CallKey {
    pub fn new(origin: u64, seq: u64) -> Self {
        Self((u128::from(origin) << 64) | u128::from(seq))
    }

    pub fn origin(self) -> u64 {
        (self.0 >> 64) as u64
    }

    pub fn seq(self) -> u64 {
        self.0 as u64
    }
}

/// Why a call was refused at the call site (fail fast — nothing was
/// sent, nothing is pending).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// The service's mailbox is full.
    Full,
    /// The service is gone (its receiver dropped).
    Gone,
    /// This room already has its share of this service in flight.
    ServiceCap,
    /// A call with this key is already in flight here.
    DuplicateKey,
}

/// Every way a call can end besides the service's own answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError<E> {
    /// The service answered with its typed error.
    Service(E),
    /// No answer by the deadline (the answer, if it comes, is `late`).
    TimedOut,
    /// The service dropped the reply without answering (it died, panicked
    /// or forgot).
    Abandoned,
    /// Never sent (only seen through [`Refused::fail`]).
    Refused(RefuseReason),
}

/// What the tag constructor receives.
pub type Outcome<R> = Result<<R as Request>::Response, CallError<<R as Request>::Error>>;

/// A refused call: the reason, the request back (as `try_send` hands back
/// its message) and the tag constructor — [`Self::fail`] turns it into the
/// game's `D` so a refusal can be handled where every other failure is.
pub struct Refused<R: Request, D> {
    pub reason: RefuseReason,
    pub req: R,
    tag: Box<dyn FnOnce(Outcome<R>) -> D + Send>,
}

impl<R: Request, D> Refused<R, D> {
    /// The game's own message for this refusal (runs the tag with
    /// `Err(CallError::Refused(reason))`).
    pub fn fail(self) -> D {
        (self.tag)(Err(CallError::Refused(self.reason)))
    }
}

impl<R: Request, D> std::fmt::Debug for Refused<R, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Refused")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

/// One finished call, as the RESPONSES phase hands it to the logic.
#[derive(Debug)]
pub struct Done<D> {
    pub key: CallKey,
    pub msg: D,
}

/// The call path's loss/outcome counters — one meaning each (SERVICES.md
/// §8). Cumulative.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CallCounters {
    pub calls_sent: u64,
    pub calls_refused_full: u64,
    pub calls_refused_gone: u64,
    pub calls_refused_service_cap: u64,
    pub calls_refused_duplicate_key: u64,
    pub calls_done_ok: u64,
    pub calls_done_service_error: u64,
    pub calls_timed_out: u64,
    pub calls_abandoned: u64,
    pub calls_late: u64,
    pub calls_dropped_migrated: u64,
    pub calls_lost_teardown: u64,
}

/// The call path's knobs.
#[derive(Debug, Clone, Copy)]
pub struct CallConfig {
    /// Deadline of a call, in ticks of the calling actor.
    pub timeout_ticks: u64,
    /// Per (room, service) in-flight share: the fairness cap.
    pub max_in_flight_per_service: u32,
}

impl Default for CallConfig {
    fn default() -> Self {
        Self {
            timeout_ticks: 150,
            max_in_flight_per_service: 32,
        }
    }
}

pub(crate) enum Answer {
    Body(Box<dyn Any + Send>),
    TimedOut,
    Abandoned,
}

#[derive(Clone, Copy)]
enum Kind {
    Ok,
    ServiceError,
    TimedOut,
    Abandoned,
}

type Deliver<D> = Box<dyn FnOnce(Answer) -> Option<(D, Kind)> + Send>;

struct Pending<D> {
    service: &'static str,
    attempt: u64,
    due: u64,
    deliver: Deliver<D>,
}

/// The calling actor's in-flight calls. Owned by the room; lent to the
/// hooks (shared) through the tick context; reconciled (exclusively) in
/// the RESPONSES phase.
pub struct CallTable<D> {
    origin: u64,
    cfg: CallConfig,
    next_seq: Cell<u64>,
    next_attempt: Cell<u64>,
    now: Cell<u64>,
    in_flight: Cell<u32>,
    pending: Cell<HashMap<CallKey, Pending<D>>>,
    per_service: Cell<HashMap<&'static str, u32>>,
    counters: Cell<CallCounters>,
    tx: mpsc::Sender<Completion>,
    rx: mpsc::Receiver<Completion>,
}

impl<D: 'static> CallTable<D> {
    /// `completion_capacity`: the room's completion channel. Sizing rule
    /// (SERVICES.md §7): ≥ 2 × the room's total in-flight allowance, so
    /// late answers of timed-out calls cannot crowd out current ones.
    pub fn new(origin: u64, cfg: CallConfig, completion_capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(completion_capacity.max(1));
        Self {
            origin,
            cfg,
            next_seq: Cell::new(1),
            next_attempt: Cell::new(1),
            now: Cell::new(0),
            in_flight: Cell::new(0),
            pending: Cell::new(HashMap::new()),
            per_service: Cell::new(HashMap::new()),
            counters: Cell::new(CallCounters::default()),
            tx,
            rx,
        }
    }

    pub fn counters(&self) -> CallCounters {
        self.counters.get()
    }

    pub fn in_flight(&self) -> u32 {
        self.in_flight.get()
    }

    pub(crate) fn set_now(&self, now: u64) {
        self.now.set(now);
    }

    fn count(&self, f: impl FnOnce(&mut CallCounters)) {
        let mut c = self.counters.get();
        f(&mut c);
        self.counters.set(c);
    }

    fn fresh_key(&self) -> CallKey {
        let seq = self.next_seq.get();
        self.next_seq.set(seq + 1);
        CallKey::new(self.origin, seq)
    }

    /// The call verb (behind `TickCtx::call` / `call_keyed`).
    pub(crate) fn call<R, M>(
        &self,
        svc: &ServiceRef<M>,
        key: Option<CallKey>,
        req: R,
        tag: Box<dyn FnOnce(Outcome<R>) -> D + Send>,
    ) -> Result<CallKey, Refused<R, D>>
    where
        R: Request,
        M: From<Call<R>>,
    {
        let refuse = |reason, req, tag| Err(Refused { reason, req, tag });
        let key = key.unwrap_or_else(|| self.fresh_key());

        let mut pending = self.pending.take();
        let mut per_service = self.per_service.take();
        let busy = pending.contains_key(&key);
        let share = per_service.get(svc.name()).copied().unwrap_or(0);
        let outcome = if busy {
            self.count(|c| c.calls_refused_duplicate_key += 1);
            refuse(RefuseReason::DuplicateKey, req, tag)
        } else if share >= self.cfg.max_in_flight_per_service {
            self.count(|c| c.calls_refused_service_cap += 1);
            refuse(RefuseReason::ServiceCap, req, tag)
        } else {
            // Reserve the mailbox slot FIRST: a refusal hands the request
            // and the tag back untouched (nothing was built, nothing sent).
            match svc.sender().try_reserve() {
                Err(mpsc::error::TrySendError::Full(())) => {
                    self.count(|c| c.calls_refused_full += 1);
                    refuse(RefuseReason::Full, req, tag)
                }
                Err(mpsc::error::TrySendError::Closed(())) => {
                    self.count(|c| c.calls_refused_gone += 1);
                    refuse(RefuseReason::Gone, req, tag)
                }
                Ok(permit) => {
                    let attempt = self.next_attempt.get();
                    self.next_attempt.set(attempt + 1);
                    let reply = Reply::<R>::new(key, attempt, self.tx.clone());
                    permit.send(M::from(Call { key, req, reply }));
                    pending.insert(
                        key,
                        Pending {
                            service: svc.name(),
                            attempt,
                            due: self.now.get() + self.cfg.timeout_ticks,
                            deliver: deliver::<R, D>(tag),
                        },
                    );
                    per_service.insert(svc.name(), share + 1);
                    self.in_flight.set(self.in_flight.get() + 1);
                    self.count(|c| c.calls_sent += 1);
                    Ok(key)
                }
            }
        };
        self.pending.set(pending);
        self.per_service.set(per_service);
        outcome
    }

    fn release(&self, per_service: &mut HashMap<&'static str, u32>, service: &'static str) {
        if let Some(n) = per_service.get_mut(service) {
            *n -= 1;
            if *n == 0 {
                per_service.remove(service);
            }
        }
        self.in_flight.set(self.in_flight.get() - 1);
    }

    fn finish(&self, key: CallKey, p: Pending<D>, answer: Answer, out: &mut Vec<Done<D>>) {
        if let Some((msg, kind)) = (p.deliver)(answer) {
            self.count(|c| match kind {
                Kind::Ok => c.calls_done_ok += 1,
                Kind::ServiceError => c.calls_done_service_error += 1,
                Kind::TimedOut => c.calls_timed_out += 1,
                Kind::Abandoned => c.calls_abandoned += 1,
            });
            out.push(Done { key, msg });
        } else {
            // Unreachable by construction (attempt-matched); counted, not
            // trusted.
            self.count(|c| c.calls_late += 1);
        }
    }

    /// The RESPONSES phase: drain the completion channel (one empty
    /// `try_recv` when quiet), reconcile against the pending set, then —
    /// only when something is in flight — sweep the deadlines. Returns
    /// whether a sweep ran (the idle-cost proof reads it).
    pub(crate) fn absorb(&mut self, out: &mut Vec<Done<D>>) -> bool {
        while let Ok(c) = self.rx.try_recv() {
            let mut pending = self.pending.take();
            let current = pending.get(&c.key).is_some_and(|p| p.attempt == c.attempt);
            if current {
                let p = pending.remove(&c.key).expect("checked above");
                let mut per_service = self.per_service.take();
                self.release(&mut per_service, p.service);
                self.per_service.set(per_service);
                let answer = match c.body {
                    Body::Answer(b) => Answer::Body(b),
                    Body::Abandoned => Answer::Abandoned,
                };
                self.finish(c.key, p, answer, out);
            } else {
                // Answer for a call that is no longer pending here:
                // timed out, dropped by a migration, or an older attempt.
                self.count(|c| c.calls_late += 1);
            }
            self.pending.set(pending);
        }
        if self.in_flight.get() == 0 {
            return false;
        }
        let now = self.now.get();
        let mut pending = self.pending.take();
        let mut expired: Vec<CallKey> = pending
            .iter()
            .filter(|(_, p)| p.due <= now)
            .map(|(k, _)| *k)
            .collect();
        expired.sort();
        let mut per_service = self.per_service.take();
        for key in expired {
            let p = pending.remove(&key).expect("collected above");
            self.release(&mut per_service, p.service);
            self.finish(key, p, Answer::TimedOut, out);
        }
        self.per_service.set(per_service);
        self.pending.set(pending);
        true
    }

    /// Migrate-out / room handoff: forget these calls here (their tags are
    /// dropped; the game carries its reserve — with the key — in its
    /// migration state and re-issues with `call_keyed`). A later answer
    /// lands here as `calls_late`.
    pub fn drop_for_migration(&self, keys: &[CallKey]) {
        let mut pending = self.pending.take();
        let mut per_service = self.per_service.take();
        for key in keys {
            if let Some(p) = pending.remove(key) {
                self.release(&mut per_service, p.service);
                self.count(|c| c.calls_dropped_migrated += 1);
            }
        }
        self.per_service.set(per_service);
        self.pending.set(pending);
    }

    /// Teardown: whatever is still in flight is lost to this incarnation
    /// (counted once; no `Done` is produced — there is no logic to take it).
    pub fn teardown(&self) {
        let pending = self.pending.take();
        let lost = pending.len() as u64;
        self.count(|c| c.calls_lost_teardown += lost);
        self.in_flight.set(0);
        self.per_service.set(HashMap::new());
    }
}

/// The typed half of a pending entry, monomorphized at the call site: it
/// knows `R`, so the downcast always matches (the attempt check keeps a
/// foreign answer out).
fn deliver<R: Request, D: 'static>(tag: Box<dyn FnOnce(Outcome<R>) -> D + Send>) -> Deliver<D> {
    Box::new(move |answer| {
        let (outcome, kind): (Outcome<R>, Kind) = match answer {
            Answer::Body(b) => match b.downcast::<Result<R::Response, R::Error>>() {
                Ok(r) => match *r {
                    Ok(resp) => (Ok(resp), Kind::Ok),
                    Err(e) => (Err(CallError::Service(e)), Kind::ServiceError),
                },
                Err(_) => return None,
            },
            Answer::TimedOut => (Err(CallError::TimedOut), Kind::TimedOut),
            Answer::Abandoned => (Err(CallError::Abandoned), Kind::Abandoned),
        };
        Some((tag(outcome), kind))
    })
}
