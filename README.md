# gsb: game-server-base

A Rust game server foundation with a **lock-free, select-free, pure channel-based actor**
architecture for MOBA / MMORPG projects.

> Target: a foundation scalable to 100k+ concurrent connections, with a
> swappable transport layer, and game logic isolated in a single crate.

## Architecture summary

- **All actors only `recv()` from their mailbox.** No `tokio::select!`, no
  locks (`Mutex`/`RwLock`), no parking waits. This rule **is enforced at
  build time** by the `gsb-lint` build-helper crate.
- **Each connection has 3 tasks:** reader pump + connection actor + writer pump.
- **Single global ticker** (single task, `broadcast` channel) + **1 task per room:**
  room actor (sole owner of the world). The *only* await of the room actor is
  the global tick `recv()`; the tick body is fully synchronous. The room rate
  must be an exact divisor of the global rate (60 Hz global / room 15 Hz → step every 4th tick).
- **The room tick has 5 phases:** CONTROL (drain join/leave/shutdown) → READ
  (`try_recv` action channels per connection) → CONVERT (action →
  component) → SYSTEMS (game systems) → BROADCAST (full
  world snapshot per group, encoded once, shared with members by reference;
  single batch + flush per connection).
- **Input isolation:** each connection has its own `Action` channel;
  the connection actor forwards incoming game ops to the room via `try_send`:
  if the channel is full, the input is dropped (that player's input, that player's isolation).
- **Frame-rate independence:** simulation advances with `dt = real elapsed time`;
  missed ticks are compensated with a single catch-up step
  (upper bound: 4 periods). Same distance in the same real time at both 15 Hz
  and 100 Hz (`gsb-demo/tests/frame_independence.rs`).
- **ECS:** `bevy_ecs` (standalone). The room actor exclusively
  holds a `World`; core is completely ECS-free via the `W` generic over the world type.
- **Protocol:** protobuf (`prost` / `Google.Protobuf` in Unity).
  `[u32 LE length][u16 LE opcode][protobuf payload]`.
- **Transport:** five doors behind the `Transport`/`Listener`/
  `Endpoint` abstraction touching zero lines of actor code: plain TCP (default),
  TLS-TCP (rustls), rUDP (`transport = "udp"`; stateless cookie
  handshake: cookie rotates in 10 s slots, captured proof
  expires within 10-20 s; lost handshake datagrams are re-sent until the
  server's accept (`ACK{1}`) arrives — `connect` returns only for a session
  the server holds, and gives up cleanly after 5 s; reliable control band `REL`: if there is no ACK
  progress for 5 s, the band is declared dead and the SESSION
  TERMINATES, no silent surrender; loss-tolerant game band `RAW`; over-MTU game frames are split into
  `FRAG` datagrams and reassembled by the client — at most 16 fragments,
  bounded client memory),
  QUIC (quinn; single bi-stream + length-prefix = TCP semantics), and
  WebSocket (RFC 6455; each binary message is a game frame). A mixed door list
  serving the same rooms is configured via the `[[listeners]]` table.

## Crate map

