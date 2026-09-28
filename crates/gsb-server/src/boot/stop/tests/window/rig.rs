//! The rig of the stop-order tests: a door the test lets peers through,
//! silent peers, and a registry that is busy until the test says go.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc, oneshot};

use gsb_core::channel::{Inbox, channel};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use gsb_core::metrics::{MetricSink, MetricsCollector};
use gsb_core::registry::RegistryMsg;
use gsb_net::transport::{BoxFuture, Door, Endpoint, Listener};

use crate::boot::ServerHandle;
use crate::boot::accept::{AcceptPipeline, ConnIdSeq, run_accept};

/// The message that keeps the registry's one mailbox slot busy.
pub(super) const BUSY: ConnectionId = ConnectionId(u64::MAX);

/// A door whose peers the test lets in one at a time.
pub(super) struct TestDoor {
    door: Door,
    pub(super) peers: Semaphore,
}

impl Listener for TestDoor {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, io::Result<Endpoint>> {
        Box::pin(async move {
            self.door
                .admit(async {
                    self.peers
                        .acquire()
                        .await
                        .map_err(io::Error::other)?
                        .forget();
                    Ok(silent_peer())
                })
                .await
        })
    }

    fn close(&self) {
        self.door.close();
    }
}

/// A peer that keeps its connection open and says nothing: its reader
/// holds the actor's inbox (as a socket pump does until the idle window),
/// its writer takes whatever the actor sends.
fn silent_peer() -> Endpoint {
    Endpoint::new(|_conn, in_tx, mut out_rx, _timeouts| {
        let reader = tokio::spawn(async move {
            let _open = in_tx;
            std::future::pending::<()>().await
        });
        let writer = tokio::spawn(async move { while out_rx.recv().await.is_some() {} });
        (Some(reader), writer)
    })
}

/// What the registry read, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Seen {
    Opened(ConnectionId),
    Shutdown,
    Closed(ConnectionId),
}

/// The registry, busy until `go`: then it reads as the real one does —
/// registers every `ConnOpened` and, on `Shutdown`, tells every
/// registered connection (a spawned `ConnIn::Shutdown`). The real one
/// stops reading there; this one goes on only to log what came too late.
fn registry(
    mut inbox: Inbox<RegistryMsg>,
    go: oneshot::Receiver<()>,
) -> mpsc::UnboundedReceiver<Seen> {
    let (log, seen) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _ = go.await;
        let mut registered = Vec::new();
        while let Some(msg) = inbox.recv().await {
            let event = match msg {
                RegistryMsg::ConnOpened { conn, inbox } => {
                    registered.push(inbox);
                    Seen::Opened(conn)
                }
                RegistryMsg::Shutdown => {
                    for inbox in registered.drain(..) {
                        tokio::spawn(async move {
                            let _ = inbox.send(ConnIn::Shutdown).await;
                        });
                    }
                    Seen::Shutdown
                }
                RegistryMsg::ConnClosed { conn } => Seen::Closed(conn),
                _ => continue,
            };
            let _ = log.send(event);
        }
    });
    seen
}

/// Every task runs until it waits on something (the paused clock
/// advances only once none can).
pub(super) async fn settle() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

/// A server of one test door over the busy registry: its handle, the
/// door, the registry's `go` and its log.
pub(super) fn server() -> (
    ServerHandle,
    Arc<TestDoor>,
    oneshot::Sender<()>,
    mpsc::UnboundedReceiver<Seen>,
) {
    let (reg_tx, reg_rx) = channel::<RegistryMsg>(1);
    reg_tx
        .try_send(RegistryMsg::ConnClosed { conn: BUSY })
        .expect("the mailbox has its one slot");
    let (go, go_rx) = oneshot::channel();
    let seen = registry(reg_rx, go_rx);
    let (ticker, ticker_task) = gsb_core::ticker::Ticker::spawn(20.0, 64).expect("ticker");
    let (metrics_tx, metrics_rx) = mpsc::channel(64);
    let metrics = tokio::spawn(
        MetricsCollector::new(
            ticker.subscribe(),
            metrics_rx,
            MetricSink::Log,
            Duration::from_secs(1),
        )
        .with_final_grace(Duration::from_millis(500))
        .run(),
    );
    drop(ticker);
    let door = Arc::new(TestDoor {
        door: Door::new(),
        peers: Semaphore::new(0),
    });
    let pipeline = AcceptPipeline {
        registry: reg_tx.clone(),
        metrics: metrics_tx,
        table: Arc::new(gsb_protocol::base_table()),
        ticket_auth: None,
        conn_inbox: 16,
        conn_out: 16,
        timeouts: Default::default(),
        conn_ids: Arc::new(ConnIdSeq::new()),
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let listener: Arc<dyn Listener> = door.clone();
    let accept = tokio::spawn(run_accept(pipeline, Arc::clone(&listener), addr));
    let (hold, rooms_released) = gsb_core::service::hold();
    drop(hold);
    let handle = ServerHandle {
        registry: reg_tx,
        rooms: crate::config::Config::default().room_template(),
        accepts: vec![accept],
        ticker: ticker_task,
        metrics,
        services: Vec::new(),
        rooms_released,
        http: None,
        listeners: vec![listener],
        addr,
        addrs: vec![addr],
        http_addr: None,
        match_results: channel(1).1,
    };
    (handle, door, go, seen)
}

/// What the registry read, once `stop` returned.
pub(super) fn read(mut seen: mpsc::UnboundedReceiver<Seen>) -> Vec<Seen> {
    let mut log = Vec::new();
    while let Ok(event) = seen.try_recv() {
        log.push(event);
    }
    log
}
