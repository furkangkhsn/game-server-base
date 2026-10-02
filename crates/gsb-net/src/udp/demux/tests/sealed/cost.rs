//! The demux's CPU per inbound datagram, sealed against plaintext (B5a's
//! measurement; BACKLOG B107's open question): an IGNORED timing probe,
//! run by hand in release —
//!
//! ```text
//! cargo test -p gsb-net --release --lib demux::tests::sealed::cost -- --ignored --nocapture
//! ```
//!
//! One session, a game-band input of `PAYLOAD` bytes per datagram (a
//! client's typical input), fed straight into `Demux::handle` (no socket
//! read: what is timed is the demux's own work — routing, the record
//! open on a sealed door, the dispatch, the forward). The difference is
//! the record layer's cost per datagram.

use std::time::Instant as Clock;

use super::*;

const N: usize = 200_000;
const PAYLOAD: usize = 40;
const RUNS: usize = 3;

/// A demux whose session inbox holds every frame of a run.
async fn bare(sealed: bool) -> (Demux, Client, Option<Sealer>) {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock.writable().await.expect("writable");
    let (mut d, end_rx, server) = match sealed {
        true => sealed_demux(sock, None),
        false => {
            let (d, rx) = demux_bare(sock);
            (d, rx, [0; 32])
        }
    };
    d.inbox_cap = N + 16;
    d.idle = None;
    let a = client();
    let tx = match sealed {
        true => {
            let (_, session, _, _) = handshake(&mut d, &a, &server).await;
            Some(session.into_halves().0)
        }
        false => {
            let cookie = cookie_for(&mut d, &a, 1).await;
            feed(&mut d, addr(&a), &encode_hello(1, cookie));
            let _ = recv(&a).await;
            None
        }
    };
    // Keep the endpoint (and so the session's inbox) alive.
    std::mem::forget(end_rx.try_recv().expect("an endpoint"));
    (d, a, tx)
}

#[tokio::test]
#[ignore = "timing probe: run by hand in release (module docs)"]
async fn demux_cpu_per_datagram_sealed_and_plaintext() {
    let inner = encode_raw(&FrameBody::new(1000, Bytes::from(vec![7u8; PAYLOAD])));
    for run in 0..RUNS {
        let mut per = Vec::new();
        for sealed in [false, true] {
            let (mut d, a, tx) = bare(sealed).await;
            let datagrams: Vec<Vec<u8>> = match tx {
                Some(mut tx) => (0..N)
                    .map(|_| {
                        let mut out = Vec::new();
                        tx.seal(&inner, &mut out).unwrap();
                        out
                    })
                    .collect(),
                None => vec![inner.clone(); N],
            };
            let from = addr(&a);
            let t = Clock::now();
            for dg in &datagrams {
                feed(&mut d, from, dg);
            }
            let ns = t.elapsed().as_nanos() as f64 / N as f64;
            per.push((sealed, datagrams[0].len(), ns));
        }
        for (sealed, len, ns) in &per {
            println!(
                "run {run}: {} datagram {len} B: {ns:.0} ns/datagram",
                if *sealed { "sealed   " } else { "plaintext" }
            );
        }
        println!(
            "run {run}: the record layer adds {:.0} ns/datagram",
            per[1].2 - per[0].2
        );
    }
}
