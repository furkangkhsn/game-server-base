//! The per-connection input rate limit (BACKLOG E1, docs/SECURITY.md
//! "post-auth input volume"), end to end: real connection actors over a
//! live registry and room, on tokio's PAUSED clock — the gate reads the
//! tick clock (`gsb_core::ticker::now`), so a virtual second is a second
//! of refill and every count below is exact.
//!
//! - a flooder above the room's rate gets its burst plus the refill —
//!   never more — and the rest is dropped at its own actor (counted, not
//!   a violation, never queued: the room's pulls stay inside the limit);
//! - an honest client AT the rate is untouched;
//! - with no limit (the default) the same flood takes today's path: the
//!   action channel fills and the sender's own input drops there.
//!
//! `gate.rs` pins the actor-side rules against a stand-in registry.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::metrics::{ConnSample, MetricsEvent};
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory};
use gsb_core::room::{Action, Admission, GameLogic, InputRate, RoomConfig, RoomLogic, TickCtx};
use gsb_core::ticker::Ticker;
use gsb_protocol::base::Heartbeat;
use gsb_protocol::{FrameBody, MessageTable, base, base_table, op};
use prost::Message;
use tokio::sync::mpsc;

#[path = "input_rate/gate.rs"]
mod gate;
#[path = "input_rate/rig.rs"]
mod rig;

use rig::{Client, registry};

const WAIT: Duration = Duration::from_secs(5);

/// The one registered game-band opcode (a real server's table is always
/// the base plus the game's messages — see `violation.rs`).
const GAME_OP: u16 = op::GAME_BAND_START;

fn table() -> MessageTable {
    let mut t = base_table();
    t.reg::<Heartbeat>(GAME_OP);
    t
}

fn game_frame() -> FrameBody {
    FrameBody::new(GAME_OP, Heartbeat { tick: 1 }.encode_to_vec())
}

/// Every ingest reported so far, split by player.
fn ingested(rx: &mut mpsc::UnboundedReceiver<(u64, u64)>, player: u64) -> Vec<u64> {
    let mut ticks = Vec::new();
    while let Ok((p, t)) = rx.try_recv() {
        if p == player {
            ticks.push(t);
        }
    }
    ticks
}

/// The most actions `ticks` puts in one tick.
fn per_tick_max(ticks: &[u64]) -> usize {
    let mut max = 0;
    for t in ticks {
        max = max.max(ticks.iter().filter(|x| *x == t).count());
    }
    max
}

/// 10/s, burst 5: two floods of 1000 a virtual second apart pass 5 + 5
/// (the refill is capped at the burst) — an honest client sending at
/// exactly 10/s alongside passes all of its 20. The flooder's excess is
/// counted as rate-limited (not dropped from its channel, not a
/// violation), it is still connected, and the room never pulled more
/// than the burst from it in one tick.
#[tokio::test(start_paused = true)]
async fn a_flooder_is_limited_and_an_honest_client_is_not() {
    let (reg, mut ingests) = registry(InputRate::new(10, 5)).await;
    let mut flooder = Client::login(&reg, 1).await;
    let honest = Client::login(&reg, 2).await;
    let honest = tokio::spawn(async move {
        for _ in 0..20 {
            honest.send(game_frame()).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        honest
    });
    for _ in 0..2 {
        for _ in 0..1000 {
            flooder.send(game_frame()).await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let honest = honest.await.expect("the honest client task");
    // Still connected, and never told off: a heartbeat is answered.
    flooder
        .send(FrameBody::new(
            op::base::HEARTBEAT,
            Heartbeat { tick: 9 }.encode_to_vec(),
        ))
        .await;
    flooder.expect(op::base::HEARTBEAT_ACK).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut all = Vec::new();
    while let Ok(x) = ingests.try_recv() {
        all.push(x);
    }
    let of = |p: u64| {
        all.iter()
            .filter(|(q, _)| *q == p)
            .map(|(_, t)| *t)
            .collect::<Vec<_>>()
    };
    let (f, h) = (of(1), of(2));
    assert_eq!(f.len(), 10, "the burst, then the burst again");
    assert!(per_tick_max(&f) <= 5, "the room pulled within the limit");
    assert_eq!(h.len(), 20, "the honest client is untouched");

    let fs = flooder.close().await;
    assert_eq!(fs.input_rate_limited, 1990, "every refused input counted");
    assert_eq!(fs.violations, 0, "an over-rate client is not a violator");
    assert_eq!(fs.actions_dropped, 0, "refused, never queued");
    let hs = honest.close().await;
    assert_eq!((hs.input_rate_limited, hs.violations), (0, 0));
}

/// No limit (the default): the same flood reaches the room's channel,
/// which fills — the sender's own input drops there, as it always did,
/// and nothing is counted as rate-limited.
#[tokio::test(start_paused = true)]
async fn without_a_limit_a_flood_takes_todays_path() {
    let (reg, mut ingests) = registry(None).await;
    let flooder = Client::login(&reg, 1).await;
    for _ in 0..1000 {
        flooder.send(game_frame()).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let f = ingested(&mut ingests, 1);
    assert!(f.len() > 10, "unlimited: {} ingested", f.len());
    assert!(
        per_tick_max(&f) <= 16,
        "the per-tick pull budget still binds"
    );
    let fs = flooder.close().await;
    assert_eq!(fs.input_rate_limited, 0);
    assert!(fs.actions_dropped > 0, "the channel filled and dropped");
    assert_eq!(fs.actions_dropped + f.len() as u64, 1000);
}
