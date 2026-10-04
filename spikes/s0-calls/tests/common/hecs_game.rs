//! The scenario on hecs: systems are plain functions taking
//! `(&mut World, &mut Calls, …)`.

use s0_calls::ecs::hecs::{Entity, EntityExt, HecsGame, HecsRoom, OwnerExt, World};

use super::*;

pub struct Game {
    pub c: Clients,
}

pub fn room(cfg: RoomConfig, clients: Clients) -> Room<HecsRoom<Game>> {
    Room::new(cfg, |mut calls| {
        register(&mut calls, &clients);
        HecsRoom {
            world: World::new(),
            calls,
            game: Game { c: clients },
        }
    })
}

impl HecsGame for Game {
    fn control(&mut self, world: &mut World, c: &Control) {
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
                    .query::<&Player>()
                    .iter()
                    .filter(|(_, p)| p.conn == *conn)
                    .map(|(e, _)| e)
                    .collect();
                for e in gone {
                    let _ = world.despawn(e);
                }
            }
        }
    }

    fn systems(&mut self, w: &mut World, calls: &mut Calls, ctx: &mut TickCtx) {
        spawn_loot(w, ctx);
        load_on_join(w, calls, &self.c.inv);
        apply_loads(w, calls);
        transfers(w, calls, &self.c.bank);
        loot(w, calls, &self.c.inv);
        craft(w, calls, &self.c.inv);
        equip(w, calls);
        autosave(w, calls, &self.c.inv, ctx);
        expire_loot(w, ctx);
    }

    fn state_hash(&self, world: &World, h: &mut StateHasher) {
        let mut s = Summary::default();
        for (_, (p, wl, b)) in world.query::<(&Player, &Wallet, &Bag)>().iter() {
            s.player(p, wl, b);
        }
        for (_, l) in world.query::<&Loot>().iter() {
            s.loot(l);
        }
        s.hash(h);
    }
}

fn player_by_conn(w: &mut World, conn: ConnId) -> Option<Entity> {
    w.query_mut::<&Player>()
        .into_iter()
        .find(|(_, p)| p.conn == conn)
        .map(|(e, _)| e)
}

fn spawn_loot(w: &mut World, ctx: &mut TickCtx) {
    if ctx.tick() == 0 {
        for l in loot_piles(ctx.rng()) {
            w.spawn((l,));
        }
    }
}

fn load_on_join(w: &mut World, calls: &mut Calls, inv: &InventoryClient) {
    let joined: Vec<(Entity, Identity)> = w
        .query_mut::<(&Player, &JustJoined)>()
        .into_iter()
        .map(|(e, (p, _))| (e, p.who.clone()))
        .collect();
    for (e, who) in joined {
        calls.call(inv.load(who)).owner(e.to_owner());
        let _ = w.remove_one::<JustJoined>(e);
    }
}

fn apply_loads(w: &mut World, calls: &mut Calls) {
    for r in calls.take_resolved::<inventory::Load, ()>() {
        let Some(e) = r.owner.entity() else { continue };
        let Ok((wallet, bag)) = w.query_one_mut::<(&mut Wallet, &mut Bag)>(e) else {
            continue;
        };
        if let Ok(data) = r.outcome {
            wallet.gold = data.gold;
            bag.fill(&data.items);
        }
    }
}

fn transfers(w: &mut World, calls: &mut Calls, bank: &BankClient) {
    for r in calls.take_resolved::<bank::Transfer, Reserve>() {
        let Some(e) = r.owner.entity() else { continue };
        if let Ok(wallet) = w.query_one_mut::<&mut Wallet>(e) {
            wallet.reserved -= r.ctx.0;
            if let Ok(done) = &r.outcome {
                wallet.gold = done.balance;
            }
        }
    }
    for r in calls.take_incoming::<TransferGold>() {
        let Some(e) = player_by_conn(w, r.conn) else {
            calls.reply(r.reply, Err(err(NOT_LOADED)));
            continue;
        };
        let me = e.to_owner();
        let Ok((wallet, bag)) = w.query_one_mut::<(&mut Wallet, &Bag)>(e) else {
            continue;
        };
        if !bag.loaded {
            calls.reply(r.reply, Err(err(NOT_LOADED)));
        } else if calls.in_flight::<bank::Transfer>(me) {
            calls.reply(r.reply, Err(err(BUSY)));
        } else if wallet.gold - wallet.reserved < r.body.amount {
            calls.reply(r.reply, Err(err(INSUFFICIENT)));
        } else {
            wallet.reserved += r.body.amount;
            calls
                .call(bank.transfer(Identity::new(&r.body.to), r.body.amount))
                .owner(me)
                .with(Reserve(r.body.amount))
                .answers(r.reply);
        }
    }
}

