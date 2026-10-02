//! Why the connection closed reaches the policy (BACKLOG F28,
//! `docs/RECONNECT.md` §3.3). A real connection actor ends on each kind
//! of end; its `ConnClosed` carries the verdict it booked, the registry
//! relays it through the connection's dispatcher, and the room (or the
//! owning shard) asks `on_disconnect_with` with
//! `DisconnectCause::ConnectionClosedBy(verdict)` — or with the plain
//! `ConnectionClosed` when the client ended the session. Before F28 every
//! one of them arrived as `ConnectionClosed`.

use super::*;
use rejoin_rig::factory_with;
use rig::Client;

/// A base-band opcode no message is registered under (a hard violation).
const UNDEFINED_BASE_OP: u16 = 99;

/// One end of the session, as the connection's inbox receives it.
fn ended_by(cause: ServerClose) -> ConnIn {
    ConnIn::ServerClosed {
        cause,
        reason: format!("{} (test)", cause.label()),
    }
}

/// Log in to room 1 of a fresh registry (one room or two shards), end
/// the session with `end`, and return the cause the policy was asked.
async fn cause_of(sharded: bool, end: Vec<ConnIn>) -> DisconnectCause {
    let (hooks, _hooks) = mpsc::unbounded_channel();
    let (causes_tx, mut causes) = mpsc::unbounded_channel();
    let (reg, _metrics) = start(factory_with(sharded, false, hooks, Some(causes_tx)));
    create(
        &reg,
        RoomConfig {
            id: RoomId(1),
            ..Default::default()
        },
    )
    .await;
    let c = Client::login(&reg, 1, "ana").await;
    for msg in end {
        if c.inbox.send(msg).await.is_err() {
            break;
        }
    }
    let (player, cause) = tokio::time::timeout(WAIT, causes.recv())
        .await
        .expect("the policy was asked in time")
        .expect("logic alive");
    assert_eq!(player, PlayerId(1));
    cause
}

/// Each server verdict a connection books reaches the policy as the
/// verdict behind its close — single room and sharded alike.
#[tokio::test]
async fn each_server_verdict_reaches_the_policy() {
    let verdicts = [
        ServerClose::IdleTimeout,
        ServerClose::WriteStall,
        ServerClose::RelDead,
        ServerClose::OutboundDead,
    ];
    for sharded in [false, true] {
        for verdict in verdicts {
            let got = cause_of(sharded, vec![ended_by(verdict)]).await;
            assert_eq!(
                got,
                DisconnectCause::ConnectionClosedBy(verdict),
                "sharded={sharded}"
            );
            assert_eq!(got.coarse(), DisconnectCause::ConnectionClosed);
        }
    }
}

/// The verdicts the connection reaches on its own: the transport's
/// refusal of the byte stream, and the protocol-violation budget (four
/// hard violations — undefined base-band opcodes).
#[tokio::test]
async fn the_connection_s_own_verdicts_reach_the_policy() {
    for sharded in [false, true] {
        let rejected = ConnIn::StreamRejected {
            reason: "frame too large".into(),
        };
        let got = cause_of(sharded, vec![rejected]).await;
        assert_eq!(
            got,
            DisconnectCause::ConnectionClosedBy(ServerClose::StreamRejected),
            "sharded={sharded}"
        );

        let garbage = (0..4)
            .map(|_| ConnIn::Frame(FrameBody::new(UNDEFINED_BASE_OP, Vec::new())))
            .collect();
        let got = cause_of(sharded, garbage).await;
        assert_eq!(
            got,
            DisconnectCause::ConnectionClosedBy(ServerClose::ViolationBudget),
            "sharded={sharded}"
        );
    }
}

/// The client's own end carries no verdict: the plain cause, as ever.
#[tokio::test]
async fn a_client_end_reaches_the_policy_as_a_plain_close() {
    for sharded in [false, true] {
        let closed = ConnIn::Closed {
            reason: "peer left".into(),
        };
        let got = cause_of(sharded, vec![closed]).await;
        assert_eq!(got, DisconnectCause::ConnectionClosed, "sharded={sharded}");
        assert_eq!(got.verdict(), None);
    }
}
