# Design documents

These design documents are written in Turkish. All code and API documentation in
`crates/` is in English. This index says what each document covers.

| File | Summary |
|---|---|
| `CHANGELOG.md` | Chronological record of completed development rounds, mutation testing results, and load testing measurements. |
| `CROSS-SHARD.md` | Design contract for cross-shard entity interactions, remote effects, and boundary sharing. |
| `DESIGN.md` | Core architectural design document covering actor constraints, scaling targets, network transports, tick execution, and the metrics pipeline. |
| `DISTRIBUTED.md` | Design contract for distributed multi-process and multi-machine shard topologies using the ShardLink abstraction. |
| `HANDOFF.md` | Session handoff context and operational instructions for continuing development rounds and preserving architecture discipline. |
| `KIT-ARCHITECTURE.md` | Design (pending approval) for splitting the reusable strategies (AOI, team fog, PVS, delta engine, sharded composites) into a pluggable `gsb-kit` crate, separate from the example game. |
| `OPS.md` | Operational interface design specifying HTTP endpoints for Prometheus metrics, health checks, and room administration. |
| `PERSISTENCE.md` | Persistence architecture separating typed in-memory match checkpoints from centralized persistent world storage. |
| `RECONNECT.md` | Contract for handling disconnected players through detach, park ledger retention, and session reattachment. |
| `ROADMAP.md` | Prioritized backlog tracking closed milestones, active work, and open development items. |
| `RPC-CONTROL-PLANE.md` | Design specification for asynchronous out-of-tick RPC handling and runtime room lifecycle control. |
| `SECURITY.md` | Security design contract defining TLS integration, authentication rate limiting, and pre-authentication resource allocation limits. |
| `TICK-ARCHITECTURE.md` | Design discussion and rationale for broadcast-based tick distribution and non-blocking action channel draining. |
| `TRAIT-ARCHITECTURE.md` | Design contract for unifying common game logic under a shared supertrait across room and shard implementations. |
