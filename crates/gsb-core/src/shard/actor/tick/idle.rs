//! Phase 0d — the shard's input-idle ceiling sweep and the disconnect
//! path both it and `ShardMsg::Detach` run (the room actor's
//! `phase_idle_sweep` / `detach_player`, mirrored).

use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;

use tracing::{debug, warn};

use crate::id::{ConnectionId, PlayerId};
use crate::room::Detach;

use crate::shard::actor::ShardActor;

impl<W, G, St, Sp> ShardActor<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip rides every exchange and view; the bounds mirror what
    // the delta protocol does with it.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// THE disconnect path on a shard: ask the policy, run its arm. Two
    /// callers, one decision point — `ShardMsg::Detach` (a dead
    /// transport) and the input-idle ceiling (a live transport that
    /// stopped playing). The room actor's `detach_player`, mirrored;
    /// callers own the guards.
    pub(crate) fn detach_player(&mut self, player: PlayerId, conn: ConnectionId, identity: &str) {
        match self.logic.on_disconnect(&mut self.world, player, identity) {
            Detach::Despawn => {
                // Today's close semantics, plus the registry report — the
                // room actor's arm mirrored (see it for the full
                // argument). A declined park never starts a hold, so no
                // phase-0c sweep can ever end it; on the grid the
                // unreported row also keeps a `ShardGroup` member slot,
                // the only whole-room capacity view there is. Flushed in
                // this same tick's phase 0c.
                if self.registry.is_some() {
                    self.despawn_reports.push(conn);
                }
                self.despawn_conn(player, false);
            }
            Detach::Hold { grace, to } => {
                // Park: keep row (stable key)/entity/slot and the binding
                // row; core owns the clock (§14.4). The dead session's
                // in-flight requests die with it (RECONNECT §11).
                let rc = self.conns.get_mut(&player).expect("guarded by caller");
                rc.detached = true;
                rc.expire_to = to;
                rc.detach_deadline = grace.map(|g| Instant::now() + g);
                // OFF the input-idle clock while parked (the room actor's
                // rule): the row has no live input source, so the ceiling
                // must not fire on top of a hold. Resume restarts it.
                self.idle.stop(player);
                self.drop_conn_request_state(conn);
                debug!(
                    room = %self.config.id,
                    shard = self.index,
                    %conn,
                    %player,
                    ?grace,
                    ?to,
                    "player detached on shard (entity parked)"
                );
            }
        }
    }

    /// Tick phase 0d — the input-idle ceiling
    /// ([`crate::room::RoomConfig::max_idle_input_secs`], default OFF).
    /// The room actor's phase, mirrored: free when unset (one `Option`
    /// test per step), a constant-cost bounded rotation when set, and on
    /// expiry the SAME disconnect path a dead transport takes.
    pub(super) fn phase_idle_sweep(&mut self, now: Instant) {
        let Some(limit) = self.config.max_idle_input() else {
            return;
        };
        let mut due: Vec<PlayerId> = Vec::new();
        self.idle.sweep_due(now, limit, &mut due);
        for player in due {
            let Some((conn, identity)) = self
                .conns
                .get(&player)
                .map(|rc| (rc.conn, rc.identity.clone()))
            else {
                self.idle.stop(player);
                continue;
            };
            if self.idle_ceiling_warns == 0 {
                self.idle_ceiling_warns += 1;
                warn!(
                    room = %self.config.id,
                    shard = self.index,
                    %player,
                    %conn,
                    idle_limit_secs = limit.as_secs(),
                    "input-idle ceiling reached (max_idle_input_secs): the \
                     member is handed to the ordinary disconnect policy \
                     (on_disconnect decides park/AI-handover/despawn). \
                     This warning is emitted once per shard."
                );
            }
            self.idle.stop(player);
            self.detach_player(player, conn, &identity);
        }
    }
}
