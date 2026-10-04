//! Test mode: services are ordinary trait impls (fakes), the ENGINE owns
//! the timing. No runtime: after each room tick the host pulls the
//! mailboxes, polls the service futures with a no-op waker, and releases
//! each answer when its `Delay` says. Same input, same ticks, same
//! answers — every run.
//!
//! Tower layers are not applied here (timing is the engine's in test
//! mode; a `timeout` is a live concern).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use tokio::sync::mpsc;

use crate::contract::{Erased, Failure};
use crate::route::{
    BoxError, BoxFuture, Delay, Dispatch, Envelope, Host, ReplySink, Route, Served, ServiceDef,
    ServiceId, ServiceKind, ServiceStats,
};

struct Running {
    issued: u64,
    fut: Option<BoxFuture>,
    out: Option<Result<Erased, BoxError>>,
    sink: Option<ReplySink>,
}

trait SimService: Send {
    fn kind(&self) -> TypeId;
    fn step(&mut self, now: u64);
    fn kill(&mut self);
}

struct SimSvc<K: ServiceKind, R> {
    rx: Option<mpsc::Receiver<Envelope<K::Msg>>>,
    router: R,
    delay: Delay,
    running: Vec<Running>,
    stats: Arc<ServiceStats>,
}

impl<K, R> SimService for SimSvc<K, R>
where
    K: ServiceKind,
    R: tower::Service<Dispatch<K::Msg>, Response = Erased, Error = BoxError, Future = BoxFuture>
        + Send,
{
    fn kind(&self) -> TypeId {
        TypeId::of::<K>()
    }

    fn step(&mut self, now: u64) {
        let mut cx = Context::from_waker(Waker::noop());
        if let Some(rx) = &mut self.rx {
            while let Ok(env) = rx.try_recv() {
                self.stats
                    .calls_received
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Envelope { cx: ccx, msg, sink } = env;
                let _ = self.router.poll_ready(&mut cx);
                let fut = self.router.call(Dispatch { cx: ccx, msg });
                self.running.push(Running {
                    issued: now,
                    fut: Some(fut),
                    out: None,
                    sink: Some(sink),
                });
            }
        }
        for r in &mut self.running {
            if let Some(f) = &mut r.fut
                && let Poll::Ready(out) = f.as_mut().poll(&mut cx)
            {
                r.out = Some(out);
                r.fut = None;
            }
        }
        let Delay::Ticks(n) = self.delay else {
            return;
        };
        let mut i = 0;
        while i < self.running.len() {
            let r = &self.running[i];
            if r.out.is_some() && now + 1 >= r.issued + n {
                let mut r = self.running.remove(i);
                let sink = r.sink.take().expect("one sink per call");
                match r.out.take().expect("checked") {
                    Ok(o) => sink.complete(o),
                    Err(_) => sink.fail(Failure::Abandoned),
                }
            } else {
                i += 1;
            }
        }
    }

    fn kill(&mut self) {
        self.rx = None;
        self.running.clear();
    }
}

/// The test-mode host.
#[derive(Default)]
pub struct Sim {
    routes: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    services: Vec<Box<dyn SimService>>,
    next: u32,
}

impl Sim {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a (fake) implementation: `sim.service(Delay::ticks(3, Inventory::serve(Fake)), 8)`.
    /// `mailbox` is the mailbox capacity.
    pub fn service<K, S>(&mut self, served: Served<K, S>, mailbox: usize) -> Arc<ServiceStats>
    where
        K: ServiceDef<S>,
        S: Send + Sync + 'static,
    {
        let (tx, rx) = mpsc::channel(mailbox.max(1));
        let stats = Arc::new(ServiceStats::default());
        let route = Route {
            id: ServiceId(self.next),
            name: K::NAME,
            tx,
            stats: Arc::clone(&stats),
        };
        self.next += 1;
        self.routes.insert(TypeId::of::<K>(), Box::new(route));
        self.services.push(Box::new(SimSvc::<K, K::Router> {
            rx: Some(rx),
            router: K::router(Arc::new(served.svc)),
            delay: served.delay,
            running: Vec::new(),
            stats: Arc::clone(&stats),
        }));
        stats
    }

    /// Run the services after the actor's tick `now` (the tick whose
    /// flush sent the calls).
    pub fn step(&mut self, now: u64) {
        for s in &mut self.services {
            s.step(now);
        }
    }

    /// "The service died": its mailbox closes (later calls: `Gone`) and
    /// every call it held answers `Abandoned`.
    pub fn kill<K: ServiceKind>(&mut self) {
        for s in &mut self.services {
            if s.kind() == TypeId::of::<K>() {
                s.kill();
            }
        }
    }
}

impl Host for Sim {
    fn route<K: ServiceKind>(&self) -> Route<K::Msg> {
        self.routes
            .get(&TypeId::of::<K>())
            .and_then(|r| r.downcast_ref::<Route<K::Msg>>())
            .unwrap_or_else(|| panic!("service {} is not registered", K::NAME))
            .clone()
    }
}
