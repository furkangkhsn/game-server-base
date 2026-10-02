//! The congestion response measured (`#[ignore]`: seconds of real time,
//! run by hand with `--ignored --nocapture`): real doors, reporting
//! `UdpClient`s behind the [`relay`] bottleneck, a room per session
//! sending a 3000-byte snapshot every 33 ms (three FRAG datagrams) and a
//! control frame every 200 ms, each stamped with its send time. Per run
//! and session: the game band's age at the client (send → receive), the
//! messages and bytes delivered, the control band's age, the link's and
//! the writer's drops by name.

use super::*;

use relay::{Link, Relay};
use tokio::time::Instant;

const SNAPSHOT: usize = 3000;
const TICK: Duration = Duration::from_millis(33);

/// What one session's client saw after the warm-up.
#[derive(Debug, Default)]
struct Seen {
    game_us: Vec<u64>,
    control_us: Vec<u64>,
    game_bytes: u64,
    stats: Option<UdpClientStats>,
}

fn stamp(epoch: std::time::Instant, op: u16, len: usize) -> FrameBody {
    let mut p = vec![0u8; len];
    p[..8].copy_from_slice(&(epoch.elapsed().as_micros() as u64).to_le_bytes());
    FrameBody::new(op, Bytes::from(p))
}

/// The room of one session: a snapshot a tick, a control frame every
/// sixth; frames its full outbound channel refuses are counted.
async fn room(out: Mailbox<FrameBatch>, epoch: std::time::Instant, until: Instant) -> u64 {
    let mut tick = tokio::time::interval(TICK);
    let (mut seq, mut refused) = (0u64, 0u64);
    while tick.tick().await < until {
        seq += 1;
        let mut batch = vec![stamp(epoch, 1004, SNAPSHOT)];
        if seq % 6 == 0 {
            batch.push(stamp(epoch, op::base::HEARTBEAT_ACK, 16));
        }
        refused += u64::from(out.try_send(batch).is_err());
    }
    refused
}

async fn client(
    addr: SocketAddr,
    epoch: std::time::Instant,
    warm: Instant,
    until: Instant,
) -> Seen {
    let mut c = UdpClient::connect(addr).await.expect("connect");
    let mut seen = Seen::default();
    while Instant::now() < until {
        let Ok(Some(f)) = c.recv_frame(Duration::from_millis(50)).await else {
            continue;
        };
        if Instant::now() < warm || f.payload.len() < 8 {
            continue;
        }
        let sent = u64::from_le_bytes(f.payload[..8].try_into().unwrap());
        let age = (epoch.elapsed().as_micros() as u64).saturating_sub(sent);
        match f.op {
            1004 => {
                seen.game_us.push(age);
                seen.game_bytes += f.payload.len() as u64;
            }
            _ => seen.control_us.push(age),
        }
    }
    seen.stats = Some(c.stats);
    seen
}

fn pct(v: &mut [u64], p: usize) -> u64 {
    v.sort_unstable();
    v.get((v.len() * p / 100).min(v.len().saturating_sub(1)))
        .copied()
        .unwrap_or(0)
        / 1000
}

