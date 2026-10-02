//! The actor's half of a migration (BACKLOG B113): its `peer` follows the
//! transport's notice, and the registry hears the new source — in order
//! with the session's frames, which share the inbox.

use std::net::SocketAddr;
use std::sync::Arc;

use gsb_protocol::{FrameBody, MessageTable};
use tokio::sync::mpsc;

use crate::channel::channel;
use crate::conn::{ConnIn, ConnectionActor};
use crate::id::ConnectionId;
use crate::registry::RegistryMsg;
use crate::source::Source;

#[tokio::test]
async fn a_peer_change_moves_the_peer_and_tells_the_registry() {
    let (in_tx, inbox) = channel::<ConnIn>(8);
    let (out_tx, _out) = channel(8);
    let (reg_tx, mut reg_rx) = channel::<RegistryMsg>(8);
    let (metrics, _m) = mpsc::channel(8);
    let first = SocketAddr::from(([10, 0, 0, 1], 4000));
    let mut actor = ConnectionActor::new(
        ConnectionId(7),
        first,
        Arc::new(MessageTable::default()),
        reg_tx,
        inbox,
        out_tx,
        metrics,
        None,
    );
    let moved = SocketAddr::from(([10, 0, 0, 2], 5000));
    actor.on_peer_changed(moved).await;
    assert_eq!(actor.peer, moved);
    match reg_rx.try_recv() {
        Ok(RegistryMsg::ConnPeerChanged { conn, source }) => {
            assert_eq!(conn, ConnectionId(7));
            assert_eq!(source, Source::of(moved.ip()));
        }
        other => panic!("expected the registry's notice, got {other:?}"),
    }

    // Through the run loop: the notice is handled where it sits, before
    // the frames behind it; the session goes on.
    let back = SocketAddr::from(([10, 0, 0, 1], 4001));
    in_tx
        .try_send(ConnIn::PeerChanged { peer: back })
        .expect("room");
    in_tx
        .try_send(ConnIn::Frame(FrameBody::new(9999, bytes::Bytes::new())))
        .expect("room");
    drop(in_tx);
    actor.run().await;
    match reg_rx.try_recv() {
        Ok(RegistryMsg::ConnPeerChanged { source, .. }) => {
            assert_eq!(source, Source::of(back.ip()))
        }
        other => panic!("expected the registry's notice, got {other:?}"),
    }
    assert!(
        matches!(reg_rx.try_recv(), Ok(RegistryMsg::ConnClosed { .. })),
        "then the session's end"
    );
}
