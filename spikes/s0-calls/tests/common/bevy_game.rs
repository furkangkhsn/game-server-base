//! The scenario as bevy function systems. `Calls` is a `Resource`;
//! clients are `Res<Svc<…Client>>`; owners are entities (`.to_owner()`,
//! `.entity()`).

use bevy_ecs::prelude::*;
use s0_calls::ecs::bevy::{BevyRoom, EntityExt, OwnerExt, Svc, Tick};

use super::*;

pub fn room(cfg: RoomConfig, clients: Clients) -> Room<BevyRoom> {
    Room::new(cfg, build(clients))
}

/// The game, given its table (live, test mode and replay build it alike).
pub fn build(clients: Clients) -> impl FnOnce(Calls) -> BevyRoom {
    move |mut calls| {
        register(&mut calls, &clients);
        let mut world = World::new();
        world.insert_resource(Svc(clients.bank.clone()));
        world.insert_resource(Svc(clients.inv.clone()));
        let mut schedule = Schedule::default();
        schedule.add_systems(
            (
                spawn_loot,
                load_on_join,
                apply_loads,
                transfers,
                loot,
                craft,
                equip,
                autosave,
                expire_loot,
            )
                .chain(),
        );
        BevyRoom::new(calls, world, schedule, control, hash)
    }
}

fn control(world: &mut World, c: &Control) {
    match c {
        Control::Join { conn, identity } => {
            world.spawn((
                Player {
                    conn: *conn,
                    who: identity.clone(),
                },
                Wallet::default(),
                Bag::default(),
                JustJoined,
            ));
        }
        Control::Leave { conn } => {
            let gone: Vec<Entity> = world
                .query::<(Entity, &Player)>()
                .iter(world)
                .filter(|(_, p)| p.conn == *conn)
                .map(|(e, _)| e)
                .collect();
            for e in gone {
                world.despawn(e);
            }
        }
    }
}

fn hash(world: &mut World, h: &mut StateHasher) {
    let mut s = Summary::default();
    for (p, w, b) in world.query::<(&Player, &Wallet, &Bag)>().iter(world) {
        s.player(p, w, b);
    }
    for l in world.query::<&Loot>().iter(world) {
        s.loot(l);
    }
    s.hash(h);
}

fn spawn_loot(mut tick: ResMut<Tick>, mut commands: Commands) {
    if tick.0.tick() == 0 {
        for l in loot_piles(tick.0.rng()) {
            commands.spawn(l);
        }
    }
}

/// Pattern 1 — load on join.
fn load_on_join(
    mut calls: ResMut<Calls>,
    inv: Res<Svc<InventoryClient>>,
    joined: Query<(Entity, &Player), With<JustJoined>>,
    mut commands: Commands,
) {
    for (e, p) in &joined {
        calls.call(inv.load(p.who.clone())).owner(e.to_owner());
        commands.entity(e).remove::<JustJoined>();
    }
}

fn apply_loads(mut calls: ResMut<Calls>, mut q: Query<(&mut Wallet, &mut Bag)>) {
    for r in calls.take_resolved::<inventory::Load, ()>() {
        let Ok((mut wallet, mut bag)) = q.get_mut(r.owner.entity()) else {
            continue;
        };
        if let Ok(data) = r.outcome {
            wallet.gold = data.gold;
            bag.fill(&data.items);
        }
    }
}

/// Friend gold transfer: reserve, one charge, answered by `.answers`.
fn transfers(
    mut calls: ResMut<Calls>,
    bank: Res<Svc<BankClient>>,
    mut q: Query<(Entity, &Player, &mut Wallet, &Bag)>,
) {
    for r in calls.take_resolved::<bank::Transfer, Reserve>() {
        if let Ok((_, _, mut w, _)) = q.get_mut(r.owner.entity()) {
            w.reserved -= r.ctx.0;
            if let Ok(done) = &r.outcome {
                w.gold = done.balance;
            }
        }
    }
    for r in calls.take_incoming::<TransferGold>() {
        let Some((e, _, mut w, bag)) = q.iter_mut().find(|(_, p, ..)| p.conn == r.conn) else {
            calls.reply(r.reply, Err(err(NOT_LOADED)));
            continue;
        };
        let me = e.to_owner();
        if !bag.loaded {
            calls.reply(r.reply, Err(err(NOT_LOADED)));
        } else if calls.in_flight::<bank::Transfer>(me) {
            calls.reply(r.reply, Err(err(BUSY)));
        } else if w.gold - w.reserved < r.body.amount {
            calls.reply(r.reply, Err(err(INSUFFICIENT)));
        } else {
            w.reserved += r.body.amount;
            calls
                .call(bank.transfer(Identity::new(&r.body.to), r.body.amount))
                .owner(me)
                .with(Reserve(r.body.amount))
                .answers(r.reply);
        }
    }
}

