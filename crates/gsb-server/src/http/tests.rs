//! `POST /rooms/open` builds the room this SERVER uses (BACKLOG F8): the
//! request the registry receives is `Config::room_config` of the id —
//! every field — with only the query's `tick_hz` laid over it.

use std::time::Duration;

use tokio::sync::{mpsc, watch};

use gsb_core::channel::channel;
use gsb_core::id::RoomId;
use gsb_core::metrics::MetricReport;
use gsb_core::registry::{RegistryMsg, RoomStatus};
use gsb_core::room::RoomConfig;

use crate::Config;
use crate::http::*;

/// A config whose every room-level key is off the core's default.
fn tuned() -> Config {
    Config {
        tick_hz: 60.0,
        room_control: 64,
        conn_action: 128,
        max_snapshot_bytes: 1200,
        keepalive_hz: 2.0,
        max_players: Some(12),
        max_idle_input_secs: Some(40),
        max_detach_hold: Some(Duration::from_secs(3)),
        ..Config::default()
    }
}

/// The surface over `cfg`, its registry faked: every create is answered
/// running, after its config is handed to the returned receiver.
fn surface(cfg: &Config) -> (OpsHttp, mpsc::Receiver<RoomConfig>) {
    let (registry, mut inbox) = channel::<RegistryMsg>(8);
    let (asked_tx, asked) = mpsc::channel(8);
    tokio::spawn(async move {
        while let Some(msg) = inbox.recv().await {
            if let RegistryMsg::CreateRoom { config, reply } = msg {
                let _ = asked_tx.send(config).await;
                let _ = reply.send(Ok(RoomStatus::Running { members: 0 }));
            }
        }
    });
    let (rooms, _) = mpsc::channel(8);
    let (_, reports) = watch::channel(MetricReport::initial_stale(Duration::from_secs(1)));
    let ops = OpsHttp {
        reports,
        registry,
        rooms,
        period: Duration::from_secs(1),
        room_template: cfg.room_template(),
    };
    (ops, asked)
}

/// Route `head`; returns the status code and the config the registry was
/// asked for (`None` when the request never reached it).
async fn open(
    ops: &OpsHttp,
    asked: &mut mpsc::Receiver<RoomConfig>,
    head: &str,
) -> (u16, Option<RoomConfig>) {
    let response = route(head, ops).await;
    (response.status, asked.try_recv().ok())
}

/// Without `tick_hz` the registry is asked for exactly the boot room of
/// that id; with it, the same room at the requested rate.
#[tokio::test]
async fn the_open_asks_for_the_server_room() {
    let cfg = tuned();
    let (ops, mut asked_rx) = surface(&cfg);

    let (status, asked) = open(&ops, &mut asked_rx, "POST /rooms/open?id=5 HTTP/1.1").await;
    assert_eq!(status, 200);
    let asked = asked.expect("the registry was asked");
    assert_eq!(asked.id, RoomId(5));
    assert_eq!(asked, cfg.room_config(5), "every field of the boot room");
    // The fixture is off the core's default in every mapped field, so the
    // equality above cannot hold by accident.
    let d = RoomConfig::default();
    assert_ne!(asked.tick_hz, d.tick_hz);
    assert_ne!(asked.control_capacity, d.control_capacity);
    assert_ne!(asked.action_capacity, d.action_capacity);
    assert_ne!(asked.max_snapshot_bytes, d.max_snapshot_bytes);
    assert_ne!(asked.keepalive_hz, d.keepalive_hz);
    assert_ne!(asked.max_players, d.max_players);
    assert_ne!(asked.max_idle_input_secs, d.max_idle_input_secs);
    assert_ne!(asked.max_detach_hold, d.max_detach_hold);

    let (status, asked) = open(
        &ops,
        &mut asked_rx,
        "POST /rooms/open?id=5&tick_hz=15 HTTP/1.1",
    )
    .await;
    assert_eq!(status, 200);
    let want = RoomConfig {
        tick_hz: 15.0,
        ..cfg.room_config(5)
    };
    assert_eq!(asked, Some(want), "the rate is the one override");
}

/// An invalid `tick_hz` is refused before the registry is asked.
#[tokio::test]
async fn an_invalid_rate_never_reaches_the_registry() {
    let (ops, mut asked_rx) = surface(&tuned());
    for bad in ["0", "-1", "nan", "inf", "x", ""] {
        let head = format!("POST /rooms/open?id=5&tick_hz={bad} HTTP/1.1");
        let (status, asked) = open(&ops, &mut asked_rx, &head).await;
        assert_eq!(status, 400, "tick_hz={bad}");
        assert!(asked.is_none(), "tick_hz={bad} reached the registry");
    }
}

/// B18: an id with a `[rooms.<id>]` section opens as THAT room (the
/// override laid over the server's room), other ids as the server's
/// room; a query `tick_hz` goes on top of the override — the request is
/// the most specific word.
#[tokio::test]
async fn the_open_takes_the_id_override_with_the_query_rate_on_top() {
    let mut cfg = tuned();
    cfg.rooms.insert(
        5,
        crate::RoomOverride {
            tick_hz: Some(30.0),
            max_players: Some(3),
            ..Default::default()
        },
    );
    let (ops, mut asked_rx) = surface(&cfg);

    let (status, asked) = open(&ops, &mut asked_rx, "POST /rooms/open?id=5 HTTP/1.1").await;
    assert_eq!(status, 200);
    let want = RoomConfig {
        tick_hz: 30.0,
        max_players: Some(3),
        ..tuned().room_config(5)
    };
    assert_eq!(asked, Some(want.clone()), "the id's override");
    assert_eq!(cfg.room_config(5), want, "the same room as the boot path");

    let (_, asked) = open(&ops, &mut asked_rx, "POST /rooms/open?id=6 HTTP/1.1").await;
    assert_eq!(
        asked,
        Some(tuned().room_config(6)),
        "no override: the server's room"
    );

    let (status, asked) = open(
        &ops,
        &mut asked_rx,
        "POST /rooms/open?id=5&tick_hz=20 HTTP/1.1",
    )
    .await;
    assert_eq!(status, 200);
    let want = RoomConfig {
        tick_hz: 20.0,
        ..want
    };
    assert_eq!(asked, Some(want), "the query rate over the override's");
}

/// `/metrics` is the Prometheus pull exporter: the exposition with the
/// `prometheus` feature, a 404 naming the feature without it (never an
/// empty 200 a scraper would ingest as "no series").
#[tokio::test]
async fn the_metrics_path_serves_the_exposition_only_when_compiled_in() {
    let (ops, _) = surface(&Config::default());
    let response = route("GET /metrics HTTP/1.1", &ops).await;
    if cfg!(feature = "prometheus") {
        assert_eq!(response.status, 200);
        assert!(
            response
                .body
                .starts_with("# HELP gsb_metrics_dropped_total ")
        );
    } else {
        assert_eq!(response.status, 404);
        assert!(response.body.contains("cargo feature `prometheus`"));
    }
}
