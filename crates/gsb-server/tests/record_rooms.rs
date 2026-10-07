//! `[record]` (BACKLOG B170, feature `record`): a configured server
//! records the rooms the table selects — one log file per room
//! incarnation, on the registry's writer thread — and the file replays
//! against the same game to the same state hashes.

use gsb_server::{Config, RecordSection, ServerError};

/// `[record]` parses in every build; its keys are checked.
#[test]
fn the_record_table_parses_with_defaults_and_refuses_unknown_keys() {
    let path = std::env::temp_dir().join(format!("gsb-q6-record-key-{}.toml", std::process::id()));
    std::fs::write(&path, "[record]\ndir = \"/tmp/x\"\n").expect("write");
    let cfg = Config::from_file(&path).expect("parse");
    let r = cfg.record.expect("the table");
    assert_eq!(
        (r.dir.as_str(), r.rooms, r.game_version, r.buffer),
        ("/tmp/x", None, None, 4096)
    );
    std::fs::write(&path, "[record]\ndir = \"/tmp/x\"\nroom = [1]\n").expect("write");
    assert!(Config::from_file(&path).is_err(), "an unknown key");
    let _ = std::fs::remove_file(&path);
}

fn with_record(dir: &str) -> Config {
    Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        record: Some(RecordSection {
            dir: dir.into(),
            rooms: None,
            game_version: None,
            buffer: 4096,
        }),
        ..Default::default()
    }
}

/// Without the feature, a `[record]` table refuses startup by name.
#[cfg(not(feature = "record"))]
#[tokio::test]
async fn a_record_table_refuses_startup_without_the_feature() {
    let dir = std::env::temp_dir().join("gsb-q6-record-unbuilt");
    let err = gsb_server::start_server(with_record(&dir.to_string_lossy()))
        .await
        .err()
        .expect("startup refused");
    assert!(matches!(err, ServerError::RecordNotBuilt), "{err}");
    assert!(err.to_string().contains("`record` cargo feature"), "{err}");
}

/// A zero buffer refuses startup.
#[cfg(feature = "record")]
#[tokio::test]
async fn a_zero_record_buffer_refuses_startup() {
    let dir = std::env::temp_dir().join(format!("gsb-q6-record-zero-{}", std::process::id()));
    let mut cfg = with_record(&dir.to_string_lossy());
    if let Some(r) = &mut cfg.record {
        r.buffer = 0;
    }
    let err = gsb_server::start_server(cfg)
        .await
        .err()
        .expect("startup refused");
    assert!(matches!(err, ServerError::BadRecord(_)), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "record")]
mod recording {
    use std::collections::BTreeMap;
    use std::hash::{Hash, Hasher};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use gsb_client::session::{self, Credentials};
    use gsb_core::determinism::{InputLog, Replay, StateHasher, read_log};
    use gsb_core::id::{ConnectionId, PlayerId};
    use gsb_core::registry::{BuiltRoom, RoomFactory};
    use gsb_core::room::{Action, Admission, GameLogic, RoomLogic, TickCtx};
    use gsb_protocol::MessageTable;
    use gsb_server::{Config, GameModule, RecordSection, RegistryParts, RegistryTask, ServerError};
    use prost::Message;

    const OP_TIP: u16 = 0x7301;

    #[derive(Clone, PartialEq, prost::Message)]
    struct Tip {
        #[prost(uint64, tag = "1")]
        gold: u64,
    }

    /// The world: each player's tips and rolls.
    type World = BTreeMap<PlayerId, (u64, u64)>;

    /// An ECS-free game: tips add gold, the room's dice add a roll per tick
    /// to everyone; the state hash covers both.
    struct Tally {
        rolls: u64,
    }

    impl GameLogic<World> for Tally {
        type GroupKey = ();
        type Strip = ();

        fn snapshot_op(&self) -> u16 {
            0x7302
        }
        fn private_op(&self) -> u16 {
            0x7303
        }
        fn group_of(&self, _w: &World, _p: PlayerId) {}
        fn snapshot(
            &mut self,
            _w: &mut World,
            _c: &TickCtx,
            _g: &(),
            _b: &[gsb_core::shard::BorderRecord<()>],
            _o: &mut bytes::BytesMut,
        ) -> bool {
            false
        }
        fn on_join(&mut self, w: &mut World, c: ConnectionId) -> Admission {
            let player = PlayerId(1000 + c.0);
            w.insert(player, (0, 0));
            Admission {
                player,
                entity: c.0,
            }
        }
        fn on_leave(&mut self, w: &mut World, p: PlayerId) {
            w.remove(&p);
        }
        fn ingest(&mut self, w: &mut World, _c: &TickCtx, actions: &mut Vec<Action>) {
            for a in actions.drain(..) {
                if a.op == OP_TIP
                    && let (Ok(t), Some(g)) = (Tip::decode(&a.payload[..]), w.get_mut(&a.player))
                {
                    g.0 += t.gold;
                }
            }
        }
        fn update(&mut self, w: &mut World, ctx: &TickCtx) {
            let roll = ctx.rng.below(6);
            self.rolls += roll;
            for g in w.values_mut() {
                g.1 += roll;
            }
        }
        fn state_hash(&mut self, w: &mut World) -> Option<u64> {
            let mut h = StateHasher::new();
            for (p, g) in w.iter() {
                p.0.hash(&mut h);
                g.hash(&mut h);
            }
            self.rolls.hash(&mut h);
            Some(h.finish())
        }
    }

