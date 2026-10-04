//! The scenario without an ECS: the game is a struct of maps; an owner
//! is the game's own id; the game says which ids are alive.

use std::collections::BTreeMap;

use s0_calls::ecs::plain::{PlainGame, PlainRoom};

use super::*;

pub struct P {
    pub player: Player,
    pub wallet: Wallet,
    pub bag: Bag,
    pub joined: bool,
}

pub struct Game {
    pub c: Clients,
    pub players: BTreeMap<u64, P>,
    pub loots: BTreeMap<u64, Loot>,
    next: u64,
}

pub fn room(cfg: RoomConfig, clients: Clients) -> Room<PlainRoom<Game>> {
    Room::new(cfg, |mut calls| {
        register(&mut calls, &clients);
        PlainRoom {
            calls,
            game: Game {
                c: clients,
                players: BTreeMap::new(),
                loots: BTreeMap::new(),
                next: 0,
            },
        }
    })
}

impl Game {
    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
    fn by_conn(&mut self, conn: ConnId) -> Option<(u64, &mut P)> {
        self.players
            .iter_mut()
            .find(|(_, p)| p.player.conn == conn)
            .map(|(id, p)| (*id, p))
    }
}

impl PlainGame for Game {
    fn alive(&self, o: Owner) -> bool {
        self.players.contains_key(&o.0) || self.loots.contains_key(&o.0)
    }

    fn control(&mut self, c: &Control) {
        match c {
            Control::Join { conn, identity } => {
                let id = self.id();
                self.players.insert(
                    id,
                    P {
                        player: Player {
                            conn: *conn,
                            who: identity.clone(),
                        },
                        wallet: Wallet::default(),
                        bag: Bag::default(),
                        joined: true,
                    },
                );
            }
            Control::Leave { conn } => self.players.retain(|_, p| p.player.conn != *conn),
        }
    }

