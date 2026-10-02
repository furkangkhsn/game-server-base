//! What one handshake costs the server's CPU (BACKLOG B110): an ignored
//! timing probe, not a gate — its numbers are recorded in
//! `docs/RUDP-SECURITY.md` §4. Run it in release:
//!
//! `cargo test -p gsb-net --release --lib seal::tests::cost -- --ignored --nocapture`
//!
//! It times the responder's whole `Msg1::cookie_verified` (the DH work a
//! verified proof will cost after B5a) against one X25519 base-point
//! multiplication (a key pair's generation), so the ratio says how many
//! scalar multiplications a handshake does.

use std::hint::black_box;
use std::time::Instant;

use super::*;

const N: u32 = 2000;

#[test]
#[ignore = "a timing probe (B110); run in release with --ignored --nocapture"]
fn responder_handshake_cost() {
    let key = StaticKey::generate().unwrap();
    let accept = Accept {
        cid: 1,
        reset_token: ResetKey::from_bytes([7; 32]).token(1),
    };
    let msg1s: Vec<Vec<u8>> = (0..N)
        .map(|_| {
            Initiator::new(&key.public(), b"ctx", b"")
                .unwrap()
                .msg1()
                .to_vec()
        })
        .collect();

    let t = Instant::now();
    for _ in 0..N {
        black_box(StaticKey::generate().unwrap());
    }
    let mult = t.elapsed() / N;

    let t = Instant::now();
    for m in &msg1s {
        let r = Msg1::parse(m)
            .unwrap()
            .cookie_verified(&key, b"ctx", &accept)
            .unwrap();
        black_box(r);
    }
    let responder = t.elapsed() / N;

    let t = Instant::now();
    for _ in 0..N {
        black_box(Initiator::new(&key.public(), b"ctx", b"").unwrap());
    }
    let initiator = t.elapsed() / N;

    println!(
        "B110 (n = {N}): responder handshake {responder:?}, initiator msg1 {initiator:?}, \
         one X25519 (key generation) {mult:?}, responder / X25519 = {:.1}, \
         handshakes per core-second ~ {:.0}",
        responder.as_secs_f64() / mult.as_secs_f64(),
        1.0 / responder.as_secs_f64(),
    );
}
