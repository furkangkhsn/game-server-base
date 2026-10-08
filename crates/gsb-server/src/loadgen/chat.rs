//! The chat/presence scenario's server half (`--chat-rate`, `--friends`;
//! `docs/EVENTS.md` §12, round e6): the demo game, hosted with two
//! event-producing services beside it and a probe of the registry —
//! the router's load and what it costs the control plane.
//!
//! - **chat** — a publisher (`chat`) that pushes `--chat-rate` lines a
//!   second, server-wide, round-robin over `--chat-channels` channels;
//!   each line goes to every member of its channel with ONE `push_many`
//!   (client `id` is in channel `id % C`). The game's chat service in
//!   miniature: who is in a channel is the game's (here: arithmetic),
//!   the fan-out is the engine's. A line carries its send instant, so a
//!   client measures push → receive ([`ChatLine::sent_at_us`]; the
//!   in-process clients share the clock).
//! - **friends** — the demo's friends service (`gsb_demo::friends`) over
//!   a ring: client `i` befriends `i+1 … i+F`. Its presence source is
//!   the engine's events (the registry derives them), its truth the
//!   registry's presence query, its voice a publisher (`friends`) — so a
//!   join storm is also a presence storm.
//! - **probe** — the registry's mailbox pressure, measured from outside:
//!   every [`PROBE_EVERY`] one presence query (`Presence::of`, one
//!   identity) — it waits in the registry's mailbox behind the joins and
//!   the pushes, and is answered in place, so its round trip is the
//!   mailbox's queueing delay. A refused query (`Busy`: the mailbox
//!   full) is counted.
//!
//! In-process only (the probe's numbers and the chat's member list are
//! this process's): `--addr`, `--serve` and `--orchestrate` refuse the
//! flags.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gsb_core::calls::Identity;
use gsb_core::events::{ClientEvent, Dest, Event, Interest, Presence, PresenceRefused};
use gsb_protocol::MessageTable;
use gsb_server::games::demo::DemoModule;
use gsb_server::{Config, GameModule, RegistryParts, RegistryTask, ServerError};
use tokio::sync::mpsc;

/// The chat line (a loadgen-only event: op [`ChatLine::OP`], outside the
/// demo's `.proto` numbering so the two can never meet).
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ChatLine {
    /// The channel (`id % channels`).
    #[prost(uint32, tag = "1")]
    pub(crate) channel: u32,
    /// The line's number, server-wide, from 1.
    #[prost(uint64, tag = "2")]
    pub(crate) line: u64,
    /// When the publisher pushed it: µs since the UNIX epoch.
    #[prost(uint64, tag = "3")]
    pub(crate) sent_at_us: u64,
    /// The text: [`TEXT_BYTES`] of filler (a chat line's size).
    #[prost(bytes = "vec", tag = "4")]
    pub(crate) text: Vec<u8>,
}

impl Event for ChatLine {
    const OP: u16 = 1990;
    const TO: Dest = Dest::Client;
}
impl ClientEvent for ChatLine {}

/// A chat line's text size.
pub(crate) const TEXT_BYTES: usize = 64;

/// How often the probe asks.
pub(crate) const PROBE_EVERY: Duration = Duration::from_millis(10);

/// The chatter's step: lines due are pushed at most this often.
const CHAT_STEP: Duration = Duration::from_millis(5);

/// µs since the UNIX epoch (the in-process clients share the clock).
pub(crate) fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// The scenario's shape (the command line's).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ChatPlan {
    /// Chat lines a second, server-wide (0: no chat — the baseline).
    pub(crate) rate: f64,
    /// Channels the clients are split over.
    pub(crate) channels: u32,
    /// Friends per client (0: no friends service).
    pub(crate) friends: u32,
}

/// What the probe saw.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProbeTally {
    /// Queries answered.
    pub(crate) answered: u64,
    /// Queries refused: the registry's mailbox full.
    pub(crate) busy: u64,
    /// Queries that found no registry (it stopped).
    pub(crate) gone: u64,
    /// The answered queries' round trips, µs.
    pub(crate) rtt_us: Vec<u32>,
}

/// The demo, hosted with the scenario's services and probe.
pub(crate) struct ChatModule {
    demo: DemoModule,
    plan: ChatPlan,
    /// The clients' ids (`offset..offset + clients`).
    ids: std::ops::Range<u64>,
    deadline: Instant,
    /// Where the probe hands its tally at the deadline (capacity 1).
    probe: mpsc::Sender<ProbeTally>,
}