fn loot(w: &mut World, calls: &mut Calls, inv: &InventoryClient) {
    for r in calls.take_resolved::<inventory::Grant, Picker>() {
        let Some(pile) = r.owner.entity() else {
            continue;
        };
        match r.outcome {
            Ok(got) => {
                let _ = w.despawn(pile);
                if let Some(pe) = r.ctx.0.entity()
                    && let Ok(bag) = w.query_one_mut::<&mut Bag>(pe)
                {
                    bag.items.push(Item {
                        kind: got.item,
                        locked: false,
                    });
                    bag.dirty = true;
                }
            }
            Err(_) => {
                if let Ok(l) = w.query_one_mut::<&mut Loot>(pile) {
                    l.claimed = false;
                }
            }
        }
    }
    for r in calls.take_incoming::<PickUp>() {
        let Some(pe) = player_by_conn(w, r.conn) else {
            calls.reply(r.reply, Err(err(NOT_LOADED)));
            continue;
        };
        let who = w.get::<&Player>(pe).map(|p| p.who.clone()).ok();
        let Some(who) = who else { continue };
        let Some((le, l)) = w
            .query_mut::<&mut Loot>()
            .into_iter()
            .find(|(_, l)| l.id == r.body.loot)
        else {
            calls.reply(r.reply, Err(err(NO_SUCH_LOOT)));
            continue;
        };
        if l.claimed {
            calls.reply(r.reply, Err(err(CLAIMED)));
            continue;
        }
        l.claimed = true;
        calls
            .call(inv.grant(who, l.item))
            .owner(le.to_owner())
            .with(Picker(pe.to_owner()))
            .answers(r.reply);
    }
}

fn craft(w: &mut World, calls: &mut Calls, inv: &InventoryClient) {
    for r in calls.take_resolved::<inventory::Craft, Locked>() {
        let Some(e) = r.owner.entity() else { continue };
        let Ok(bag) = w.query_one_mut::<&mut Bag>(e) else {
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
        let Some(e) = player_by_conn(w, r.conn) else {
            calls.reply(r.reply, Err(err(NOT_LOADED)));
            continue;
        };
        let Ok(bag) = w.query_one_mut::<&mut Bag>(e) else {
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

fn equip(w: &mut World, calls: &mut Calls) {
    for r in calls.take_incoming::<Equip>() {
        let Some(e) = player_by_conn(w, r.conn) else {
            calls.reply(r.reply, Err(err(NOT_LOADED)));
            continue;
        };
        let Ok(bag) = w.query_one_mut::<&mut Bag>(e) else {
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

fn autosave(w: &mut World, calls: &mut Calls, inv: &InventoryClient, ctx: &TickCtx) {
    for r in calls.take_resolved::<inventory::Save, ()>() {
        if r.outcome.is_err()
            && let Some(e) = r.owner.entity()
            && let Ok(bag) = w.query_one_mut::<&mut Bag>(e)
        {
            bag.dirty = true;
        }
    }
    if ctx.tick() % AUTOSAVE_EVERY != AUTOSAVE_EVERY - 1 {
        return;
    }
    for (e, (p, bag)) in w.query_mut::<(&Player, &mut Bag)>() {
        if bag.dirty && !calls.in_flight::<inventory::Save>(e.to_owner()) {
            calls
                .call(inv.save(p.who.clone(), bag.kinds(), bag.equipped.unwrap_or(0)))
                .owner(e.to_owner());
            bag.dirty = false;
        }
    }
}

fn expire_loot(w: &mut World, ctx: &TickCtx) {
    let gone: Vec<Entity> = w
        .query_mut::<&Loot>()
        .into_iter()
        .filter(|(_, l)| ctx.tick() >= l.expires)
        .map(|(e, _)| e)
        .collect();
    for e in gone {
        let _ = w.despawn(e);
    }
}
