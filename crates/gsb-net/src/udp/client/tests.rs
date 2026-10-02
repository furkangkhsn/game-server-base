//! `UdpClientStats` — the four client-side transport counters the load
//! generator prints on its `RESULT` line.
//!
//! The BEHAVIOURS behind them are tested elsewhere in this crate
//! (dedup, out-of-order handling, retransmission, the liveness bound),
//! but through the server's demux and a raw socket — so the client's own
//! counters had never been read back. They are what an operator reads
//! after a lossy run: "was the loss on the wire (`dup_in`,
//! `oob_dropped`) or did a direction die (`gave_up`)?". A counter that
//! never fires answers that question wrong and confidently.
//!
//! The tests drive the real counting functions (`process_datagram`,
//! `retransmit_pass`) on a real `UdpClient` over a real socket, with real
//! wire-format datagrams. Deterministic: the clocks the retransmit rules
//! read are fields of the client, so a test rewinds them instead of
//! sleeping.

use super::*;

/// A client wired to a peer that never answers, without a handshake (the
/// handshake is tested in `udp::tests`; this module is about what the
/// counters do afterwards). The second socket is returned so the peer
/// address stays bound for the client's lifetime.
async fn detached() -> (UdpClient, UdpSocket) {
    detached_with(UdpClientConfig::default()).await
}

/// [`detached`] with a [`UdpClientConfig`].
async fn detached_with(config: UdpClientConfig) -> (UdpClient, UdpSocket) {
    let sock = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind the client socket");
    // Its writability known to the runtime, as a connected client's is
    // (the handshake sent on it): the synchronous sends then go out.
    sock.writable().await.expect("writable");
    let sink = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind the silent peer");
    let peer = sink.local_addr().expect("the peer is bound");
    let client = UdpClient {
        sock,
        peer,
        established: true,
        in_expected: 1,
        in_oob: HashMap::new(),
        pending: VecDeque::new(),
        out_seq: 0,
        rel: RelSend::new(Instant::now(), Rto::default()),
        stats: UdpClientStats::default(),
        buf: vec![0u8; 2048],
        raw: None,
        reasm: Reassembly::default(),
        reports: super::report::Reports::new(config),
        path: super::migrate::Migration::new(config.migration),
    };
    (client, sink)
}

fn rel(seq: u32, op: u16, payload: &[u8]) -> Vec<u8> {
    encode_rel(seq, &FrameBody::new(op, Bytes::copy_from_slice(payload)))
}

/// `dup_in` counts the SERVER's retransmissions this client observed —
/// a REL frame it has already delivered and ACKed.
///
/// The counter's whole job is attribution: it is the server's retransmit
/// seen from here, so a non-zero `dup_in` with a zero `retrans_out`
/// means loss on the server→client leg specifically. The load-bearing
/// second half is that the duplicate is not re-DELIVERED: control runs
/// exactly once, so a `dup_in` that came with a second delivery would be
/// counting a correctness bug rather than a wire observation.
#[tokio::test]
async fn dup_in_counts_a_server_retransmit_without_redelivering_it() {
    let (mut c, _sink) = detached().await;

    let d = rel(1, gsb_protocol::op::base::ERROR, &[9, 0]);
    c.process_datagram(&d.clone());
    assert_eq!(c.stats.dup_in, 0, "the first delivery is not a duplicate");
    assert_eq!(c.pending.len(), 1, "and it was delivered");

    // The same seq again: the server retransmitted before our ACK landed.
    c.process_datagram(&d);
    assert_eq!(c.stats.dup_in, 1, "the retransmit is counted");
    assert_eq!(
        c.pending.len(),
        1,
        "and NOT delivered a second time: the control band runs exactly once"
    );

    c.process_datagram(&d);
    assert_eq!(c.stats.dup_in, 2, "every observed retransmit counts");
    assert_eq!(c.pending.len(), 1);
    assert_eq!(
        c.stats.oob_dropped, 0,
        "a duplicate is behind the window, not ahead of it"
    );
}

