//! Three-axis config selection (docs/ROADMAP.md P2 "Konfigürasyon
//! düzeltmesi", Faz A): the config surface split into topology × visibility
//! × communication with legacy-key derivation and combination validation.
//!
//! What this suite locks:
//!
//! - every legacy `visibility` value derives its documented axes and maps
//!   onto the SAME room it always built (backward compatibility by
//!   construction, not by promise);
//! - an explicit new-axis key (`topology` / `communication`) overrides the
//!   derived cell;
//! - each SUPPORTED combination resolves to its expected room kind —
//!   asserted through `Config::resolve_selection`, no server needed;
//! - each UNSUPPORTED combination fails with ITS SPECIFIC error naming the
//!   roadmap phase/document that will deliver it, both at resolution time
//!   and through the real startup path (`start_server` refuses before any
//!   socket exists);
//! - the TOML grammar parses both spellings of the surface.

use gsb_server::{
    Communication, Config, ResolvedSelection, RoomKind, ServerError, Topology, Visibility,
    VisibilityAxis,
};

/// A config whose only non-default key is the legacy visibility spelling.
fn legacy(v: Visibility) -> Config {
    Config {
        visibility: v,
        ..Default::default()
    }
}

/// `cfg` plus explicit new-axis keys.
fn with_axes(
    cfg: &Config,
    topology: Option<Topology>,
    communication: Option<Communication>,
) -> Config {
    Config {
        topology,
        communication,
        ..cfg.clone()
    }
}

/// Resolve or panic; the happy-path helper keeps assertions one line.
fn resolved(cfg: &Config) -> ResolvedSelection {
    cfg.resolve_selection()
        .expect("combination must resolve to a supported room")
}

/// Resolve expecting rejection; returns the specific error for matching.
fn rejected(cfg: &Config) -> ServerError {
    cfg.resolve_selection()
        .expect_err("combination must be rejected at startup")
}

// ── Derivation defaults ────────────────────────────────────────────────

/// Every legacy `visibility` value derives EXACTLY the documented axes
/// and still lands on the same room it built before the axes existed:
/// `"sharded"` splits into topology=sharded + visibility=all (it was a
/// topology statement all along), spatial's derived `delta` names the
/// packaging AoiRoom already serves (its internal per-cell diff) — the
/// same room an explicit `communication = "delta"` request resolves to.
#[test]
fn legacy_visibility_values_derive_the_documented_axes() {
    let cases = [
        (
            Visibility::All,
            Topology::Single,
            VisibilityAxis::All,
            Communication::AlwaysFull,
            RoomKind::Open,
        ),
        (
            Visibility::Spatial,
            Topology::Single,
            VisibilityAxis::Spatial,
            // DERIVED delta = a description of AoiRoom's internal encoding;
            // it must NOT trip the explicit-delta rejection (the pre-axes
            // spelling has to keep working).
            Communication::Delta,
            RoomKind::Aoi,
        ),
        (
            Visibility::Team,
            Topology::Single,
            VisibilityAxis::Team,
            Communication::AlwaysFull,
            RoomKind::Team,
        ),
        (
            Visibility::Pvs,
            Topology::Single,
            VisibilityAxis::Pvs,
            Communication::AlwaysFull,
            RoomKind::Sector,
        ),
        (
            Visibility::Sharded,
            Topology::Sharded,
            VisibilityAxis::All,
            Communication::AlwaysFull,
            RoomKind::Sharded,
        ),
    ];
    for (legacy_value, want_topology, want_visibility, want_communication, want_kind) in cases {
        let sel = resolved(&legacy(legacy_value));
        assert_eq!(sel.topology, want_topology, "topology for {legacy_value}");
        assert_eq!(
            sel.visibility, want_visibility,
            "visibility for {legacy_value}"
        );
        assert_eq!(
            sel.communication, want_communication,
            "communication for {legacy_value}"
        );
        assert_eq!(sel.kind, want_kind, "room kind for {legacy_value}");
    }
}

/// The default config (no keys at all) is single × all × always-full →
/// OpenRoom: the least-surprising starting point, unchanged since ever.
#[test]
fn default_config_derives_the_all_baseline() {
    let sel = resolved(&Config::default());
    assert_eq!(sel.topology, Topology::Single);
    assert_eq!(sel.visibility, VisibilityAxis::All);
    assert_eq!(sel.communication, Communication::AlwaysFull);
    assert_eq!(sel.kind, RoomKind::Open);
}

// ── Explicit-key precedence ────────────────────────────────────────────

