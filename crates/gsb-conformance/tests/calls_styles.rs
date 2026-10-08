//! The same game three ways — no ECS, bevy_ecs (`gsb-bevy-ecs`), hecs
//! (`gsb-hecs`) — against a real room actor: the traces are identical
//! (`docs/SERVICES.md` §5.5, Faz 5), for the calls scenario and for the
//! events one (`docs/EVENTS.md` §4.5, round e5).

use std::collections::{BTreeMap, HashMap};

use gsb_conformance::{
    Answer, BUSY, BankClient, Charge, GoldReceived, INSUFFICIENT, Reserve, Style, Trace,
    TransferGold, Wallet, credit, decide, play, play_events, settle,
};
use gsb_core::calls::{Calls, Owner};
use gsb_core::events::{Addressee, Events};
use gsb_core::id::PlayerId;

/// What a call's outcome confirms.
fn balance(o: &gsb_core::calls::Outcome<Charge>) -> Option<u64> {
    o.as_ref().ok().map(|d| d.balance)
}

// ---- no ECS: the owner is the player ------------------------------------------

struct Plain;

impl Style for Plain {
    type World = BTreeMap<PlayerId, Wallet>;
    fn join(&mut self, w: &mut Self::World, p: PlayerId) {
        w.insert(p, Wallet::START);
    }
    fn leave(&mut self, w: &mut Self::World, p: PlayerId) {
        w.remove(&p);
    }
    fn owner(&self, _w: &Self::World, p: PlayerId) -> Option<Owner> {
        Some(p.into())
    }
    fn alive(&self, w: &Self::World, o: Owner) -> bool {
        w.contains_key(&PlayerId(o.to_bits()))
    }
    fn systems(
        &mut self,
        w: &mut Self::World,
        calls: &mut Calls,
        events: &mut Events,
        bank: &BankClient,
    ) {
        for e in events.take::<GoldReceived>() {
            match e.to {
                Addressee::Room => w.values_mut().for_each(|x| credit(x, e.body.amount)),
                Addressee::Player(_) => {
                    if let Some(x) = w.get_mut(&PlayerId(e.owner.to_bits())) {
                        credit(x, e.body.amount);
                    }
                }
            }
        }
        for r in calls.take_resolved::<Charge, Reserve>() {
            if let Some(wallet) = w.get_mut(&PlayerId(r.owner.to_bits())) {
                settle(wallet, r.ctx, balance(&r.outcome));
            }
        }
        for r in calls.take_incoming::<TransferGold>() {
            let busy = calls.in_flight::<Charge>(r.owner);
            let wallet = w.get_mut(&PlayerId(r.owner.to_bits()));
            match decide(wallet, busy, r.body.amount) {
                Ok(c) => {
                    let reserve = Reserve(c.amount);
                    calls
                        .call(bank.charge(c))
                        .owner(r.owner)
                        .with(reserve)
                        .answers(r.reply);
                }
                Err(e) => calls.reply(r.reply, Err(e)),
            }
        }
    }
    fn wallets(&mut self, w: &mut Self::World) -> Vec<(PlayerId, Wallet)> {
        w.iter().map(|(p, w)| (*p, *w)).collect()
    }
}

// ---- bevy_ecs: the table as a resource, the owner an entity ----------------------

mod bevy {
    use super::*;
    use bevy_ecs::prelude::{Component, Entity, Query, Res, ResMut, Schedule, World};
    use bevy_ecs::schedule::{IntoScheduleConfigs, SingleThreadedExecutor};
    use gsb_bevy_ecs::{EntityExt, GsbCalls, GsbEvents, OwnerExt, Svc};

    #[derive(Component)]
    struct Player(PlayerId);

    #[derive(Component)]
    struct Purse(Wallet);

    fn gold(mut events: ResMut<GsbEvents>, mut q: Query<&mut Purse>) {
        for e in events.take::<GoldReceived>() {
            match e.owner.entity() {
                Some(x) => {
                    if let Ok(mut purse) = q.get_mut(x) {
                        credit(&mut purse.0, e.body.amount);
                    }
                }
                // Room-wide (`Owner::ACTOR`): every purse.
                None => q
                    .iter_mut()
                    .for_each(|mut purse| credit(&mut purse.0, e.body.amount)),
            }
        }
    }

    fn settle_charges(mut calls: ResMut<GsbCalls>, mut q: Query<&mut Purse>) {
        for r in calls.take_resolved::<Charge, Reserve>() {
            if let Some(mut purse) = r.owner.entity().and_then(|e| q.get_mut(e).ok()) {
                settle(&mut purse.0, r.ctx, balance(&r.outcome));
            }
        }
    }