impl ChatModule {
    pub(crate) fn new(
        plan: ChatPlan,
        ids: std::ops::Range<u64>,
        deadline: Instant,
        probe: mpsc::Sender<ProbeTally>,
    ) -> Self {
        Self {
            demo: DemoModule::new(),
            plan,
            ids,
            deadline,
            probe,
        }
    }

    /// Every client's identity, by id.
    fn identities(&self) -> Vec<(u64, Identity)> {
        self.ids
            .clone()
            .map(|id| (id, Identity::new(&crate::bot::bot_name(id))))
            .collect()
    }
}

impl GameModule for ChatModule {
    fn name(&self) -> &'static str {
        self.demo.name()
    }
    fn owned_keys(&self) -> Vec<&'static str> {
        self.demo.owned_keys()
    }
    fn configure(&mut self, raw: &toml::Table, engine: &Config) -> Result<(), ServerError> {
        self.demo.configure(raw, engine)
    }
    fn register(&self, table: &mut MessageTable) {
        self.demo.register(table);
    }
    fn input_rate(&self) -> Option<gsb_core::room::InputRate> {
        self.demo.input_rate()
    }
    fn afk_action(&self) -> gsb_core::room::AfkAction {
        self.demo.afk_action()
    }
    fn describe(&self) -> String {
        format!(
            "{} + chat(rate={}/s channels={}) friends(ring={})",
            self.demo.describe(),
            self.plan.rate,
            self.plan.channels,
            self.plan.friends
        )
    }

    fn spawn_registry(&self, mut parts: RegistryParts) -> RegistryTask {
        let ids = self.identities();
        if self.plan.friends > 0 {
            let n = ids.len();
            let names: Vec<String> = ids.iter().map(|(_, i)| i.as_str().to_owned()).collect();
            let mut pairs = Vec::new();
            for i in 0..n {
                for j in 1..=self.plan.friends as usize {
                    let k = (i + j) % n;
                    if k != i {
                        pairs.push((names[i].as_str(), names[k].as_str()));
                    }
                }
            }
            use gsb_demo::friends::{FriendGraph, Friends};
            let graph = FriendGraph::new(&pairs, parts.presence(), parts.publisher("friends"));
            parts
                .service(Friends::serve(graph), |s| {
                    s.engine_events(Interest::PRESENCE)
                })
                .expect("the scenario hosts the friends once");
        }
        if self.plan.rate > 0.0 {
            let channels = self.plan.channels.max(1);
            let mut members: Vec<Vec<Identity>> = vec![Vec::new(); channels as usize];
            for (id, who) in &ids {
                members[(id % u64::from(channels)) as usize].push(who.clone());
            }
            tokio::spawn(chatter(
                parts.publisher("chat"),
                members,
                self.plan.rate,
                self.deadline,
            ));
        }
        tokio::spawn(probe(parts.presence(), self.deadline, self.probe.clone()));
        self.demo.spawn_registry(parts)
    }
}

/// Push `rate` lines a second, round-robin over the channels, until the
/// deadline. A refused push is counted by the publisher
/// (`events_refused_router_*`); nothing is retried.
async fn chatter(
    chat: gsb_core::events::Publisher,
    members: Vec<Vec<Identity>>,
    rate: f64,
    deadline: Instant,
) {
    let start = Instant::now();
    let text = vec![b'x'; TEXT_BYTES];
    let mut sent: u64 = 0;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let due = (now.duration_since(start).as_secs_f64() * rate) as u64;
        while sent < due {
            let channel = (sent % members.len() as u64) as usize;
            sent += 1;
            let line = ChatLine {
                channel: channel as u32,
                line: sent,
                sent_at_us: now_us(),
                text: text.clone(),
            };
            let _ = chat.push_many(&members[channel], &line);
        }
        tokio::time::sleep(CHAT_STEP).await;
    }
}

/// Ask the registry one presence question every [`PROBE_EVERY`] until
/// the deadline; hand the tally over.
async fn probe(presence: Presence, deadline: Instant, out: mpsc::Sender<ProbeTally>) {
    let who = [Identity::new("lg-probe")];
    let mut t = ProbeTally::default();
    let mut next = Instant::now();
    while next < deadline {
        tokio::time::sleep_until(next.into()).await;
        next += PROBE_EVERY;
        let at = Instant::now();
        match presence.of(&who).await {
            Ok(_) => {
                t.answered += 1;
                t.rtt_us
                    .push(at.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
            }
            Err(PresenceRefused::Busy) => t.busy += 1,
            Err(_) => t.gone += 1,
        }
        // A probe that fell behind does not burst to catch up.
        next = next.max(Instant::now());
    }
    let _ = out.try_send(t);
}