    fn systems(&mut self, calls: &mut Calls, ctx: &mut TickCtx) {
        let c = self.c.clone();

        // spawn loot
        if ctx.tick() == 0 {
            for l in loot_piles(ctx.rng()) {
                let id = self.id();
                self.loots.insert(id, l);
            }
        }

        // pattern 1: load on join
        for (id, p) in &mut self.players {
            if p.joined {
                calls
                    .call(c.inv.load(p.player.who.clone()))
                    .owner(Owner(*id));
                p.joined = false;
            }
        }
        for r in calls.take_resolved::<inventory::Load, ()>() {
            if let (Some(p), Ok(data)) = (self.players.get_mut(&r.owner.0), r.outcome) {
                p.wallet.gold = data.gold;
                p.bag.fill(&data.items);
            }
        }

        // friend gold transfer
        for r in calls.take_resolved::<bank::Transfer, Reserve>() {
            if let Some(p) = self.players.get_mut(&r.owner.0) {
                p.wallet.reserved -= r.ctx.0;
                if let Ok(done) = &r.outcome {
                    p.wallet.gold = done.balance;
                }
            }
        }
        for r in calls.take_incoming::<TransferGold>() {
            let Some((id, p)) = self.by_conn(r.conn) else {
                calls.reply(r.reply, Err(err(NOT_LOADED)));
                continue;
            };
            if !p.bag.loaded {
                calls.reply(r.reply, Err(err(NOT_LOADED)));
            } else if calls.in_flight::<bank::Transfer>(Owner(id)) {
                calls.reply(r.reply, Err(err(BUSY)));
            } else if p.wallet.gold - p.wallet.reserved < r.body.amount {
                calls.reply(r.reply, Err(err(INSUFFICIENT)));
            } else {
                p.wallet.reserved += r.body.amount;
                calls
                    .call(c.bank.transfer(Identity::new(&r.body.to), r.body.amount))
                    .owner(Owner(id))
                    .with(Reserve(r.body.amount))
                    .answers(r.reply);
            }
        }

        // pattern 2: loot pickup (pending state on the loot)
        for r in calls.take_resolved::<inventory::Grant, Picker>() {
            match r.outcome {
                Ok(got) => {
                    self.loots.remove(&r.owner.0);
                    if let Some(p) = self.players.get_mut(&r.ctx.0.0) {
                        p.bag.items.push(Item {
                            kind: got.item,
                            locked: false,
                        });
                        p.bag.dirty = true;
                    }
                }
                Err(_) => {
                    if let Some(l) = self.loots.get_mut(&r.owner.0) {
                        l.claimed = false;
                    }
                }
            }
        }
        for r in calls.take_incoming::<PickUp>() {
            let Some((pid, p)) = self.by_conn(r.conn) else {
                calls.reply(r.reply, Err(err(NOT_LOADED)));
                continue;
            };
            let who = p.player.who.clone();
            let Some((lid, l)) = self.loots.iter_mut().find(|(_, l)| l.id == r.body.loot) else {
                calls.reply(r.reply, Err(err(NO_SUCH_LOOT)));
                continue;
            };
            if l.claimed {
                calls.reply(r.reply, Err(err(CLAIMED)));
                continue;
            }
            l.claimed = true;
            calls
                .call(c.inv.grant(who, l.item))
                .owner(Owner(*lid))
                .with(Picker(Owner(pid)))
                .answers(r.reply);
        }

        // pattern 3: crafting with locked ingredients
        for r in calls.take_resolved::<inventory::Craft, Locked>() {
            let Some(p) = self.players.get_mut(&r.owner.0) else {
                continue;
            };
            match r.outcome {
                Ok(made) => {
                    p.bag.consume_locked(&r.ctx.0);
                    p.bag.items.push(Item {
                        kind: made.item,
                        locked: false,
                    });
                    p.bag.dirty = true;
                }
                Err(_) => p.bag.unlock(&r.ctx.0),
            }
        }
        for r in calls.take_incoming::<CraftReq>() {
            let Some((id, p)) = self.by_conn(r.conn) else {
                calls.reply(r.reply, Err(err(NOT_LOADED)));
                continue;
            };
            if !p.bag.loaded {
                calls.reply(r.reply, Err(err(NOT_LOADED)));
            } else if !p.bag.try_lock(&r.body.consume) {
                calls.reply(r.reply, Err(err(MISSING)));
            } else {
                calls
                    .call(c.inv.craft(r.body.recipe, r.body.consume.clone()))
                    .owner(Owner(id))
                    .with(Locked(r.body.consume))
                    .answers(r.reply);
            }
        }

        // pattern 4a: equip (no service)
        for r in calls.take_incoming::<Equip>() {
            let Some((_, p)) = self.by_conn(r.conn) else {
                calls.reply(r.reply, Err(err(NOT_LOADED)));
                continue;
            };
            if p.bag.loaded && p.bag.has_unlocked(r.body.item) {
                p.bag.equipped = Some(r.body.item);
                p.bag.dirty = true;
                calls.reply(r.reply, Ok(Equipped { item: r.body.item }));
            } else {
                calls.reply(r.reply, Err(err(MISSING)));
            }
        }

        // pattern 4b: autosave
        for r in calls.take_resolved::<inventory::Save, ()>() {
            if r.outcome.is_err()
                && let Some(p) = self.players.get_mut(&r.owner.0)
            {
                p.bag.dirty = true;
            }
        }
        if ctx.tick() % AUTOSAVE_EVERY == AUTOSAVE_EVERY - 1 {
            for (id, p) in &mut self.players {
                if p.bag.dirty && !calls.in_flight::<inventory::Save>(Owner(*id)) {
                    calls
                        .call(c.inv.save(
                            p.player.who.clone(),
                            p.bag.kinds(),
                            p.bag.equipped.unwrap_or(0),
                        ))
                        .owner(Owner(*id));
                    p.bag.dirty = false;
                }
            }
        }

        // loot expiry
        self.loots.retain(|_, l| ctx.tick() < l.expires);
    }

    fn state_hash(&self, h: &mut StateHasher) {
        let mut s = Summary::default();
        for p in self.players.values() {
            s.player(&p.player, &p.wallet, &p.bag);
        }
        for l in self.loots.values() {
            s.loot(l);
        }
        s.hash(h);
    }
}
