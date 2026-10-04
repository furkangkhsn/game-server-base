//! The live host: each service is a mailbox actor on tokio; every call
//! runs through the game's tower stack (`|s| s.timeout(..)`).
//!
//! The actor's loop has one await (`recv`); each call is its own task
//! with one await (`oneshot` through the stack). No select, no lock.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tower::ServiceBuilder;
use tower::ServiceExt;
use tower::layer::util::{Identity as Id, Stack};

use crate::contract::Failure;
use crate::route::{
    Dispatch, Envelope, Host, Route, Served, ServiceDef, ServiceId, ServiceKind, ServiceStats,
};

/// What `server.service(.., |s| ..)` hands the closure: the tower stack
/// plus the mailbox size.
pub struct Setup<L> {
    builder: ServiceBuilder<L>,
    mailbox: usize,
}

impl<L> Setup<L> {
    pub fn timeout(self, d: Duration) -> Setup<Stack<tower::timeout::TimeoutLayer, L>> {
        Setup {
            builder: self.builder.timeout(d),
            mailbox: self.mailbox,
        }
    }
    pub fn concurrency_limit(
        self,
        n: usize,
    ) -> Setup<Stack<tower::limit::ConcurrencyLimitLayer, L>> {
        Setup {
            builder: self.builder.concurrency_limit(n),
            mailbox: self.mailbox,
        }
    }
    pub fn load_shed(self) -> Setup<Stack<tower::load_shed::LoadShedLayer, L>> {
        Setup {
            builder: self.builder.load_shed(),
            mailbox: self.mailbox,
        }
    }
    /// Any other tower layer (tracing, metrics, retry, …).
    pub fn layer<T>(self, layer: T) -> Setup<Stack<T, L>> {
        Setup {
            builder: self.builder.layer(layer),
            mailbox: self.mailbox,
        }
    }
    /// The service's mailbox capacity (default 1024).
    pub fn mailbox(mut self, n: usize) -> Self {
        self.mailbox = n.max(1);
        self
    }
}

/// The live composition root (the spike's stand-in for the server).
pub struct Server {
    rt: Handle,
    routes: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    next: u32,
}

impl Server {
    pub fn new(rt: Handle) -> Self {
        Self {
            rt,
            routes: HashMap::new(),
            next: 0,
        }
    }

    /// Register a service: `server.service(Inventory::serve(PgInventory::new(db)), |s| s.timeout(..))`.
    pub fn service<K, S, L, F>(&mut self, served: Served<K, S>, setup: F) -> Arc<ServiceStats>
    where
        K: ServiceDef<S>,
        S: Send + Sync + 'static,
        F: FnOnce(Setup<Id>) -> Setup<L>,
        L: tower::Layer<K::Router>,
        L::Service:
            tower::Service<Dispatch<K::Msg>, Response = crate::Erased> + Clone + Send + 'static,
        <L::Service as tower::Service<Dispatch<K::Msg>>>::Future: Send,
        <L::Service as tower::Service<Dispatch<K::Msg>>>::Error:
            Into<crate::route::BoxError> + Send,
    {
        let setup = setup(Setup {
            builder: ServiceBuilder::new(),
            mailbox: 1024,
        });
        let stack = setup.builder.service(K::router(Arc::new(served.svc)));
        let (tx, mut rx) = mpsc::channel::<Envelope<K::Msg>>(setup.mailbox);
        let stats = Arc::new(ServiceStats::default());
        let id = ServiceId(self.next);
        self.next += 1;
        let route = Route {
            id,
            name: K::NAME,
            tx,
            stats: Arc::clone(&stats),
        };
        self.routes.insert(TypeId::of::<K>(), Box::new(route));
        let st = Arc::clone(&stats);
        self.rt.spawn(async move {
            while let Some(env) = rx.recv().await {
                st.calls_received
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let svc = stack.clone();
                tokio::spawn(async move {
                    let Envelope { cx, msg, sink } = env;
                    match svc.oneshot(Dispatch { cx, msg }).await {
                        Ok(out) => sink.complete(out),
                        Err(e) => {
                            let e: crate::route::BoxError = e.into();
                            let f = if e.is::<tower::timeout::error::Elapsed>() {
                                Failure::TimedOut
                            } else if e.is::<tower::load_shed::error::Overloaded>() {
                                Failure::Refused(crate::Refusal::Shed)
                            } else {
                                Failure::Abandoned
                            };
                            sink.fail(f);
                        }
                    }
                });
            }
        });
        stats
    }
}

impl Host for Server {
    fn route<K: ServiceKind>(&self) -> Route<K::Msg> {
        self.routes
            .get(&TypeId::of::<K>())
            .and_then(|r| r.downcast_ref::<Route<K::Msg>>())
            .unwrap_or_else(|| panic!("service {} is not registered", K::NAME))
            .clone()
    }
}

/// The replay host: hands out routes to nowhere. In replay mode the
/// table never sends (recorded outcomes arrive by key), so a game built
/// against it runs unchanged and no service is called.
#[derive(Default)]
pub struct ReplayHost;

impl Host for ReplayHost {
    fn route<K: ServiceKind>(&self) -> Route<K::Msg> {
        let (tx, _rx) = mpsc::channel(1);
        Route {
            id: ServiceId(u32::MAX),
            name: K::NAME,
            tx,
            stats: Arc::new(ServiceStats::default()),
        }
    }
}
