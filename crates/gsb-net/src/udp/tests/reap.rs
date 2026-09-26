//! A session whose connection actor is gone leaves the demux promptly
//! (BACKLOG B6) — with the idle sweep OFF and not one datagram from the
//! peer after its handshake. Observed from outside: the session's writer
//! finishes (the demux dropped the session's outbound sender, the last
//! one left) and the peer's address takes a NEW handshake (an
//! established address ignores a challenge request).

use super::*;

/// How long the reap may take: the writer's RTO wake plus a wake
/// datagram's round trip, with a wide margin for a loaded test host.
const BOUND: Duration = Duration::from_secs(1);

/// A session with its pump running, as the accept loop would leave it:
/// the actor's mailbox receiver and outbound sender are returned (the
/// test "is" the actor), the writer's handle too.
async fn session(
    eps: &mut mpsc::UnboundedReceiver<Endpoint>,
) -> (
    gsb_core::channel::Inbox<gsb_core::conn::ConnIn>,
    gsb_core::channel::Mailbox<gsb_core::channel::FrameBatch>,
    tokio::task::JoinHandle<()>,
) {
    let mut ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("the endpoint")
        .expect("the endpoint");
    let (in_tx, in_rx) = ep.take_inbox(16);
    let (out_tx, out_rx) = ep.take_outbox(16);
    let (_reader, writer) = ep.start_pump(
        ConnectionId(1),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    (in_rx, out_tx, writer)
}

/// The next datagram that is not a HELLO (a slow answer to an earlier
/// challenge retry may still be in flight), if one comes within `wait`.
async fn next_non_hello(raw: &UdpSocket, wait: Duration) -> Option<Vec<u8>> {
    let mut buf = [0u8; 256];
    loop {
        match tokio::time::timeout(wait, raw.recv_from(&mut buf)).await {
            Ok(Ok(_)) if buf[0] == KIND_HELLO => continue,
            Ok(Ok((n, _))) => return Some(buf[..n].to_vec()),
            _ => return None,
        }
    }
}

fn no_idle() -> UdpTransportConfig {
    UdpTransportConfig {
        idle_timeout: None,
        ..UdpTransportConfig::default()
    }
}

/// The actor exits: within the bound the session is gone — its writer
/// finishes, and the same address completes a fresh handshake.
#[tokio::test]
async fn an_exited_actor_frees_its_session_without_a_datagram() {
    let (listener, addr, mut eps, _accept) = bound_transport(no_idle()).await;
    let raw = UdpSocket::bind("127.0.0.1:0").await.expect("raw client");
    raw_handshake(&raw, addr, 0x5EED).await;
    let (in_rx, out_tx, writer) = session(&mut eps).await;

    // The actor exits: its mailbox and its outbound sender go.
    drop(in_rx);
    drop(out_tx);
    tokio::time::timeout(BOUND, writer)
        .await
        .expect("the demux let the session go (its writer finished)")
        .expect("the writer did not panic");

    // The address is free: a new handshake from it is a new session.
    raw_handshake(&raw, addr, 0x5EED + 1).await;
    let _second = session(&mut eps).await;
    listener.close();
}

/// The actor's last control frame (its close notice) is delivered
/// first: while the peer has not ACKed it, the session stays (the demux
/// is what carries the ACK to the writer); the ACK ends it.
#[tokio::test]
async fn the_actor_s_last_notice_is_acked_before_the_session_goes() {
    let (listener, addr, mut eps, _accept) = bound_transport(no_idle()).await;
    let raw = UdpSocket::bind("127.0.0.1:0").await.expect("raw client");
    raw_handshake(&raw, addr, 0xACE).await;
    let (in_rx, out_tx, mut writer) = session(&mut eps).await;

    // The notice, then the actor's exit.
    let notice = FrameBody::new(gsb_protocol::op::base::ERROR, Bytes::from_static(b"bye"));
    out_tx.send(vec![notice]).await.expect("to the writer");
    drop(in_rx);
    drop(out_tx);

    // Not ACKed: the writer retransmits and the session stays.
    let mut buf = [0u8; 256];
    let (n, _) = tokio::time::timeout(Duration::from_secs(1), raw.recv_from(&mut buf))
        .await
        .expect("the notice")
        .expect("recv");
    assert_eq!(buf[0], KIND_REL, "the notice rides the reliable band");
    let seq = u32::from_le_bytes(buf[1..5].try_into().unwrap());
    assert!(n > 7);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut writer)
            .await
            .is_err(),
        "an unACKed notice keeps the session"
    );

    // The ACK: the session goes.
    raw.send_to(&encode_ack(seq + 1), addr).await.expect("ack");
    tokio::time::timeout(BOUND, writer)
        .await
        .expect("the ACKed session was let go")
        .expect("the writer did not panic");
    listener.close();
}

/// The room keeps sending until it has processed the detach, and by
/// then the address may carry a NEW session: the old writer takes those
/// frames off its channel and puts none of them on the wire.
#[tokio::test]
async fn an_ended_session_s_late_frames_never_reach_the_address() {
    let (listener, addr, mut eps, _accept) = bound_transport(no_idle()).await;
    let raw = UdpSocket::bind("127.0.0.1:0").await.expect("raw client");
    raw_handshake(&raw, addr, 0xB0B).await;
    let (in_rx, out_tx, _writer) = session(&mut eps).await;
    let room = out_tx.clone();
    drop(in_rx);
    drop(out_tx);

    // The address frees (a challenge request is answered again), then
    // takes a new session.
    let mut buf = [0u8; 256];
    let started = std::time::Instant::now();
    let cookie = loop {
        assert!(started.elapsed() < BOUND, "the address was never freed");
        raw.send_to(&encode_hello(0xB0B + 1, 0), addr)
            .await
            .unwrap();
        if let Ok(Ok((18, _))) =
            tokio::time::timeout(Duration::from_millis(50), raw.recv_from(&mut buf)).await
        {
            break u64::from_le_bytes(buf[9..17].try_into().unwrap());
        }
    };
    raw.send_to(&encode_hello(0xB0B + 1, cookie), addr)
        .await
        .unwrap();
    let accept = next_non_hello(&raw, Duration::from_secs(1)).await;
    assert_eq!(accept.as_deref(), Some(&[KIND_ACK, 1, 0, 0, 0][..]));
    let _second = session(&mut eps).await;

    // The old room's frame goes nowhere.
    let late = FrameBody::new(1000, Bytes::from_static(b"stale snapshot"));
    room.send(vec![late]).await.expect("the old writer drains");
    assert_eq!(
        next_non_hello(&raw, Duration::from_millis(300)).await,
        None,
        "an ended session's frame reached the address's new session"
    );
    listener.close();
}