    fn transfers(
        mut calls: ResMut<GsbCalls>,
        bank: Res<Svc<BankClient>>,
        mut q: Query<&mut Purse>,
    ) {
        for r in calls.take_incoming::<TransferGold>() {
            let busy = calls.in_flight::<Charge>(r.owner);
            let mut purse = r.owner.entity().and_then(|e| q.get_mut(e).ok());
            match decide(purse.as_mut().map(|p| &mut p.0), busy, r.body.amount) {
                Ok(c) => {
                    let reserve = Reserve(c.amount);
                    calls
                        .call(bank.charge(c))
                        .owner(r.owner)
                        .with(reserve)
                        .answers(r.reply);
                }
                Err(e) => calls.reply(r.reply, Err(e)),
            }
        }
    }

    pub(super) struct Bevy {
        players: HashMap<PlayerId, Entity>,
        schedule: Schedule,
    }

    impl Bevy {
        pub(super) fn new() -> Self {
            let mut schedule = Schedule::default();
            schedule.set_executor(SingleThreadedExecutor::new());
            schedule.add_systems((gold, settle_charges, transfers).chain());
            Self {
                players: HashMap::new(),
                schedule,
            }
        }
    }

    impl Style for Bevy {
        type World = World;
        fn join(&mut self, w: &mut World, p: PlayerId) {
            let e = w.spawn((Player(p), Purse(Wallet::START))).id();
            self.players.insert(p, e);
        }
        fn leave(&mut self, w: &mut World, p: PlayerId) {
            if let Some(e) = self.players.remove(&p) {
                w.despawn(e);
            }
        }
        fn owner(&self, _w: &World, p: PlayerId) -> Option<Owner> {
            self.players.get(&p).map(|e| e.to_owner())
        }
        fn alive(&self, w: &World, o: Owner) -> bool {
            gsb_bevy_ecs::owner_alive(w, o)
        }
        fn systems(
            &mut self,
            w: &mut World,
            calls: &mut Calls,
            events: &mut Events,
            bank: &BankClient,
        ) {
            if !w.contains_resource::<Svc<BankClient>>() {
                w.insert_resource(Svc(bank.clone()));
            }
            gsb_bevy_ecs::events::lend(w, events, |w| {
                gsb_bevy_ecs::lend(w, calls, |w| self.schedule.run(w))
            });
        }
        fn wallets(&mut self, w: &mut World) -> Vec<(PlayerId, Wallet)> {
            let mut out: Vec<_> = w
                .query::<(&Player, &Purse)>()
                .iter(w)
                .map(|(p, purse)| (p.0, purse.0))
                .collect();
            out.sort_by_key(|(p, _)| *p);
            out
        }
    }
}

// ---- hecs: plain-function systems, the owner an entity ----------------------------

mod hecs_style {
    use super::*;
    use gsb_hecs::{EntityExt, OwnerExt};
    use hecs::{Entity, World};

    struct Player(PlayerId);

    fn gold(w: &mut World, events: &mut Events) {
        let rest = gsb_hecs::take_for::<GoldReceived, Wallet>(w, events, |wallet, e| {
            credit(wallet, e.body.amount)
        });
        // Room-wide (`Owner::ACTOR`, no entity's): every wallet.
        for e in rest.iter().filter(|e| e.to == Addressee::Room) {
            for (_, wallet) in w.query_mut::<&mut Wallet>() {
                credit(wallet, e.body.amount);
            }
        }
    }

    fn settle_charges(w: &mut World, calls: &mut Calls) {
        for r in calls.take_resolved::<Charge, Reserve>() {
            if let Some(e) = r.owner.entity()
                && let Ok(mut wallet) = w.get::<&mut Wallet>(e)
            {
                settle(&mut wallet, r.ctx, balance(&r.outcome));
            }
        }
    }

    fn transfers(w: &mut World, calls: &mut Calls, bank: &BankClient) {
        for r in calls.take_incoming::<TransferGold>() {
            let busy = calls.in_flight::<Charge>(r.owner);
            let mut wallet = r.owner.entity().and_then(|e| w.get::<&mut Wallet>(e).ok());
            match decide(wallet.as_deref_mut(), busy, r.body.amount) {
                Ok(c) => {
                    let reserve = Reserve(c.amount);
                    calls
                        .call(bank.charge(c))
                        .owner(r.owner)
                        .with(reserve)
                        .answers(r.reply);
                }
                Err(e) => calls.reply(r.reply, Err(e)),
            }
        }
    }

    #[derive(Default)]
    pub(super) struct Hecs {
        players: HashMap<PlayerId, Entity>,
    }

