//! The connection actor's protocol-violation budget (the anti-amplification
//! guardrail, `gsb_core::conn` module docs): the first few violations of a
//! connection are answered with an `ERROR` frame (diagnosis), after that
//! the `reply_err` funnel is silent (amplification bounded), and a
//! weighted lifetime score at the budget closes the connection (an
//! `ERROR` code 9, then teardown).
//!
//! The actor is driven here directly over its inbox — no registry, no
//! transport: a registry mailbox with no receiver models "registry gone"
//! (every send fails, exactly the server-shutdown condition), and the
//! out channel is the actor's sole outbound, so "the channel closed" is
//! the observable connection teardown.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::id::ConnectionId;
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::RegistryMsg;
use gsb_protocol::base::{Auth, Heartbeat};
use gsb_protocol::{base_table, op};
use prost::Message;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(5);

fn frame(op: u16, payload: &[u8]) -> gsb_protocol::FrameBody {
    gsb_protocol::FrameBody::new(op, payload.to_vec())
}

/// Spawn a connection actor with a *dead* registry (no receiver: every
/// registry send fails immediately) and a drained-by-nobody metrics
/// channel. Returns the inbox sender and the out receiver.
fn spawn_actor(
    conn: u64,
) -> (
    mpsc::Sender<ConnIn>,
    mpsc::Receiver<FrameBatch>,
    JoinHandle<()>,
) {
    let (inbox_tx, inbox) = channel::<ConnIn>(128);
    let (out_tx, out_rx) = channel::<FrameBatch>(16);
    let (reg_tx, _reg_rx) = channel::<RegistryMsg>(16);
    let (metrics_tx, _metrics_rx) = mpsc::channel::<MetricsEvent>(16);
    let actor = ConnectionActor::new(
        ConnectionId(conn),
        SocketAddr::from(([127, 0, 0, 1], 40_000u16 + conn as u16)),
        Arc::new(base_table()),
        reg_tx,
        inbox,
        out_tx,
        metrics_tx,
        None, // local auth (no ticket hook): these tests cover the
              // pre-hook protocol-violation budget, unchanged by the hook.
    );
    let h = tokio::spawn(actor.run());
    (inbox_tx, out_rx, h)
}

async fn read_err(out: &mut mpsc::Receiver<FrameBatch>) -> Option<(u32, String)> {
    let batch = tokio::time::timeout(WAIT, out.recv())
        .await
        .ok()?
        .expect("out channel closed");
    for f in batch {
        if f.op != op::base::ERROR {
            continue;
        }
        let e: gsb_protocol::base::Error =
            gsb_protocol::base::Error::decode(f.payload.as_ref()).expect("Error decode");
        return Some((e.code, e.message));
    }
    panic!("batch without an ERROR frame");
}

/// Read with a short window: `None` = silence (the budget's silent phase).
async fn read_err_quiet(out: &mut mpsc::Receiver<FrameBatch>) -> Option<(u32, String)> {
    let batch = tokio::time::timeout(Duration::from_millis(400), out.recv())
        .await
        .ok()?
        .expect("out channel closed");
    for f in batch {
        if f.op != op::base::ERROR {
            continue;
        }
        let e: gsb_protocol::base::Error =
            gsb_protocol::base::Error::decode(f.payload.as_ref()).expect("Error decode");
        return Some((e.code, e.message));
    }
    panic!("batch without an ERROR frame");
}

#[tokio::test]
async fn hard_violations_answer_three_then_silence_then_close() {
    let (in_tx, mut out, handle) = spawn_actor(1);
    // Six unknown base-band opcodes: hard violations (weight 4 each).
    // Budget 16 is exhausted on the FOURTH (4×4=16): answers on 1-3,
    // close on 4; 5-6 must never be answered (the actor is gone).
    for i in 0..6 {
        in_tx
            .send(ConnIn::Frame(frame(42 + i, &[])))
            .await
            .expect("inbox open");
    }
    let mut codes = Vec::new();
    for _ in 0..4 {
        let (code, msg) = read_err(&mut out).await.expect("answered");
        if code == 9 {
            assert!(
                msg.contains("violation"),
                "the budget close must name the violation budget: {msg}"
            );
        } else {
            assert_eq!(code, 1, "unknown opcode answers with code 1");
        }
        codes.push(code);
    }
    assert_eq!(codes, vec![1, 1, 1, 9]);
    // The connection is torn down: the actor dropped its out sender.
    assert!(
        tokio::time::timeout(WAIT, out.recv())
            .await
            .unwrap()
            .is_none(),
        "no frames after the budget close"
    );
    handle.await.expect("actor exits");
}