    impl RoomLogic<World> for Tally {}

    struct TallyModule;

    impl GameModule for TallyModule {
        fn name(&self) -> &'static str {
            "tally"
        }
        fn configure(&mut self, _raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
            Ok(())
        }
        fn register(&self, table: &mut MessageTable) {
            table.reg::<Tip>(OP_TIP);
        }
        fn spawn_registry(&self, parts: RegistryParts) -> RegistryTask {
            let factory: RoomFactory<World, (), (), ()> =
                Arc::new(|_id, _config| BuiltRoom::Single {
                    world: World::new(),
                    logic: Box::new(Tally { rolls: 0 }),
                });
            parts.spawn(factory)
        }
        fn describe(&self) -> String {
            "tally".into()
        }
    }

    /// The log at `path` once the writer thread finished it (it ends when the
    /// stopped room's recorder drops).
    async fn finished_log(path: &Path) -> InputLog {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last: Option<usize> = None;
        loop {
            if let Ok(log) = read_log(path) {
                let n = log.ticks.len();
                if last == Some(n) {
                    return log;
                }
                last = Some(n);
            }
            assert!(
                Instant::now() < deadline,
                "the log at {path:?} never settled"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn join(addr: std::net::SocketAddr, name: &str, room: u64) -> gsb_client::conn::Conn {
        let mut s = gsb_client::connect::tcp(addr).await.expect("connect");
        session::auth_and_join(
            &mut s,
            &Credentials::named(name),
            room,
            Duration::from_secs(10),
            |_| {},
        )
        .await
        .unwrap_or_else(|e| panic!("join failed: {e}"));
        s
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_configured_server_records_the_selected_rooms_and_the_log_replays() {
        let dir: PathBuf =
            std::env::temp_dir().join(format!("gsb-q6-record-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config {
            bind: "127.0.0.1:0".into(),
            room_count: 2,
            record: Some(RecordSection {
                dir: dir.to_string_lossy().into_owned(),
                rooms: Some(vec![1]),
                game_version: Some("tally-7".into()),
                buffer: 4096,
            }),
            ..Default::default()
        };
        let handle = gsb_server::start_game_server(Box::new(TallyModule), cfg)
            .await
            .expect("the server starts with [record]");
        let mut alice = join(handle.addr, "alice", 1).await;
        let mut bob = join(handle.addr, "bob", 1).await;
        let _carol = join(handle.addr, "carol", 2).await;
        for gold in [3u64, 5, 7] {
            alice
                .send(OP_TIP, &Tip { gold }.encode_to_vec())
                .await
                .expect("send");
            bob.send(OP_TIP, &Tip { gold: 1 }.encode_to_vec())
                .await
                .expect("send");
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        handle.stop().await;

        let log = finished_log(&dir.join("room-1-0.gsblog")).await;
        assert!(
            !dir.join("room-2-0.gsblog").exists(),
            "room 2 is not selected"
        );
        assert_eq!(log.header.game, "tally-7");
        assert_eq!(log.header.room, 1);
        assert!(log.ticks.len() > 10, "{} ticks", log.ticks.len());
        let recorded = log.hashes();
        assert!(recorded.iter().all(Option::is_some), "every step hashed");

        let mut replay = Replay::new(Arc::new(log), World::new(), Box::new(Tally { rolls: 0 }));
        let report = replay.run();
        assert_eq!(report.hashes, recorded, "the replay walks the same states");
        assert_eq!(report.first_divergent, None);
        assert_eq!(report.counts.replay_diverged, 0);
        // The tips reached the logic, and the replay saw them.
        let world = replay.world();
        let tips: Vec<u64> = world.values().map(|g| g.0).collect();
        assert_eq!(tips, vec![15, 3], "alice's and bob's tips: {world:?}");
        assert!(
            world.values().all(|g| g.1 > 0),
            "the dice rolled: {world:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