| Crate | Task |
|---|---|
| `gsb-lint` | Build-helper: scanning forbidden patterns (`tokio::select`, `Mutex`, …) |
| `gsb-protocol` | Framing/opcode/`MessageTable` + base protobuf messages |
| `gsb-ecs` | `System` trait, `SystemRunner` |
| `gsb-core` | IDs, channels, global ticker, registry actor, room actor (5-phase tick), connection actor |
| `gsb-net` | `Transport`/`Listener`/`Endpoint` + pump tasks + default TCP |
| `gsb-client` | **The client building block**: the stream wire (`[u32 LE len][u16 LE op][payload]`, a cancel-safe reader with a frame-size guard, a writer), one `Conn` over TCP / TLS / the QUIC bi-stream / WebSocket (RFC 6455, one frame per binary message; the close code on `Conn::ws_close`) / rUDP (`gsb-net`'s client half), the base frames, and the session steps (AUTH with or without a ticket, JOIN, HEARTBEAT, LEAVE, the resume key) as plain async functions — an `ERROR` frame comes back as a typed `ServerError` (`ErrorCode`, raw number, message). Game payloads stay opaque (`Bytes` + opcode); no reconnect policy. Used by `gsb-loadgen`, the example client and the server suites — see `docs/DESIGN.md` §5.7 |
| `gsb-kit` | **Pluggable game components, generic over the game**: the visibility strategies (open, AOI, team fog, PVS) and sharded composites (spatial, and team fog over a sharded map via a registry relay) as complete `GameLogic` rooms, the delta engines (per cell for the spatial rooms, per group for the team room's opt-in delta mode), park/resume, the input seq/ack rule, wire identity, the snapshot/`Private` envelopes (own `kit.proto`), and presets (`Grid2`, `VisionGrid2`, `VisionGrid3`, `ConvexSectors2`, `GridPartition2`) read through the `Planar`/`Spatial` accessors. Depends on no game — see `docs/KIT-ARCHITECTURE.md` |
| `gsb-demo` | **The example game**: 2D components, movement, the demo wire protocol (`game.proto`, a typed mirror of the kit envelope), input/bot/RPC/economy; implements the kit's seams and instantiates its rooms (constructors: `gsb_demo::prelude`) |
| `gsb-demo-arena` | **Validation demo: a 3D team arena** built on `gsb-kit`'s public API only (no kit or core change): `Pos3` (`Spatial`), its own 3D movement, a centimetre-quantized record codec, round-robin teams over three sides, **3D team fog of war** (`TeamRoom` in delta mode, units sent at 15 Hz through the kit's opt-in per-record send rate + `VisionGrid3` — height counts), its own `arena.proto` (typed mirror of the kit envelope). Hosted by the server as `game = "arena"` (settings in `[arena]`); verified through the real room actor (`docs/KIT-ARCHITECTURE.md` §10 "Faz 3 sonucu") and end to end over TCP/TLS (`gsb-server/tests/arena_e2e.rs`) |
| `gsb-demo-mmo` | **Validation demo: a 3D MMO world** — the closing check, built on `gsb-kit`'s public API only: `Pos3` with a ground-plane `Planar` (`[x, z]`), a decimetre-quantized record codec (position + kind + hit points), mobs spawned and despawned by game code that migrate without any speed component, flyers, a logout timer (hold, then release the slot; an optional logout bot), **a ground-plane grid AOI over a sharded world** (`ShardedSpatialRoom` + `Grid2` + `GridPartition2` with corners, 2×2 shards — height ignored), its own `mmo.proto`. Hosted by the server as `game = "mmo"` (settings in `[mmo]`; every room is a whole sharded world); verified through four real shard actors and end to end under the real registry (`gsb-server/tests/mmo_*.rs`) (`docs/KIT-ARCHITECTURE.md` §10 "Faz 4 sonucu"; the kit design findings it recorded were fixed in "Faz 5 sonucu") |
| `gsb-demo-war` | **Validation demo: "Cephe", a three-faction war** built on `gsb-kit`'s public API only: `Pos3` with a ground-plane `Planar`, a decimetre-quantized record carrying the unit's faction, watchtowers of every faction in every region (vision sources), capture points that become their taker's unit, saved characters by identity (a hashed faction for the rest), a `Welcome` naming the faction, melee across shard seams through remote effects, **team fog of war over a sharded map** (`ShardedTeamRoom` + `VisionGrid2` + `GridPartition2`, 2×2 shards, delta mode — allies map-wide, an enemy only while a unit of the faction sees it), its own `war.proto`; it rides the kit's opt-in record run (A31: a 6-byte packed unit body, records back to back in one field — about 54% less bandwidth than protobuf entries). Hosted as `game = "war"` (settings in `[war]`); verified through a live registry and four shard actors and end to end over TCP (`gsb-server/tests/war_e2e.rs`) (`docs/KIT-ARCHITECTURE.md` §10 "W2 sonucu") |
| `gsb-server` | Composition root: config, startup, the game modules (`game = "demo" | "arena" | "mmo" | "war"`, one cargo feature each), `gsb-server` binary + client example + `gsb-loadgen` load generator |

## Quick start

Requirements: Rust **1.95.0** (pinned via `rust-toolchain.toml`; this is also the
MSRV), **nothing else**. Having `protoc` installed on the system is not required:
proto build scripts (`gsb-protocol`, `gsb-kit`, `gsb-demo`, `gsb-demo-arena`,
`gsb-demo-mmo`, `gsb-demo-war`)
explicitly provide the embedded binary of `protoc-bin-vendored` to `prost-build`
(`Config::protoc_executable`), which takes precedence over searching
`PROTOC`/`PATH`. On an exotic target where the embedded binary is not found, the build script
prints a `cargo:warning` and falls back to the old behavior (`PROTOC`, then `PATH`);
there, system `protoc` is required. CI: `.github/workflows/ci.yml` (fmt ·
clippy `-D warnings` · test · rustdoc `-D warnings` · the Autobahn RFC 6455 fuzzing client against the
WebSocket door, `docs/SECURITY.md` §3.7).

```sh
# 1161 tests: framing, lint, ticker/room tick, RPC (single room + shard, the rpc_shard
# suite), ticket/control plane, READ fairness (rotating cursor), supervision (panicking
# room/shard), table pruning (epoch/tombstone TTL, metric retirement), reconnect
# (detach/resume/bot handover, PlayerId continuity), trait unification (GameLogic +
# sharded keepalive + shard RPC), security (TLS transport, auth rate limit, pre-auth
# caps), multi-listener (mixed transports on the same map: TCP/TLS/rUDP/QUIC/WS),
# WebSocket RFC 6455 conformance at the reader (fragmentation, framing, close frames),
# border-delta exchange, ops surface (/metrics, /healthz, admin API), metrics export (Prometheus +
# OTLP push), visibility (delta
# AOI, PVS, team fog, sharded), frame independence, identity invariant, publishability,
# e2e, metric flow, load smoke, wire contract (RPC envelope bytes, retired opcodes),
# ERROR code enumeration, protocol version handshake, session lifecycle (idle window +
# byte-granular write stall, server-close reasons) and heartbeat-ACK throttling, AFK
# signal (input-idle clock + default-off ceiling), sharded report folding (per-field
# fold rule), kit/demo layering (the kit's manifest names no game crate), kit envelope
# (field numbers frozen; kit and demo typed mirror pinned to identical bytes), kit seams
# on a fixture game (single WireId minter, codec/grid wire pins, the kit-owned change
# window in every room, more than two teams / sixteen sectors, non-adjacent and
# Speed-less migration, despawns by game code, request forwarding in every room,
# ground-plane 3D presets, 3D team fog, arrivals in a lent cell, the 8-neighbourhood
# grid, Planar's unit check, the disconnect policy and veto in every room, team chosen
# at the spawn, required frame opcodes), demo record values through the kit rooms,
# the 3D arena through the room actor (three-team fog, height, shared vision, movement,
# input ack, arena mirror bytes), the 3D MMO through four shard actors (ground-plane AOI, cross-seam
# combat — a remote effect applied by the target's owner, once, in a fixed order,
# with kill credit and the logout veto seeing the fight,
# a duel that keeps going across a seam crystallizes onto one shard — the higher wire joins
# the lower one's shard, held there while the fight lasts, every blow applied once through
# the handover, back to its region once it is over, no oscillation),
# player / speed-less mob / teleport shard crossings, game-code despawn, park, logout
# timer and logout bot, MMO mirror bytes, the kit design findings flipped to fixed).
cargo test --workspace

cargo run -p gsb-server                    # default config (0.0.0.0:7777, 1 room, 30 Hz global)
cargo run -p gsb-server -- config.example.toml
cargo run -p gsb-server --example client   # AUTH + JOIN + MOVE_TO, prints snapshots
RUST_LOG=info cargo run -p gsb-server      # also per-second gsb-metric lines (DESIGN §12)
```

All keys in `config.example.toml` are optional; embedded defaults are used for
missing ones.

## Load testing (gsb-loadgen)

Persistent load generator binary (first end-to-end numbers and saturation analysis:
`docs/CHANGELOG.md`, the metrics + load round):

```sh
cargo run -p gsb-server --bin gsb-loadgen -- 500 --duration 10
# N clients (default 100), 10 s window, 150 ms MOVE_TO, single room.
# In-process: a real server (ephemeral port) + N real TCP clients;
# server-side metrics are captured from the channel (no stdout parsing).
# --stagger-ms MS: client i connects after an i×MS delay (default 0 = all at once;
#  a loopback burst is the worst case for the accept path; rUDP heals a lost
#  handshake datagram by re-sending it, counted as `hs_retries`).
# --addr HOST:PORT: client-only mode against an external server.
# --transport tcp|udp|ws (default tcp): the clients' door and the in-process /
#  served server's (forwarded to both orchestrated children). ws = WebSocket: the
#  server gets one "ws" listener, client bytes count each WS message (header,
#  client mask, frame); --tls-ca with ws is refused (the WS door has no TLS form).
# --game demo|arena|mmo|war (default demo): the clients' bot and the in-process /
#  served server's `game` key (forwarded to both orchestrated children). The
#  arena bot runs its units base → centre → base with height; the MMO bot
#  roams a waystone, travels between shards and attacks nearby mobs (RESULT
#  adds `shard_members=`); the war bot holds a tower or capture point, moves
#  between posts and strikes enemies in reach (RESULT adds `shard_members=`
#  and the team exchange's `team_*=` rates). Demo-only flags (--visibility, --profile, …)
#  refuse the other games.
# --rpc-rate R [--rpc-burst B] (demo; in-process or --addr runs): every client also
#  sends R ECONOMY requests/s, B back to back every B/R s, and matches the answers;
#  RESULT adds the rpc_* keys (sent, ok, each rejection, client-side timeouts,
#  duplicate/unmatched answers — both must be 0 — and the ok latency p50/p99/max).
```

Output: human-readable report + single line `RESULT mode=.. clients=.. joined=..
snap_per_client_p50=.. tick_hz_med=.. errors=.. server_closes=.. server_hz=..
step_p50_us=.. dropped=.. server_in_bps=.. server_out_bps=.. peak_conns=..`
(scriptable). `errors` counts what the clients observed; `server_closes` counts
the sessions the server ended on its own initiative (write stall, idle window,
violation budget, …), with one `server_close_<reason>=N` key per reason — a
client whose socket the server gave up on never receives the ERROR frame, so only
the server-side count can show it. A non-zero total also prints a `WARNING` line.
`--write-stall-secs F` / `--idle-timeout-secs F` override the two session windows
of the in-process, `--serve` and orchestrated server (0 = disabled).
Smoke test (`gsb-server/tests/loadgen_smoke.rs`) actually spawns the binary inside the suite
and verifies `RESULT`; heavy runs are deliberately kept outside the suite
(to avoid slowing it down or making it flaky).

## Wire protocol

```text
[u32 LE body length][u16 LE opcode][protobuf payload]
```

The length prefix lives **only in the transport layer** (the transport
owns framing). Opcode bands: `1..=64` base control (auth/join/leave/heartbeat/
error), `1000+` game band (`MOVE_TO=1000`, `WORLD_SNAPSHOT=1003`,
`PRIVATE=1004`).

**Version and error contract** (protocol-hardening round, DESIGN §5.2-5.5):

- The client sends `Auth.protocol_version` in its first frame (`AUTH_REQ`);
  `gsb_protocol::PROTOCOL_VERSION` is the single source of truth. `0` =
  unversioned (legacy client), accepted, logged. Mismatch: `ERROR`
  code 13, **the connection survives** (client must be recompiled). Single check,
  no extra round trip.
- `ERROR` codes are not a comment table, but a generated proto enum:
  `gsb.base.ErrorCode`. A client receiving an unknown code treats it
  like `ERROR_CODE_OTHER` (raw number is preserved, can be logged); `0`
  is never sent.
- **Both halves** of the RPC envelope are in `base.proto` (`RpcRequest` +
  `RpcResponse`); `game.proto` imports it.
- Removed field numbers are `reserved` (see `EntityRecord`), retired
  opcodes are in the `gsb_demo::op::RETIRED` list and locked by tests.

Bandwidth: broadcast sends a **full, self-contained snapshot** per entity;
a batch dropped for a slow client causes at most 1 tick of staleness.
Delta-encoded AOI snapshots (spatial strategy), team fog of
war, PVS, and sharded rooms are implemented today; all are selected via
the `visibility` key in config (`all` / `spatial` / `team` /
`pvs` / `sharded`).

## The next game

A new game is a new crate like `gsb-demo` (2D), `gsb-demo-arena` (3D
team fog), `gsb-demo-mmo` (3D, ground-plane AOI over shards) or
`gsb-demo-war` (3D, team fog over a sharded map), on top
of `gsb-kit`:

1. Write its `proto/` (`import "kit.proto"`; declare typed mirrors of the
   kit's `WorldSnapshot`/`Private` with its own record type — the client
   rules are written in `kit.proto`) + opcodes (its own block: `Game`'s
   `SNAPSHOT_OP`/`PRIVATE_OP` have no default);
2. Define its components and systems;
3. Implement the kit's seams — `RecordCodec` (what a record's bytes are),
   `Game` (+ `TeamGame` / `ShardGame` for those strategies), and
   `Planar` / `Spatial` on its position and wire types to use the
   presets — and pick a room + preset (`AoiRoom<G, Grid2>`, …);