    impl Style for Hecs {
        type World = World;
        fn join(&mut self, w: &mut World, p: PlayerId) {
            let e = w.spawn((Player(p), Wallet::START));
            self.players.insert(p, e);
        }
        fn leave(&mut self, w: &mut World, p: PlayerId) {
            if let Some(e) = self.players.remove(&p) {
                let _ = w.despawn(e);
            }
        }
        fn owner(&self, _w: &World, p: PlayerId) -> Option<Owner> {
            self.players.get(&p).map(|e| e.to_owner())
        }
        fn alive(&self, w: &World, o: Owner) -> bool {
            gsb_hecs::owner_alive(w, o)
        }
        fn systems(
            &mut self,
            w: &mut World,
            calls: &mut Calls,
            events: &mut Events,
            bank: &BankClient,
        ) {
            gold(w, events);
            settle_charges(w, calls);
            transfers(w, calls, bank);
        }
        fn wallets(&mut self, w: &mut World) -> Vec<(PlayerId, Wallet)> {
            let mut out: Vec<_> = w
                .query::<(&Player, &Wallet)>()
                .iter()
                .map(|(_, (p, wallet))| (p.0, *wallet))
                .collect();
            out.sort_by_key(|(p, _)| *p);
            out
        }
    }
}

fn wallet(gold: u64, reserved: u64) -> Wallet {
    Wallet { gold, reserved }
}

/// What every style must produce.
fn check(t: &Trace) {
    assert_eq!(t.charges, 1, "one charge");
    assert_eq!(t.bank_calls, 1);
    assert_eq!(
        t.answers,
        [
            (2, 1, 11, Answer::GameError(BUSY)),
            (2, 2, 20, Answer::GameError(INSUFFICIENT)),
            (3, 1, 10, Answer::Done(400)),
        ]
    );
    let (alice, bob) = (PlayerId(1), PlayerId(2));
    let at = |tick: u64| {
        t.wallets
            .iter()
            .find(|(k, _)| *k == tick)
            .map(|(_, w)| w.clone())
            .expect("a tick's wallets")
    };
    assert_eq!(at(2), [(alice, wallet(500, 100)), (bob, wallet(500, 0))]);
    assert_eq!(at(3), [(alice, wallet(400, 0)), (bob, wallet(500, 0))]);
    let c = t.counts;
    assert_eq!(
        (
            c.calls_issued,
            c.calls_sent,
            c.results_ok,
            c.responses_delivered,
            c.answers_sent,
            c.requests_answered
        ),
        (1, 1, 1, 1, 1, 2)
    );
}

#[test]
fn the_same_game_gives_the_same_trace_without_an_ecs_on_bevy_ecs_and_on_hecs() {
    let plain = play(Plain, BTreeMap::new());
    check(&plain);
    let bevy = play(bevy::Bevy::new(), bevy_ecs::world::World::new());
    assert_eq!(bevy, plain, "bevy_ecs");
    let hecs = play(hecs_style::Hecs::default(), hecs::World::new());
    assert_eq!(hecs, plain, "hecs");
}

/// What every style must produce in the events scenario: bob's 200
/// received pays his 600 in the same tick; the room-wide 7 reaches both.
fn check_events(t: &Trace) {
    assert_eq!((t.charges, t.bank_calls), (1, 1));
    assert_eq!(t.answers, [(3, 2, 20, Answer::Done(100))]);
    let (alice, bob) = (PlayerId(1), PlayerId(2));
    assert_eq!(
        t.wallets,
        [
            (1, vec![(alice, wallet(500, 0)), (bob, wallet(500, 0))]),
            (2, vec![(alice, wallet(500, 0)), (bob, wallet(700, 600))]),
            (3, vec![(alice, wallet(500, 0)), (bob, wallet(100, 0))]),
            (4, vec![(alice, wallet(507, 0)), (bob, wallet(107, 0))]),
            (5, vec![(alice, wallet(507, 0)), (bob, wallet(107, 0))]),
        ]
    );
    let e = t.events;
    assert_eq!(
        (e.events_admitted, e.events_taken, e.events_untaken),
        (2, 2, 0)
    );
}

#[test]
fn the_same_events_give_the_same_trace_without_an_ecs_on_bevy_ecs_and_on_hecs() {
    let plain = play_events(Plain, BTreeMap::new());
    check_events(&plain);
    let bevy = play_events(bevy::Bevy::new(), bevy_ecs::world::World::new());
    assert_eq!(bevy, plain, "bevy_ecs");
    let hecs = play_events(hecs_style::Hecs::default(), hecs::World::new());
    assert_eq!(hecs, plain, "hecs");
}