#[tokio::test]
async fn race_class_strays_do_not_exhaust_the_budget() {
    let (in_tx, mut out, handle) = spawn_actor(2);
    // 15 stray game-band frames while not in a room (the legitimate
    // leave/race pattern, weight 1 each): score 15 < 16 — NO close.
    for _ in 0..15 {
        in_tx
            .send(ConnIn::Frame(frame(op::GAME_BAND_START, &[])))
            .await
            .expect("inbox open");
    }
    // The first three are answered (diagnosis)…
    for _ in 0..3 {
        let (code, _) = read_err(&mut out).await.expect("answered");
        assert_eq!(code, 6, "not-in-a-room answers with code 6");
    }
    // …the rest are silent, and the connection is still alive:
    assert!(
        read_err_quiet(&mut out).await.is_none(),
        "violations 4..15 must be silent (score 15 < budget 16)"
    );
    // A heartbeat is still answered (the connection is not closed):
    let hb = Heartbeat { tick: 1 }.encode_to_vec();
    in_tx
        .send(ConnIn::Frame(frame(op::base::HEARTBEAT, &hb)))
        .await
        .expect("inbox open");
    let batch = tokio::time::timeout(WAIT, out.recv())
        .await
        .expect("timed out")
        .expect("out open");
    assert!(
        batch.iter().any(|f| f.op == op::base::HEARTBEAT_ACK),
        "the connection must still answer a heartbeat"
    );
    // The 16th stray finally exhausts the budget: close.
    in_tx
        .send(ConnIn::Frame(frame(op::GAME_BAND_START, &[])))
        .await
        .expect("inbox open");
    let (code, msg) = read_err(&mut out).await.expect("close notice");
    assert_eq!(code, 9);
    assert!(msg.contains("violation"));
    assert!(
        tokio::time::timeout(WAIT, out.recv())
            .await
            .unwrap()
            .is_none(),
        "teardown after the close"
    );
    handle.await.expect("actor exits");
}

#[tokio::test]
async fn server_side_conditions_are_never_counted() {
    let (in_tx, mut out, handle) = spawn_actor(3);
    // Authenticate first (the JOIN path checks auth before touching the
    // registry), then with a dead registry every JOIN fails server-side
    // ("registry gone", class None): always answered, never budgeted.
    // Ten of them must not come close to the budget (10 answered
    // ERROR 7s, no 9).
    let auth = Auth {
        name: "neo".into(),
        ticket: vec![],
    }
    .encode_to_vec();
    in_tx
        .send(ConnIn::Frame(frame(op::base::AUTH_REQ, &auth)))
        .await
        .expect("inbox open");
    let batch = tokio::time::timeout(WAIT, out.recv())
        .await
        .expect("timed out")
        .expect("out open");
    assert!(batch.iter().any(|f| f.op == op::base::AUTH_RESULT));
    let join = gsb_protocol::base::JoinRoom { room_id: 1 }.encode_to_vec();
    for _ in 0..10 {
        in_tx
            .send(ConnIn::Frame(frame(op::base::JOIN_ROOM_REQ, &join)))
            .await
            .expect("inbox open");
    }
    for _ in 0..10 {
        let (code, msg) = read_err(&mut out).await.expect("answered");
        assert_eq!(code, 7, "registry-gone answers with code 7: {msg}");
    }
    // The connection survived all ten: a heartbeat is still answered.
    let hb = Heartbeat { tick: 1 }.encode_to_vec();
    in_tx
        .send(ConnIn::Frame(frame(op::base::HEARTBEAT, &hb)))
        .await
        .expect("inbox open");
    let batch = tokio::time::timeout(WAIT, out.recv())
        .await
        .expect("timed out")
        .expect("out open");
    assert!(
        batch.iter().any(|f| f.op == op::base::HEARTBEAT_ACK),
        "server-side errors must not close the connection"
    );
    // Shut it down cleanly (the test's own teardown, not the budget's).
    in_tx.send(ConnIn::Shutdown).await.expect("inbox open");
    handle.await.expect("actor exits on Shutdown");
}

#[tokio::test]
async fn double_auth_is_a_hard_violation() {
    let (in_tx, mut out, handle) = spawn_actor(4);
    let auth = Auth {
        name: "neo".into(),
        ticket: vec![],
    }
    .encode_to_vec();
    in_tx
        .send(ConnIn::Frame(frame(op::base::AUTH_REQ, &auth)))
        .await
        .expect("inbox open");
    let batch = tokio::time::timeout(WAIT, out.recv())
        .await
        .expect("timed out")
        .expect("out open");
    assert!(
        batch.iter().any(|f| f.op == op::base::AUTH_RESULT),
        "first auth succeeds"
    );
    // Re-authenticating on the same connection is a hard violation
    // (weight 4): the budget exhausts on the fourth.
    for i in 0..4 {
        in_tx
            .send(ConnIn::Frame(frame(op::base::AUTH_REQ, &auth)))
            .await
            .expect("inbox open");
        let (code, msg) = read_err(&mut out).await.expect("answered");
        if i < 3 {
            assert_eq!(code, 3, "auth violations answer with code 3");
        } else {
            assert_eq!(code, 9, "the fourth closes");
            assert!(msg.contains("violation"));
        }
    }
    assert!(
        tokio::time::timeout(WAIT, out.recv())
            .await
            .unwrap()
            .is_none(),
        "teardown after the close"
    );
    handle.await.expect("actor exits");
}