4. Implement `gsb_server::GameModule` for it (read your own `[<name>]` table with `gsb_server::games::settings`), and host it through `start_game_server`, or add it to the catalog behind a cargo feature as `games/arena.rs` and `games/mmo.rs` do.

The core/net/protocol/ecs/kit crates are untouched.

## Documentation

- `docs/DESIGN.md`: architectural decisions, scaling, metrics infrastructure (§12),
  constraints, and roadmap.
- `docs/ROADMAP.md`: open tasks and priorities.
- `docs/CHANGELOG.md`: complete record of completed rounds (including raw load test
  numbers).
- `docs/TICK-ARCHITECTURE.md`: broadcast-based tick architecture.
- `docs/RPC-CONTROL-PLANE.md`: RPC pattern and control plane design.
- `docs/OPS.md`: ops surface design (/metrics, /healthz, admin API) and metrics export:
  counters are collected cheaply inside and leave the server at one seam through pluggable
  exporters, each behind a cargo feature — Prometheus text at `/metrics` (`prometheus`, on by
  default) and an OTLP/HTTP protobuf push to an OpenTelemetry collector (`otlp`, off by default;
  `[metrics.otlp] endpoint = "http://127.0.0.1:4318"`); both walk one family table.
- `docs/TRAIT-ARCHITECTURE.md`: GameLogic unification, PlayerId path.
- `docs/KIT-ARCHITECTURE.md`: `gsb-kit` design (the seams a game implements,
  the kit proto and its typed mirrors, presets, the phases and their results).
- `docs/RECONNECT.md`: disconnected player policy (detach/resume/bot handover)
  design: **implemented** (ROADMAP P1 `[x]`; core mechanics + demo park
  policy, `crates/gsb-core/tests/reconnect.rs`), remains valid as the
  contract of the implementation.
- `CONTRIBUTING.md`: contribution discipline (lint prohibitions, actor rules,
  fmt/clippy/test gates).

The design documents under `docs/` are written in Turkish; `docs/README.md` is an
English index of what each one covers. All code and API documentation is in English.

## License

MIT: `LICENSE`.