/// One run: `n` sessions started `stagger` apart, `secs` long, the last
/// `window` seconds measured.
async fn run(
    name: &str,
    mode: UdpCongestion,
    n: usize,
    link: Link,
    stagger: u64,
    secs: u64,
    window: u64,
) {
    let (metrics_tx, mut metrics_rx) = mpsc::channel(4096);
    let cfg = UdpTransportConfig {
        metrics: Some(metrics_tx),
        congestion: mode,
        // The clients send nothing of their own: no idle sweep.
        idle_timeout: None,
        ..Default::default()
    };
    let (listener, addr, mut eps, _accept) = bound_transport(cfg).await;
    let relay = Relay::start(addr, n, link).await;
    let epoch = std::time::Instant::now();
    let t0 = Instant::now();
    let (until, warm) = (
        t0 + Duration::from_secs(secs),
        t0 + Duration::from_secs(secs - window),
    );
    let (mut clients, mut rooms, mut pumps, mut keep) = (vec![], vec![], vec![], vec![]);
    for i in 0..n {
        tokio::time::sleep_until(t0 + Duration::from_secs(stagger * i as u64)).await;
        clients.push(tokio::spawn(client(relay.addrs[i], epoch, warm, until)));
        let mut ep = eps.recv().await.expect("endpoint");
        let (in_tx, in_rx) = ep.take_inbox(64);
        let (out_tx, out_rx) = ep.take_outbox(64);
        keep.push(in_rx);
        pumps.push(
            ep.start_pump(ConnectionId(i as u64), in_tx, out_rx, Default::default())
                .1,
        );
        rooms.push(tokio::spawn(room(out_tx, epoch, until)));
    }
    let mut refused = 0;
    for r in rooms {
        refused += r.await.unwrap();
    }
    let mut seen = Vec::new();
    for c in clients {
        seen.push(c.await.unwrap());
    }
    listener.close();
    for p in pumps {
        let _ = tokio::time::timeout(Duration::from_secs(5), p).await;
    }
    let link_stats = relay.finish().await;
    let mut t = TransportCounters::default();
    while let Ok(ev) = metrics_rx.try_recv() {
        if let MetricsEvent::Transport(d) = ev {
            t.add(&d);
        }
    }
    let w = window as f64;
    let rates: Vec<f64> = seen
        .iter()
        .map(|s| s.game_bytes as f64 / w / 1000.0)
        .collect();
    let jain =
        rates.iter().sum::<f64>().powi(2) / (n as f64 * rates.iter().map(|r| r * r).sum::<f64>());
    for (i, s) in seen.iter_mut().enumerate() {
        let st = s.stats.as_ref().unwrap();
        println!(
            "{name:9} {mode:5?} s{i} msg/s {:5.1} KB/s {:6.1} game ms p50 {:4} p95 {:4} max {:4} | ctl ms p50 {:4} p95 {:4} max {:4} | link drops {:5} frag incomplete {:4}",
            s.game_us.len() as f64 / w,
            rates[i],
            pct(&mut s.game_us, 50),
            pct(&mut s.game_us, 95),
            pct(&mut s.game_us, 100),
            pct(&mut s.control_us, 50),
            pct(&mut s.control_us, 95),
            pct(&mut s.control_us, 100),
            link_stats.dropped[i],
            st.frag_dropped_incomplete,
        );
    }
    println!(
        "{name:9} {mode:5?} total: jain {jain:.3} | queued_paced {} dropped_paced {} unsent_paced {} episodes {} cuts {} | retransmits {} probes {} | link delivered {} B, room refused {refused}",
        t.udp_game_frames_queued_paced,
        t.udp_game_frames_dropped_paced,
        t.udp_game_frames_unsent_paced,
        t.udp_game_paced_episodes,
        t.udp_game_paced_rate_cuts,
        t.udp_control_retransmits_timeout,
        t.udp_game_probes_sent,
        link_stats.delivered_bytes.iter().sum::<u64>(),
    );
}

/// The scenario table of DESIGN §6 "Tıkanıklık tepkisi".
#[ignore = "a measurement: seconds of real time; run by hand"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn measure_the_congestion_response() {
    let ms = Duration::from_millis;
    let deep = Link {
        rate: 60_000.0,
        buffer: 30_000,
        delay: ms(20),
    };
    let shallow = Link {
        rate: 60_000.0,
        buffer: 6_000,
        delay: ms(20),
    };
    let shared = Link {
        rate: 180_000.0,
        buffer: 90_000,
        delay: ms(20),
    };
    let open = Link {
        rate: 10_000_000.0,
        buffer: 1 << 20,
        delay: ms(20),
    };
    for mode in [UdpCongestion::Off, UdpCongestion::Pace] {
        run("deep", mode, 1, deep, 0, 14, 8).await;
        run("shallow", mode, 1, shallow, 0, 14, 8).await;
        run("shared4", mode, 4, shared, 1, 40, 15).await;
        run("nobneck", mode, 1, open, 0, 10, 6).await;
    }
}