/// An explicit `topology` key beats the legacy derivation: legacy
/// `visibility = "sharded"` next to `topology = "single"` resolves to the
/// single whole-world room (with a startup warn — behavior never flips
/// silently), which is exactly what the old spelling can no longer force.
#[test]
fn explicit_topology_overrides_the_legacy_sharded_spelling() {
    let sel = resolved(&with_axes(
        &legacy(Visibility::Sharded),
        Some(Topology::Single),
        None,
    ));
    assert_eq!(sel.topology, Topology::Single);
    // The visibility half of the sharded spelling decodes to `all`.
    assert_eq!(sel.visibility, VisibilityAxis::All);
    assert_eq!(sel.kind, RoomKind::Open);
}

/// The symmetric direction: an explicit `topology = "sharded"` upgrades a
/// legacy single-world config to the shard grid without touching the
/// legacy key at all.
#[test]
fn explicit_topology_sharded_upgrades_a_legacy_single_config() {
    let sel = resolved(&with_axes(
        &legacy(Visibility::All),
        Some(Topology::Sharded),
        None,
    ));
    assert_eq!(sel.topology, Topology::Sharded);
    assert_eq!(sel.visibility, VisibilityAxis::All);
    assert_eq!(sel.communication, Communication::AlwaysFull);
    assert_eq!(sel.kind, RoomKind::Sharded);
}

// (An explicit `communication = "always-full"` under spatial USED to
// resolve here, asserting it was "honored as written" while mapping onto
// the delta-only AoiRoom — the selection reported a packaging its room did
// not speak. It is a startup rejection now; see
// `explicit_always_full_under_spatial_is_rejected_on_both_topologies`.)

/// Consistency of the two spellings (the round's fix): an EXPLICIT
/// `communication = "delta"` under single × spatial resolves to the SAME
/// AoiRoom the derived spelling always built — delta packaging already
/// exists there (the internal per-cell diff), so the explicit request must
/// not fail while its derived twin succeeds. Same room, one behavior.
#[test]
fn explicit_spatial_delta_resolves_to_the_aoi_room() {
    // The derived spelling (legacy key only) — the pre-existing path.
    let derived = resolved(&legacy(Visibility::Spatial));
    assert_eq!(derived.topology, Topology::Single);
    assert_eq!(derived.visibility, VisibilityAxis::Spatial);
    assert_eq!(derived.communication, Communication::Delta);
    assert_eq!(derived.kind, RoomKind::Aoi);

    // The fully explicit triple — must agree on every axis.
    let explicit = resolved(&with_axes(
        &legacy(Visibility::Spatial),
        Some(Topology::Single),
        Some(Communication::Delta),
    ));
    assert_eq!(explicit, derived, "explicit and derived spellings agree");
    assert_eq!(explicit.kind, RoomKind::Aoi);
}

// ── Supported-combination mapping ──────────────────────────────────────

/// Each SUPPORTED combination resolves to its room kind. These are the
/// factories that exist today — Faz A re-expressed the pre-axes surface,
/// and the Faz B composite added the sharded × spatial pair to it.
#[test]
fn every_supported_combination_maps_to_its_existing_room() {
    let cases = [
        // Legacy spellings (already covered above) plus their fully
        // explicit equivalents:
        (
            Visibility::All,
            Some(Topology::Single),
            Some(Communication::AlwaysFull),
            RoomKind::Open,
        ),
        (
            // Explicit spatial × delta is the SAME room as the derived
            // spelling: delta is AoiRoom's native packaging. (Its
            // always-full sibling is NOT a supported combination — the
            // room has no full-frame mode; see the rejection test.)
            Visibility::Spatial,
            Some(Topology::Single),
            Some(Communication::Delta),
            RoomKind::Aoi,
        ),
        (
            Visibility::Team,
            Some(Topology::Single),
            Some(Communication::AlwaysFull),
            RoomKind::Team,
        ),
        (
            Visibility::Pvs,
            Some(Topology::Single),
            Some(Communication::AlwaysFull),
            RoomKind::Sector,
        ),
        // Sharded × all is reachable BOTH via the legacy spelling and via
        // the explicit topology key.
        (Visibility::Sharded, None, None, RoomKind::Sharded),
        (
            Visibility::All,
            Some(Topology::Sharded),
            Some(Communication::AlwaysFull),
            RoomKind::Sharded,
        ),
        (
            // The Faz B composite: reachable ONLY through the explicit
            // topology key (the bare legacy "spatial" spelling means
            // single × spatial — the single-world AoiRoom).
            Visibility::Spatial,
            Some(Topology::Sharded),
            None,
            RoomKind::ShardedSpatial,
        ),
        (
            Visibility::Spatial,
            Some(Topology::Sharded),
            Some(Communication::Delta),
            RoomKind::ShardedSpatial,
        ),
    ];
    for (legacy_value, topology, communication, want_kind) in cases {
        let cfg = with_axes(&legacy(legacy_value), topology, communication);
        let sel = resolved(&cfg);
        assert_eq!(
            sel.kind, want_kind,
            "{legacy_value} + topology={topology:?} + communication={communication:?}"
        );
    }
}

