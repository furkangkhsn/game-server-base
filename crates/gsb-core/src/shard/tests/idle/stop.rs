//! The shard's half of the ceiling's verdicts at the server's stop
//! (BACKLOG F56): the room actor's rule, mirrored — a verdict the
//! stopped registry's closed mailbox refused, and one still queued when
//! the shard stopped, are lost verdicts, sent once as the shard stops.

use super::*;
use crate::conn::ServerClose;
use crate::registry::RegistryMsg;
use crate::room::AfkAction;

/// `Disconnect` with the registry already stopped: each expired member's
/// close is refused and counted by its reason, and so is each despawn
/// report (refused by a later tick's flush, or still queued at the
/// stop — either way once).
#[tokio::test]
async fn a_shards_refused_and_queued_verdicts_are_lost_verdicts() {
    let cfg = RoomConfig {
        afk_action: AfkAction::Disconnect,
        ..config(Some(5))
    };
    let (a, _obs, mut disc) = rig_with(cfg, Detach::Despawn);
    let (tx, reg) = channel::<RegistryMsg>(64);
    drop(reg);
    let mut a = a.with_registry(tx);
    let _o1 = join(&mut a, ConnectionId(1), "ana");
    let _o2 = join(&mut a, ConnectionId(2), "bora");
    let t0 = Instant::now();
    for k in 1..=4u64 {
        a.step_phases(&TickInfo {
            tick: k,
            at: t0 + Duration::from_secs(10 + k),
        });
    }
    assert_eq!(drain(&mut disc).len(), 2, "the policy ran for both");
    let (metrics, mut events) = mpsc::channel(8);
    a.metrics = metrics;
    a.finish();
    let lost: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|ev| match ev {
            MetricsEvent::VerdictsLost(v) => Some(v),
            _ => None,
        })
        .collect();
    let [v] = lost[..] else {
        panic!("one VerdictsLost: {lost:?}");
    };
    assert_eq!(v.closes.get(ServerClose::IdleInput), 2);
    assert_eq!(v.closes.total(), 2);
    assert_eq!((v.leaves, v.detach_despawns), (0, 2));
}