/// Pattern 2 — loot pickup; the pending state is on the loot entity.
fn loot(
    mut calls: ResMut<Calls>,
    inv: Res<Svc<InventoryClient>>,
    mut bags: Query<(Entity, &Player, &mut Bag)>,
    mut piles: Query<(Entity, &mut Loot)>,
    mut commands: Commands,
) {
    for r in calls.take_resolved::<inventory::Grant, Picker>() {
        match r.outcome {
            Ok(got) => {
                commands.entity(r.owner.entity()).despawn();
                if let Ok((_, _, mut bag)) = bags.get_mut(r.ctx.0.entity()) {
                    bag.items.push(Item {
                        kind: got.item,
                        locked: false,
                    });
                    bag.dirty = true;
                }
            }
            Err(_) => {
                if let Ok((_, mut l)) = piles.get_mut(r.owner.entity()) {
                    l.claimed = false;
                }
            }
        }
    }
    for r in calls.take_incoming::<PickUp>() {
        let Some((pe, p, _)) = bags.iter().find(|(_, p, _)| p.conn == r.conn) else {
            calls.reply(r.reply, Err(err(NOT_LOADED)));
            continue;
        };
        let Some((le, mut l)) = piles.iter_mut().find(|(_, l)| l.id == r.body.loot) else {
            calls.reply(r.reply, Err(err(NO_SUCH_LOOT)));
            continue;
        };
        if l.claimed {
            calls.reply(r.reply, Err(err(CLAIMED)));
            continue;
        }
        l.claimed = true;
        calls
            .call(inv.grant(p.who.clone(), l.item))
            .owner(le.to_owner())
            .with(Picker(pe.to_owner()))
            .answers(r.reply);
    }
}

/// Pattern 3 — crafting with locked ingredients, answered by `.answers`.
fn craft(
    mut calls: ResMut<Calls>,
    inv: Res<Svc<InventoryClient>>,
    mut q: Query<(Entity, &Player, &mut Bag)>,
) {
    for r in calls.take_resolved::<inventory::Craft, Locked>() {
        let Ok((_, _, mut bag)) = q.get_mut(r.owner.entity()) else {
            continue;
        };
        match r.outcome {
            Ok(made) => {
                bag.consume_locked(&r.ctx.0);
                bag.items.push(Item {
                    kind: made.item,
                    locked: false,
                });
                bag.dirty = true;
            }
            Err(_) => bag.unlock(&r.ctx.0),
        }
    }
    for r in calls.take_incoming::<CraftReq>() {
        let Some((e, _, mut bag)) = q.iter_mut().find(|(_, p, _)| p.conn == r.conn) else {
            calls.reply(r.reply, Err(err(NOT_LOADED)));
            continue;
        };
        if !bag.loaded {
            calls.reply(r.reply, Err(err(NOT_LOADED)));
        } else if !bag.try_lock(&r.body.consume) {
            calls.reply(r.reply, Err(err(MISSING)));
        } else {
            calls
                .call(inv.craft(r.body.recipe, r.body.consume.clone()))
                .owner(e.to_owner())
                .with(Locked(r.body.consume))
                .answers(r.reply);
        }
    }
}

/// Pattern 4a — equip: no service, answered at once.
fn equip(mut calls: ResMut<Calls>, mut q: Query<(&Player, &mut Bag)>) {
    for r in calls.take_incoming::<Equip>() {
        let Some((_, mut bag)) = q.iter_mut().find(|(p, _)| p.conn == r.conn) else {
            calls.reply(r.reply, Err(err(NOT_LOADED)));
            continue;
        };
        if bag.loaded && bag.has_unlocked(r.body.item) {
            bag.equipped = Some(r.body.item);
            bag.dirty = true;
            calls.reply(r.reply, Ok(Equipped { item: r.body.item }));
        } else {
            calls.reply(r.reply, Err(err(MISSING)));
        }
    }
}

/// Pattern 4b — autosave: a call nobody asked for, one in flight per player.
fn autosave(
    mut calls: ResMut<Calls>,
    inv: Res<Svc<InventoryClient>>,
    tick: Res<Tick>,
    mut q: Query<(Entity, &Player, &mut Bag)>,
) {
    for r in calls.take_resolved::<inventory::Save, ()>() {
        if r.outcome.is_err()
            && let Ok((_, _, mut bag)) = q.get_mut(r.owner.entity())
        {
            bag.dirty = true;
        }
    }
    if tick.0.tick() % AUTOSAVE_EVERY != AUTOSAVE_EVERY - 1 {
        return;
    }
    for (e, p, mut bag) in &mut q {
        if bag.dirty && !calls.in_flight::<inventory::Save>(e.to_owner()) {
            calls
                .call(inv.save(p.who.clone(), bag.kinds(), bag.equipped.unwrap_or(0)))
                .owner(e.to_owner());
            bag.dirty = false;
        }
    }
}

fn expire_loot(tick: Res<Tick>, piles: Query<(Entity, &Loot)>, mut commands: Commands) {
    for (e, l) in &piles {
        if tick.0.tick() >= l.expires {
            commands.entity(e).despawn();
        }
    }
}
