//! The demo's defaults, as `Config` spells them, stay the demo's own.

use crate::Config;

/// `Config` spells the demo's defaults as literals so it builds without
/// `gsb-demo`; these must never drift from the constants they copy
/// (docs/GAME-MODULE.md §4.5: the default config's effect is unchanged).
#[test]
fn config_defaults_match_the_demo_constants() {
    let cfg = Config::default();
    assert_eq!(
        cfg.team_vision_radius,
        gsb_demo::team::DEFAULT_VISION_RADIUS
    );
    assert_eq!(cfg.spawn_half_size, gsb_demo::room::DEFAULT_SPAWN_HALF);
    assert_eq!(
        cfg.disconnect_grace_secs,
        gsb_demo::DEFAULT_DISCONNECT_GRACE.as_secs_f64()
    );
    assert_eq!(cfg.game, super::DemoModule::NAME);
}