/// `oob_dropped` counts inbound REL frames dropped because the
/// out-of-order window was full — the only place this client discards
/// something the server successfully delivered.
///
/// The boundary is the assertion: exactly `OOB_CAP` frames fit, and the
/// next one is the first drop. An off-by-one here is the difference
/// between "the window is the bound" and "the window is one short of
/// it", and the counter is the only visible symptom either way.
#[tokio::test]
async fn oob_dropped_counts_only_past_the_reorder_window() {
    let (mut c, _sink) = detached().await;

    // Seqs 2..=OOB_CAP+1 all sit AHEAD of the expected 1, so they buffer.
    for seq in 2..=(OOB_CAP as u32 + 1) {
        c.process_datagram(&rel(seq, gsb_protocol::op::base::ERROR, &[1]));
    }
    assert_eq!(c.in_oob.len(), OOB_CAP, "the window is exactly full");
    assert_eq!(
        c.stats.oob_dropped, 0,
        "a full window is not an overflow: nothing was dropped yet"
    );

    // One more: no room, so it is dropped and counted.
    c.process_datagram(&rel(
        OOB_CAP as u32 + 2,
        gsb_protocol::op::base::ERROR,
        &[1],
    ));
    assert_eq!(
        c.stats.oob_dropped, 1,
        "the first frame past the window is the first drop"
    );
    assert_eq!(c.in_oob.len(), OOB_CAP, "and the window did not grow");
    assert!(
        c.pending.is_empty(),
        "nothing is deliverable while seq 1 is still missing"
    );
}

/// `retrans_out` counts THIS client's own re-sends: an un-ACKed control
/// frame whose RTO has passed.
///
/// Paired with `dup_in` it splits the two legs of a lossy link
/// (`retrans_out` = our frames were lost going out, `dup_in` = theirs
/// coming in), which is the only reason the client keeps both. The
/// second half of the test is that the RTO is respected — a counter that
/// incremented on every pass would report a re-send storm that never
/// happened, on a healthy link.
#[tokio::test]
async fn retrans_out_counts_re_sends_and_respects_the_rto() {
    let (mut c, _sink) = detached().await;

    c.send_frame(gsb_protocol::op::base::ERROR, Bytes::from_static(&[9, 0]))
        .await
        .expect("send");
    assert_eq!(c.rel.len(), 1, "one frame outstanding");

    // The RTO has not passed: nothing is re-sent.
    c.retransmit_pass();
    assert_eq!(
        c.stats.retrans_out, 0,
        "a frame within its RTO must not be re-sent"
    );

    // Rewind the frame's send stamp past its timer (the clock is the
    // client's own field, so this needs no sleep).
    let rto = c.rto();
    c.rel.front_mut().expect("outstanding").sent -= rto;
    c.retransmit_pass();
    assert_eq!(c.stats.retrans_out, 1, "the expired frame was re-sent");
    assert_eq!(
        c.rel.len(),
        1,
        "and is still outstanding: a re-send is not a delivery"
    );

    assert_eq!(c.rto(), rto * 2, "and its timer doubled");

    // The stamp was refreshed by the pass, so the next one is quiet again.
    c.retransmit_pass();
    assert_eq!(
        c.stats.retrans_out, 1,
        "the re-send restarts the RTO; it must not fire every pass"
    );
}

/// `gave_up` counts the frames abandoned when the reliable band DIES —
/// all of them at once, because a frame is never abandoned on its own
/// age (abandoning one silently wedges the direction; the band as a
/// whole dies instead).
///
/// So `gave_up` is not "how many retransmits failed" but "how much was
/// still owed when we stopped trying", and a non-zero value means the
/// session is over — `is_established` is false beside it. Both halves
/// are asserted: counting without flipping the flag would report a dead
/// band to the operator while the caller kept using it.
#[tokio::test]
async fn gave_up_counts_everything_outstanding_when_the_band_dies() {
    let (mut c, _sink) = detached().await;

    for _ in 0..3 {
        c.send_frame(gsb_protocol::op::base::ERROR, Bytes::from_static(&[9, 0]))
            .await
            .expect("send");
    }
    assert_eq!(c.rel.len(), 3, "three frames outstanding");

    // No ACK has advanced for longer than the fatal window.
    c.rel.rewind_progress(REL_NO_ACK_FATAL);
    c.retransmit_pass();

    assert_eq!(
        c.stats.gave_up, 3,
        "every outstanding frame is abandoned together: the BAND dies, \
         not one frame"
    );
    assert!(
        !c.is_established(),
        "a counted give-up must come with the session flipping to dead"
    );
    assert!(c.rel.is_empty(), "nothing is still owed");
}

