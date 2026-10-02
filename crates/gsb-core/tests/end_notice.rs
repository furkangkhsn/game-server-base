//! The connection actor tells the door how its session ended (BACKLOG
//! B30): once, at its end, before its outbound sender drops — the stop,
//! a server verdict, the client's own end.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::{ConnIn, ConnectionActor, ServerClose, SessionEnd};
use gsb_core::id::ConnectionId;
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::RegistryMsg;
use tokio::sync::{mpsc, oneshot};

const WAIT: Duration = Duration::from_secs(5);

/// A connection actor whose session `end` ends; what it told the door.
async fn told(end: ConnIn) -> SessionEnd {
    let (registry, _registry) = channel::<RegistryMsg>(8);
    let (inbox, inbox_rx) = channel::<ConnIn>(8);
    let (out_tx, _out) = channel::<FrameBatch>(8);
    let (metrics, _metrics) = mpsc::channel::<MetricsEvent>(64);
    let (notice, door) = oneshot::channel();
    let actor = ConnectionActor::new(
        ConnectionId(30),
        SocketAddr::from(([127, 0, 0, 1], 45_030)),
        Arc::new(gsb_protocol::base_table()),
        registry,
        inbox_rx,
        out_tx,
        metrics,
        None,
    )
    .with_end_notice(notice);
    let task = tokio::spawn(actor.run());
    inbox.send(end).await.expect("actor alive");
    let end = tokio::time::timeout(WAIT, door)
        .await
        .expect("told in time")
        .expect("told");
    tokio::time::timeout(WAIT, task)
        .await
        .expect("the actor ends")
        .expect("no panic");
    end
}

#[tokio::test]
async fn the_door_learns_how_the_session_ended() {
    let kick = ConnIn::ServerClosed {
        cause: ServerClose::Kicked,
        reason: "kicked: test".into(),
    };
    let idle = ConnIn::ServerClosed {
        cause: ServerClose::IdleTimeout,
        reason: "idle".into(),
    };
    let client = ConnIn::Closed {
        reason: "peer left".into(),
    };
    let cases = [
        (ConnIn::Shutdown, SessionEnd::Stopped),
        (
            ConnIn::ShutdownOvertaking(ServerClose::Kicked),
            SessionEnd::Stopped,
        ),
        (kick, SessionEnd::Verdict(ServerClose::Kicked)),
        (idle, SessionEnd::Verdict(ServerClose::IdleTimeout)),
        (client, SessionEnd::Client),
    ];
    for (end, want) in cases {
        let what = format!("{end:?}");
        let got = told(end).await;
        assert_eq!(got, want, "{what}");
    }
}
