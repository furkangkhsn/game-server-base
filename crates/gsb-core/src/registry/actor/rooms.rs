//! Installing a room (single or sharded) and watching it die: the
//! generation stamp is what makes a late death report from an earlier
//! incarnation a one-integer no-op.

use std::fmt::Debug;
use std::hash::Hash;

use tracing::debug;

use crate::channel::{Inbox, Mailbox, channel};
use crate::room::{RoomActor, RoomConfig};
use crate::shard::{ShardActor, ShardMsg};
use crate::registry::*;

use crate::registry::actor::Registry;

mod watch;

impl<W, G, St, Sp> Registry<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip payload's trait bounds (`GameLogic::Strip`) — the
    // registry never inspects payloads, but both actor shapes it spawns
    // require them.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Install a freshly built room: spawn its actor task(s) plus ONE death
    /// watcher per task, and put this incarnation's table entry. This is
    /// THE single creation path — `CreateRoom` and a panic rebuild (see
    /// [`RegistryMsg::RoomDied`]) both wire their actors through here, so
    /// the two can never drift apart.
    ///
    /// The factory has already run by the time this is called, inside the
    /// registry loop (as on every create): room construction is synchronous
    /// game code, and a panicking FACTORY is out of scope for supervision —
    /// it would kill the registry itself, exactly as it does today. The
    /// watch covers the spawned tick-loop tasks, whose panics are contained
    /// by tokio's task boundary.
    pub(super) fn install_room(
        &mut self,
        config: RoomConfig,
        built: BuiltRoom<W, G, St, Sp>,
        run_every: u64,
    ) {
        let id = config.id;
        // This incarnation's generation (0 = first create for this id,
        // +1 per rebuild/re-create): copied into every death watcher of
        // this install so a late report from an earlier incarnation can be
        // rejected at report time with one integer comparison (see
        // `RegistryMsg::RoomDied`) — no cancellation plumbing anywhere.
        let generation = {
            let slot = self.room_gen.entry(id).or_insert(0);
            let current = *slot;
            *slot = current + 1;
            current
        };
        match built {
            BuiltRoom::Single { world, logic } => {
                let (control_tx, control_rx) = channel(config.control_capacity);
                let handle = tokio::spawn(
                    RoomActor::new(
                        config.clone(),
                        world,
                        logic,
                        self.ticker.subscribe(),
                        control_rx,
                        run_every,
                        self.metrics.clone(),
                        self.result_sink.clone(),
                    )
                    // The park-expiry report path (`ParkExpired`): the
                    // room is the only actor that sees a hold end, and
                    // this registry is holding the row it ends.
                    .with_registry(self.self_mailbox.clone())
                    .run(),
                );
                Self::spawn_room_watcher(
                    id,
                    None,
                    generation,
                    handle,
                    self.self_mailbox.clone(),
                );
                self.rooms.insert(
                    id,
                    RoomEntry {
                        control: Some(control_tx),
                        shards: None,
                        config,
                        generation,
                    },
                );
            }
            BuiltRoom::Sharded { shards, home_shard } => {
                let n = shards.len();
                debug_assert!(n >= 1, "a sharded room needs >= 1 shard");
                // One channel per shard: the registry keeps the original
                // sender, and every neighbor of the shard holds a CLONE of
                // it (tokio mpsc: many senders, one receiver). So each
                // shard's mailbox carries both the registry's control
                // messages and its neighbors' protocol messages
                // (Migrate/Border) — the shard actor's single `try_recv`
                // drain handles all of them.
                //
                // Pass 1 — one channel per shard (the registry keeps the
                // original sender; every neighbor holds a clone — tokio
                // mpsc: many senders, one receiver), and each actor's
                // `neighbors` vec (`txs[a][b]` = the sender shard a uses to
                // reach shard b, indexed by the receiver's index — the
                // actor indexes it that way; non-neighbor slots are
                // dummies, closed senders that are never sent to).
                let mut rxs: Vec<Inbox<ShardMsg<St, Sp>>> = Vec::with_capacity(n);
                let mut reg_txs = Vec::with_capacity(n);
                let (dummy_tx, _dummy_rx) = channel::<ShardMsg<St, Sp>>(1);
                for _ in 0..n {
                    let (tx, rx) = channel(config.control_capacity);
                    reg_txs.push(tx.clone());
                    rxs.push(rx);
                }
                let mut txs: Vec<Vec<Mailbox<ShardMsg<St, Sp>>>> =
                    Vec::with_capacity(n);
                for a in 0..n {
                    let mut row = Vec::with_capacity(n);
                    for (b, tx_b) in reg_txs.iter().enumerate() {
                        // neighbors() is game knowledge (the grid
                        // topology); it is read after the factory built
                        // the logics.
                        let is_neighbor = shards
                            .get(a)
                            .map(|(_, l)| l.neighbors().contains(&b))
                            .unwrap_or(false);
                        row.push(if is_neighbor {
                            tx_b.clone()
                        } else {
                            dummy_tx.clone()
                        });
                    }
                    txs.push(row);
                }
                // Pass 2 — spawn the shard actors (indices = the vec order;
                // the sample id / neighbor slots rely on it) and one death
                // watcher per shard: ANY dead shard breaks the whole
                // logical room (its neighbors hold senders into its closed
                // mailbox, so cross-shard migration can never complete
                // again), which is exactly what the watcher will report.
                for (i, (world, logic)) in shards.into_iter().enumerate() {
                    // `rxs` was built in shard order (pass 1), so popping
                    // the front pairs each shard with its own receiver.
                    let rx = rxs.remove(0);
                    let handle = tokio::spawn(
                        ShardActor::new(
                            config.clone(),
                            i,
                            world,
                            logic,
                            self.ticker.subscribe(),
                            rx,
                            txs[i].clone(),
                            run_every,
                            self.metrics.clone(),
                            // Faz 3: every shard of a logical room shares
                            // this room's match-result sink; each reports
                            // its own final state at ITS teardown (one
                            // payload per shard — see `crate::shard`).
                            self.result_sink.clone(),
                        )
                        // The park-expiry report path (`ParkExpired`) —
                        // as for a single room, plus the ShardGroup member
                        // slot the detached row holds on the grid.
                        .with_registry(self.self_mailbox.clone())
                        .run(),
                    );
                    Self::spawn_room_watcher(
                        id,
                        Some(i),
                        generation,
                        handle,
                        self.self_mailbox.clone(),
                    );
                }
                self.rooms.insert(
                    id,
                    RoomEntry {
                        control: None,
                        shards: Some(ShardGroup {
                            mailboxes: reg_txs,
                            home: home_shard,
                            cap: config.max_players.map(|c| c as u64),
                            members: 0,
                            pending: 0,
                        }),
                        config,
                        generation,
                    },
                );
                debug!(room = %id, shards = n, "sharded room created");
            }
        }
    }
}