/// `resolve_selection` is pure and stable: the same config resolves to the
/// same selection twice (the composition root may call it once, but tools
/// and tests rely on it being a function of the config alone).
#[test]
fn resolution_is_deterministic() {
    let cfg = with_axes(
        &legacy(Visibility::Team),
        Some(Topology::Single),
        Some(Communication::AlwaysFull),
    );
    assert_eq!(
        cfg.resolve_selection().unwrap(),
        cfg.resolve_selection().unwrap()
    );
}

// ── Unsupported-combination rejection ──────────────────────────────────

/// `single × delta` is rejected for every visibility WITHOUT a delta
/// implementation (all/team/pvs — spatial is the exemption that resolves,
/// see `explicit_spatial_delta_resolves_to_the_aoi_room`); the message
/// names which visibility CAN serve delta today plus the codec gap on the
/// roadmap.
#[test]
fn single_delta_rejected_for_visibilities_without_a_delta_impl() {
    for v in [Visibility::All, Visibility::Team, Visibility::Pvs] {
        let err = rejected(&with_axes(&legacy(v), None, Some(Communication::Delta)));
        match err {
            ServerError::SingleDelta => {}
            other => panic!("{v}: wrong error kind: {other}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("only spatial (AoiRoom) does"),
            "message must name the one visibility serving delta today: {msg}"
        );
        assert!(
            msg.contains("all/team/pvs have no delta packaging yet"),
            "message must name which visibilities lack delta: {msg}"
        );
        assert!(
            msg.contains("ROADMAP"),
            "message must point at the roadmap: {msg}"
        );
    }
}

/// `sharded × delta` fails cleanly naming BOTH roadmap phases that will
/// deliver it (Faz B composite book, Faz C per-link derivation), via the
/// legacy spelling AND the explicit topology key alike.
#[test]
fn sharded_delta_names_roadmap_faz_b_and_c() {
    let via_legacy = rejected(&with_axes(
        &legacy(Visibility::Sharded),
        None,
        Some(Communication::Delta),
    ));
    let via_explicit = rejected(&with_axes(
        &legacy(Visibility::All),
        Some(Topology::Sharded),
        Some(Communication::Delta),
    ));
    for err in [via_legacy, via_explicit] {
        match err {
            ServerError::ShardedDelta => {}
            other => panic!("wrong error kind: {other}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("Faz B"), "message must name Faz B: {msg}");
        assert!(msg.contains("Faz C"), "message must name Faz C: {msg}");
    }
}

/// The Faz B composite resolves END-TO-END on the config surface:
/// `sharded × spatial` maps onto the per-shard cell-delta room, and the
/// derived and explicit delta spellings agree (same room, one behavior —
/// the per-shard cell encoding IS the delta packaging on the grid).
#[test]
fn sharded_spatial_resolves_to_the_faz_b_composite() {
    // Derived spelling: explicit topology key + legacy spatial visibility.
    let derived = resolved(&with_axes(
        &legacy(Visibility::Spatial),
        Some(Topology::Sharded),
        None,
    ));
    assert_eq!(derived.topology, Topology::Sharded);
    assert_eq!(derived.visibility, VisibilityAxis::Spatial);
    assert_eq!(derived.communication, Communication::Delta);
    assert_eq!(derived.kind, RoomKind::ShardedSpatial);

    // Fully explicit triple: the same room, no disagreement.
    let explicit = resolved(&with_axes(
        &legacy(Visibility::Spatial),
        Some(Topology::Sharded),
        Some(Communication::Delta),
    ));
    assert_eq!(explicit, derived);
}

/// The mirror of the explicit-delta rejection, and the reason this test
/// exists: `spatial` rooms speak delta and ONLY delta (AoiRoom's per-cell
/// diff on `single`, the Faz B composite's per-shard cell delta on
/// `sharded` — neither has a full-frame mode). An explicit
/// `communication = "always-full"` therefore asks for packaging no spatial
/// room serves, exactly as `communication = "delta"` asks for packaging no
/// all/team/pvs room serves. Both must refuse at startup: accepting this
/// one used to resolve to `communication = always-full` while the room
/// that ran broadcast deltas — a running server misconfigured by a check
/// whose whole job is to prevent that.
///
/// Omitting the key still DERIVES delta (see
/// `legacy_visibility_values_derive_the_documented_axes`), so no config
/// that never mentioned the axis is affected.
#[test]
fn explicit_always_full_under_spatial_is_rejected_on_both_topologies() {
    for (topology, want_single) in [(Topology::Single, true), (Topology::Sharded, false)] {
        let err = rejected(&with_axes(
            &legacy(Visibility::Spatial),
            Some(topology),
            Some(Communication::AlwaysFull),
        ));
        match (&err, want_single) {
            (ServerError::SingleAlwaysFull, true) | (ServerError::ShardedAlwaysFull, false) => {}
            (other, _) => panic!("wrong error kind for {topology}: {other}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("spatial"),
            "message must name the spatial visibility that forces delta: {msg}"
        );
        assert!(
            msg.contains("delta"),
            "message must name the packaging the room actually speaks: {msg}"
        );
    }
}

/// Team/PVS interest reaches across shard seams, violating the shard
/// locality constraint (cross-shard interest needs a subscription layer
/// nobody has built): the error points at docs/CROSS-SHARD.md §4.
#[test]
fn sharded_team_and_pvs_violate_cross_shard_locality() {
    for v in [Visibility::Team, Visibility::Pvs] {
        let err = rejected(&with_axes(&legacy(v), Some(Topology::Sharded), None));
        match &err {
            ServerError::ShardedCrossInterest(axis) => {
                assert_eq!(*axis, VisibilityAxis::from(v).to_string())
            }
            other => panic!("{v}: wrong error kind: {other}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("CROSS-SHARD.md"),
            "message must reference docs/CROSS-SHARD.md §4: {msg}"
        );
    }
}

/// Rejections hold on the REAL startup path too: `start_server` runs the
/// combination check BEFORE binding any socket, so an unsupported config
/// never half-starts (the error surfaces from a plain await, nothing else).
#[tokio::test]
async fn unsupported_combinations_fail_at_startup_before_binding() {
    let locality = with_axes(&legacy(Visibility::Team), Some(Topology::Sharded), None);
    match gsb_server::start_server(locality).await {
        Err(ServerError::ShardedCrossInterest(_)) => {} // the contract
        Ok(_) => panic!("sharded × team must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }

    let codec_gap = with_axes(&legacy(Visibility::All), None, Some(Communication::Delta));
    match gsb_server::start_server(codec_gap).await {
        Err(ServerError::SingleDelta) => {} // the contract
        Ok(_) => panic!("single × delta must NOT start"),
        Err(e) => panic!("wrong error kind: {e}"),
    }
}

/// A SUPPORTED explicit-axes config starts end-to-end: the shard grid is
/// reachable purely through `topology = "sharded"` with the legacy key
/// left at its default (proving the axes are authoritative, not just
/// decoded decoration).
#[tokio::test]
async fn explicit_sharded_topology_starts_end_to_end() {
    let cfg = Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        topology: Some(Topology::Sharded),
        ..Default::default()
    };
    assert_eq!(cfg.resolve_selection().unwrap().kind, RoomKind::Sharded);
    let handle = gsb_server::start_server(cfg)
        .await
        .expect("explicit sharded topology starts");
    assert_eq!(handle.addrs.len(), 1);
    handle.stop().await;
}

// ── TOML grammar ───────────────────────────────────────────────────────

/// Write a temp TOML file and parse it back (`Config::from_file`).
fn parse_toml(name: &str, body: &str) -> Config {
    let path = std::env::temp_dir().join(format!(
        "gsb-config-axes-{}-{name}.toml",
        std::process::id()
    ));
    std::fs::write(&path, body).expect("write temp config");
    let parsed = Config::from_file(&path).expect("parse temp config");
    let _ = std::fs::remove_file(&path);
    parsed
}

/// The config FILE parses both grammars: the new axis keys deserialize
/// into the explicit options (and their unsupported combination still
/// rejects), while the legacy five-value spelling keeps parsing and
/// deriving exactly as before.
#[test]
fn config_file_parses_both_axis_grammars() {
    // New-style keys…
    let new_style = parse_toml(
        "new",
        r#"
bind = "127.0.0.1:0"
topology = "sharded"
communication = "delta"
visibility = "all"
"#,
    );
    assert_eq!(new_style.topology, Some(Topology::Sharded));
    assert_eq!(new_style.communication, Some(Communication::Delta));
    match new_style.resolve_selection() {
        Err(ServerError::ShardedDelta) => {} // the contract
        Ok(_) => panic!("new-style sharded × delta must NOT resolve"),
        Err(e) => panic!("wrong error kind: {e}"),
    }

    // …and the legacy spelling, untouched.
    let old_style = parse_toml(
        "legacy",
        r#"
bind = "127.0.0.1:0"
visibility = "sharded"
"#,
    );
    assert_eq!(old_style.topology, None);
    assert_eq!(old_style.communication, None);
    let sel = old_style.resolve_selection().expect("legacy parses");
    assert_eq!(sel.topology, Topology::Sharded);
    assert_eq!(sel.visibility, VisibilityAxis::All);
    assert_eq!(sel.kind, RoomKind::Sharded);
}
