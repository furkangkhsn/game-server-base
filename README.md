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
  and 100 Hz (`gsb-game/tests/frame_independence.rs`).
- **ECS:** `bevy_ecs` (standalone). The room actor exclusively
  holds a `World`; core is completely ECS-free via the `W` generic over the world type.
- **Protocol:** protobuf (`prost` / `Google.Protobuf` in Unity).
  `[u32 LE length][u16 LE opcode][protobuf payload]`.
- **Transport:** five doors behind the `Transport`/`Listener`/
  `Endpoint` abstraction touching zero lines of actor code: plain TCP (default),
  TLS-TCP (rustls), rUDP (`transport = "udp"`; stateless cookie
  handshake: cookie rotates in 10 s slots, captured proof
  expires within 10-20 s; reliable control band `REL`: if there is no ACK
  progress for 5 s, the band is declared dead and the SESSION
  TERMINATES, no silent surrender; loss-tolerant game band `RAW`),
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
| `gsb-game` | **All game logic** (components, systems, `RoomLogic`, game proto) |
| `gsb-server` | Composition root: config, startup, `gsb-server` binary + client example + `gsb-loadgen` load generator |

## Quick start

Requirements: Rust **1.95.0** (pinned via `rust-toolchain.toml`; this is also the
MSRV), **nothing else**. Having `protoc` installed on the system is not required:
proto build scripts (`gsb-protocol`, `gsb-game`)
explicitly provide the embedded binary of `protoc-bin-vendored` to `prost-build`
(`Config::protoc_executable`), which takes precedence over searching
`PROTOC`/`PATH`. On an exotic target where the embedded binary is not found, the build script
prints a `cargo:warning` and falls back to the old behavior (`PROTOC`, then `PATH`);
there, system `protoc` is required. CI: `.github/workflows/ci.yml` (fmt ·
clippy `-D warnings` · test).

```sh
# 388 tests: framing, lint, ticker/room tick, RPC (single room + shard, the rpc_shard
# suite), ticket/control plane, READ fairness (rotating cursor), supervision (panicking
# room/shard), table pruning (epoch/tombstone TTL, metric retirement), reconnect
# (detach/resume/bot handover, PlayerId continuity), trait unification (GameLogic +
# sharded keepalive + shard RPC), security (TLS transport, auth rate limit, pre-auth
# caps), multi-listener (mixed transports on the same map: TCP/TLS/rUDP/QUIC/WS),
# border-delta exchange, ops surface (/metrics, /healthz, admin API), visibility (delta
# AOI, PVS, team fog, sharded), frame independence, identity invariant, publishability,
# e2e, metric flow, load smoke, wire contract (RPC envelope bytes, retired opcodes),
# ERROR code enumeration, protocol version handshake, session lifecycle (idle window +
# write stall) and heartbeat-ACK throttling, AFK signal (input-idle clock + default-off
# ceiling), sharded report folding (per-field fold rule).
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
#  a loopback burst is the worst case for the accept path, see the ROADMAP finding).
# --addr HOST:PORT: client-only mode against an external server.
```

Output: human-readable report + single line `RESULT mode=.. clients=.. joined=..
snap_per_client_p50=.. tick_hz_med=.. server_hz=.. step_p50_us=.. dropped=..
server_in_bps=.. server_out_bps=.. peak_conns=..` (scriptable).
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
  opcodes are in the `gsb_game::op::RETIRED` list and locked by tests.

Bandwidth: broadcast sends a **full, self-contained snapshot** per entity;
a batch dropped for a slow client causes at most 1 tick of staleness.
Delta-encoded AOI snapshots (spatial strategy), team fog of
war, PVS, and sharded rooms are implemented today; all are selected via
the `visibility` key in config (`all` / `spatial` / `team` /
`pvs` / `sharded`).

## The next game

Writing new game logic = only modifying the `gsb-game` crate:

1. Update `proto/game.proto` + ops;
2. Define components/`System`;
3. Implement `RoomLogic<World>`;
4. Bind the `demo_room_factory()` and `build_table()` call in `gsb-server`
   to the new factory.

The core/net/protocol/ecs crates are untouched.

## Documentation

- `docs/DESIGN.md`: architectural decisions, scaling, metrics infrastructure (§12),
  constraints, and roadmap.
- `docs/ROADMAP.md`: open tasks and priorities.
- `docs/CHANGELOG.md`: complete record of completed rounds (including raw load test
  numbers).
- `docs/TICK-ARCHITECTURE.md`: broadcast-based tick architecture.
- `docs/RPC-CONTROL-PLANE.md`: RPC pattern and control plane design.
- `docs/OPS.md`: ops surface design (/metrics, /healthz, admin API).
- `docs/TRAIT-ARCHITECTURE.md`: GameLogic unification, PlayerId path.
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
