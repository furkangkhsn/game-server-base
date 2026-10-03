//! The path told to the connection actor, on a real socket (BACKLOG
//! B103): a session paced by its reports tells its actor — open once
//! measured, suspect, then paced at the controller's rate; the very same
//! session on a door with the response off tells it nothing.

use super::*;

/// The paced test's script: two reports to start the probes, then two
/// intervals of 20 game datagrams of which the client got 5. Returns the
/// path states the actor was told, in order.
async fn script(mode: UdpCongestion) -> Vec<gsb_core::path::PathState> {
    let (client, out_tx, w, mut rx, mut inbox) = writer(mode).await;
    out_tx.send(vec![report(0, 0)]).await.unwrap();
    assert_eq!(next_probe(&client).await, 1);
    out_tx.send(vec![report(1, 0)]).await.unwrap();
    let mut received = 0;
    for id in 2..=3 {
        out_tx.send(vec![raw(100); 20]).await.unwrap();
        for _ in 0..20 {
            next_game(&client).await;
        }
        assert_eq!(next_probe(&client).await, id);
        received += 5;
        out_tx.send(vec![report(id, received)]).await.unwrap();
    }
    // One more frame through the writer: every report before it is
    // applied by the time it arrives.
    out_tx.send(vec![raw(10)]).await.unwrap();
    next_game(&client).await;
    drop(out_tx);
    let _ = totals(w, &mut rx).await;
    let mut told = Vec::new();
    while let Ok(m) = inbox.try_recv() {
        match m {
            ConnIn::Path(p) => told.push(p),
            other => panic!("only path news was expected: {other:?}"),
        }
    }
    told
}

#[tokio::test]
async fn a_paced_session_tells_its_actor_the_path() {
    let told = script(UdpCongestion::Pace).await;
    let phases: Vec<_> = told.iter().map(|p| p.phase).collect();
    assert_eq!(
        phases,
        vec![PathPhase::Open, PathPhase::Suspect, PathPhase::Paced],
        "{told:?}"
    );
    let paced = told[2];
    // What the path delivered, times BETA: a quarter of twenty ~110-byte
    // datagrams a quarter-second (≈ 2.2 kB/s × 0.85) — never under the
    // floor, one budget a second (round 4).
    let rate = paced.rate.expect("paced: a rate");
    assert!((BUDGET as u32..2_500).contains(&rate), "{rate} B/s");
    assert_eq!(paced.loss_permille.map(|l| l > 0), Some(true));
    assert!(paced.rtt.is_some() && paced.demand.is_some());
    // What the room will read per 30 Hz tick: a thirtieth of it.
    let tick = Duration::from_secs(1) / 30;
    let per_tick = paced.budget(tick).expect("paced: a budget");
    assert_eq!(per_tick, (f64::from(rate) * tick.as_secs_f64()) as usize);
    assert!(per_tick >= BUDGET / 30, "{per_tick} B a tick");
}

/// The same session, the response off: the actor's inbox carries
/// nothing new — message for message what it always did.
#[tokio::test]
async fn with_the_response_off_the_actor_is_told_nothing() {
    assert_eq!(script(UdpCongestion::Off).await, vec![]);
}
