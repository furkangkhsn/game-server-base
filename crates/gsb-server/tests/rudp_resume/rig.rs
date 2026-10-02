//! The server side of the suite: a one-room demo server on one door,
//! its metric reports folded into the latest word of every counter the
//! resume path moves, and condition waits over that word.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gsb_core::conn::ServerClose;
use gsb_core::id::RoomId;
use gsb_core::metrics::{MetricReport, RoomReport, ServerCloses};
use gsb_core::registry::RoomStatus;
use tokio::sync::mpsc;

/// The hang guard of every wait in the suite: never the assertion.
pub const GUARD: Duration = Duration::from_secs(30);

/// The door a scenario runs through. rUDP is the subject; TCP runs the
/// same flow beside it, so a later rUDP round (B3: a session that
/// survives an address change) can state its own expectation next to
/// the stream door's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Door {
    Tcp,
    Udp,
    /// rUDP with `udp_migration` on (B3): a session survives its
    /// client's address change.
    Migrating,
}

impl Door {
    /// How the server learns that a silently dropped client is gone: on
    /// TCP the socket's EOF (a client-side end, never a server close);
    /// on rUDP — no FIN — the demux's idle sweep, a server close booked
    /// as `idle_timeout`.
    pub fn drop_close(self) -> Option<ServerClose> {
        match self {
            Door::Tcp => None,
            Door::Udp | Door::Migrating => Some(ServerClose::IdleTimeout),
        }
    }
}

/// The room's shape: one actor, or the sharded grid (whose resume is a
/// broadcast to every shard, RECONNECT §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Single,
    Sharded,
}

/// The idle window: the only way the server can learn that a silent
/// rUDP client is gone. Long enough that a heartbeating client under a
/// starved scheduler never trips it.
pub const IDLE_SECS: f64 = 3.0;

/// A one-room demo server on `door` with the park grace `grace_secs`.
pub fn config(door: Door, shape: Shape, grace_secs: f64) -> gsb_server::Config {
    gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        transport: match door {
            Door::Tcp => gsb_server::TransportKind::Tcp,
            Door::Udp | Door::Migrating => gsb_server::TransportKind::Udp,
        },
        udp_migration: door == Door::Migrating,
        topology: Some(match shape {
            Shape::Single => gsb_server::Topology::Single,
            Shape::Sharded => gsb_server::Topology::Sharded,
        }),
        idle_timeout_secs: IDLE_SECS,
        disconnect_grace_secs: grace_secs,
        ..Default::default()
    }
}

/// Room 1's counters, summed over its rows (its own, or every shard's:
/// a shard reports as `room << 16 | index`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoomCounts {
    pub joins: u64,
    pub leaves: u64,
    pub resumes: u64,
    pub detached: u32,
    pub stale: u64,
    pub expired_ai: u64,
    pub expired_despawn: u64,
}

/// The registry's connection and membership counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegCounts {
    pub opens: u64,
    pub closes: u64,
    pub conns: u32,
    pub joins: u64,
    pub leaves: u64,
}

/// The latest word of everything the resume path moves.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub room: RoomCounts,
    pub reg: RegCounts,
    pub closes: ServerCloses,
    /// rUDP sessions moved to a new client address (B3).
    pub migrations: u64,
    /// Reports folded so far (a settle reads a few more).
    pub reports: u64,
}

impl Seen {
    /// Exactly one server close under each reason in `want` (repeats
    /// allowed), and nothing under any other reason.
    pub fn assert_closes(&self, want: &[ServerClose]) {
        for (reason, n) in self.closes.iter() {
            let expect = want.iter().filter(|r| **r == reason).count() as u64;
            assert_eq!(
                n,
                expect,
                "server closes `{}`: {}",
                reason.label(),
                self.closes.nonzero_summary()
            );
        }
    }
}

/// A started server and its report stream.
pub struct Rig {
    pub handle: gsb_server::ServerHandle,
    reports: mpsc::UnboundedReceiver<MetricReport>,
    rooms: HashMap<u64, RoomReport>,
    pub seen: Seen,
}

impl Rig {
    pub async fn start(cfg: gsb_server::Config) -> Self {
        let (tx, reports) = mpsc::unbounded_channel();
        let handle = gsb_server::start_server_metrics(cfg, tx)
            .await
            .expect("server starts");
        Self {
            handle,
            reports,
            rooms: HashMap::new(),
            seen: Seen::default(),
        }
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.handle.addr
    }

    /// Fold the next report into [`Self::seen`].
    async fn next(&mut self, what: &str, deadline: Instant) {
        let left = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out: {what}; last seen {:?}", self.seen));
        let r = match tokio::time::timeout(left, self.reports.recv()).await {
            Ok(Some(r)) => r,
            Ok(None) => panic!("the metrics channel closed: {what}"),
            Err(_) => panic!("timed out: {what}; last seen {:?}", self.seen),
        };
        for room in r.rooms {
            self.rooms.insert(room.room.0, room);
        }
        let mut sum = RoomCounts::default();
        for (id, r) in &self.rooms {
            if *id == 1 || id >> 16 == 1 {
                sum.joins += r.joins;
                sum.leaves += r.leaves;
                sum.resumes += r.resumes;
                sum.detached += r.detached;
                sum.stale += r.resume_rejected_stale;
                sum.expired_ai += r.detach_expired_ai;
                sum.expired_despawn += r.detach_expired_despawn;
            }
        }
        self.seen.room = sum;
        if let Some(g) = r.registry {
            self.seen.reg = RegCounts {
                opens: g.opens,
                closes: g.closes,
                conns: g.conns,
                joins: g.joins,
                leaves: g.leaves,
            };
        }
        self.seen.closes = r.net.server_closes;
        self.seen.migrations = r.transport.udp_migrations;
        self.seen.reports += 1;
    }

    /// Fold reports until `done` holds; the guard only bounds a hang.
    pub async fn until(&mut self, what: &str, done: impl Fn(&Seen) -> bool) -> Seen {
        let deadline = Instant::now() + GUARD;
        while !done(&self.seen) {
            self.next(what, deadline).await;
        }
        self.seen.clone()
    }

    /// Fold `n` more reports (each carries a fresher room sample), so a
    /// late move of any counter would still be seen.
    pub async fn settle(&mut self, n: u64) -> Seen {
        let deadline = Instant::now() + GUARD;
        for _ in 0..n {
            self.next("settling", deadline).await;
        }
        self.seen.clone()
    }

    /// How many metric rows room 1 reports under: its own (one actor),
    /// or one per shard (the grid) — proof of which shape ran.
    pub fn rows(&self) -> usize {
        self.rooms
            .keys()
            .filter(|id| **id == 1 || *id >> 16 == 1)
            .count()
    }

    /// The registry's member count of room 1 (the cap's own source).
    pub async fn members(&self) -> u32 {
        match self.handle.room_status(RoomId(1)).await.expect("registry") {
            RoomStatus::Running { members } => members,
            other => panic!("room 1 is {other:?}"),
        }
    }

    pub async fn stop(self) {
        self.handle.stop().await;
    }
}