/// A client with nothing outstanding never dies of silence: the fatal
/// clock measures unanswered WORK, not idleness.
///
/// The negative side of `gave_up`. A rule that read the wall clock
/// instead of the outstanding queue would kill every quiet session after
/// the fatal window and report a fleet of give-ups on an idle server.
#[tokio::test]
async fn an_idle_client_never_gives_up() {
    let (mut c, _sink) = detached().await;

    c.rel.rewind_progress(REL_NO_ACK_FATAL * 2);
    c.retransmit_pass();

    assert_eq!(
        c.stats.gave_up, 0,
        "nothing was owed, so nothing was abandoned"
    );
    assert!(
        c.is_established(),
        "an idle session stays alive however long it has been quiet"
    );
}

/// The liveness clock starts when a control frame becomes outstanding,
/// not at the last RTO pass that happened to run while the queue was
/// empty.
///
/// A client busy on the game band never takes an RTO pass: every read
/// returns a datagram (a fragmented snapshot stream keeps the socket
/// full), so the pass that refreshes `ack_progress` on an empty queue
/// never runs. Its first control frame after a long quiet spell — the
/// LEAVE at the end of a session — then found a clock stamped at the
/// last ACK (the JOIN, seconds ago) and the band was declared dead on
/// the very first pass, with nothing actually overdue.
#[tokio::test]
async fn a_control_frame_after_a_quiet_spell_starts_a_fresh_liveness_clock() {
    let (mut c, _sink) = detached().await;

    // The last ACK progress was long ago and no pass has run since.
    c.rel.rewind_progress(REL_NO_ACK_FATAL * 2);
    c.send_frame(gsb_protocol::op::base::ERROR, Bytes::from_static(&[9, 0]))
        .await
        .expect("send");
    c.retransmit_pass();

    assert!(
        c.is_established(),
        "a frame outstanding for microseconds is not a dead band"
    );
    assert_eq!(c.stats.gave_up, 0);
}

/// A busy game band must not starve the control band's retransmit.
///
/// The pass used to run only when a read timed out — i.e. after a full
/// RTO of silence. A client receiving a steady snapshot stream (and a
/// fragmented one is steadier still) never saw one, so a lost control
/// frame (a LEAVE, say) was never re-sent while the stream lasted.
///
/// The busy band is exact here (BACKLOG F25): the RAW frames are queued
/// on the socket BEFORE each read, so no read waits at all — let alone a
/// whole RTO — and the outstanding frame is made overdue by rewinding
/// its clock, not by sleeping. (A peer streaming every 5 ms against a
/// 300 ms wall-clock window proved the same only while the machine kept
/// up with both.) Every read that returns a datagram must run the pass.
#[tokio::test]
async fn a_busy_game_band_does_not_starve_the_retransmit() {
    const READS: u64 = 3;
    let (mut c, sink) = detached().await;
    c.send_frame(gsb_protocol::op::base::ERROR, Bytes::from_static(&[9, 0]))
        .await
        .expect("send");
    // The peer never ACKs, but the game band is never idle.
    let me = c.local_addr().expect("bound");
    let raw = encode_raw(&FrameBody::new(1000, Bytes::from_static(&[1])));
    for _ in 0..READS {
        sink.send_to(&raw, me).await.expect("queue a RAW frame");
    }
    for read in 1..=READS {
        // Overdue by its CURRENT timer: each re-send doubled it.
        let rto = c.rto();
        let outstanding = c.rel.front_mut().expect("still un-ACKed");
        outstanding.sent -= rto;
        let got = c
            .recv_frame(Duration::from_secs(5))
            .await
            .expect("recv")
            .expect("a queued RAW frame, without waiting");
        assert_eq!(got.op, 1000, "the game band's frame");
        assert_eq!(
            c.stats.retrans_out, read,
            "the overdue control frame must be re-sent on every busy read"
        );
    }
}

mod migrate;
mod report;
mod rtt;
