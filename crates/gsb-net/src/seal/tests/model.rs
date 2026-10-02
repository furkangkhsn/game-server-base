//! A seeded adversarial network (reorder, hold back, drop, duplicate,
//! forge) between a rekeying sealer and an opener, every verdict checked
//! against a model that knows nothing about keys: genuine and fresh opens;
//! older than the window is TooOld; an opened counter is Replayed;
//! anything else forged is Forged.

use std::collections::{BTreeMap, BTreeSet};

use super::super::wire::HEADER_LEN_C2S;
use super::*;

#[derive(Default)]
struct Model {
    seen: BTreeSet<u64>,
    top: Option<u64>,
}

impl Model {
    fn expect(&self, counter: u64, genuine: bool) -> Result<u64, Refusal> {
        match self.top {
            Some(t) if counter <= t && t - counter >= REPLAY_WINDOW => Err(Refusal::TooOld),
            _ if self.seen.contains(&counter) => Err(Refusal::Replayed),
            _ if genuine => Ok(counter),
            _ => Err(Refusal::Forged),
        }
    }
}

fn pick(rng: &mut Rng, len: usize, front: usize) -> usize {
    rng.below(len.min(front) as u64) as usize
}

fn run(seed: u64, steps: usize) {
    let (mut cs, _, _, mut so, _) = pair(seed);
    let mut rng = Rng(seed);
    let mut model = Model::default();
    let mut flight: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut held: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut tally: BTreeMap<&str, u64> = BTreeMap::new();
    let mut rekeys = 0;
    let mut deliver = |c: u64, d: &[u8], genuine: bool, cs: &mut Sealer, so: &mut Opener| {
        let expect = model.expect(c, genuine);
        let got = so.open(d).map(|o| o.counter);
        assert_eq!(got, expect, "seed {seed}, counter {c}, genuine {genuine}");
        if got.is_ok() {
            model.seen.insert(c);
            model.top = Some(model.top.map_or(c, |t| t.max(c)));
            cs.note_peer_ack(c); // only authenticated datagrams are acked
        }
        *tally
            .entry(got.map_or_else(|r| r.name(), |_| "ok"))
            .or_default() += 1;
    };
    for _ in 0..steps {
        let roll = rng.below(100);
        if roll < 35 {
            let mut d = Vec::new();
            let c = cs.seal(&rng.next().to_le_bytes(), &mut d).unwrap();
            flight.push((c, d));
            if c % 97 == 0 && cs.rekey().is_ok() {
                rekeys += 1;
            }
            continue;
        }
        if roll == 38 && !held.is_empty() {
            let (c, d) = held.swap_remove(pick(&mut rng, held.len(), usize::MAX));
            deliver(c, &d, true, &mut cs, &mut so);
            continue;
        }
        if flight.is_empty() {
            continue;
        }
        let i = pick(&mut rng, flight.len(), 32);
        match roll {
            35..=37 => held.push(flight.remove(i)),
            39..=43 => drop(flight.remove(i)),
            44..=53 => {
                let (c, mut x) = flight[i].clone();
                let at = HEADER_LEN_C2S + pick(&mut rng, x.len() - HEADER_LEN_C2S, usize::MAX);
                x[at] ^= 1 << rng.below(8);
                deliver(c, &x, false, &mut cs, &mut so);
            }
            54..=63 => {
                let (c, d) = flight[i].clone();
                deliver(c, &d, true, &mut cs, &mut so);
            }
            _ => {
                let (c, d) = flight.remove(i);
                deliver(c, &d, true, &mut cs, &mut so);
            }
        }
    }
    assert!(rekeys >= 3, "seed {seed}: only {rekeys} rekeys");
    assert!(
        so.generation() >= 3,
        "seed {seed}: opener followed {}",
        so.generation()
    );
    for name in ["ok", "seal_too_old", "seal_replayed", "seal_forged"] {
        assert!(
            tally.get(name).copied().unwrap_or(0) > 0,
            "seed {seed}: no {name} in {tally:?}"
        );
    }
    assert_eq!(Some(&so.forged()), tally.get("seal_forged"));
}

#[test]
fn seeded_adversarial_network_matches_the_model() {
    for seed in [1, 0x00c0_ffee, 0xdead_beef] {
        run(seed, 20_000);
    }
}
