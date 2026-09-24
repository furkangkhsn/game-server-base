//! The demo's rooms: the kit's generic strategy rooms instantiated with
//! [`DemoGame`], plus the constructors every consumer (`gsb-server`'s
//! factories, the load generator, the tests) has always called. The
//! crate root's compatibility paths (`gsb_game::room::OpenRoom`, …) are
//! type aliases onto these instantiations.

use crate::demo::economy::EconomyService;
use crate::demo::play::DemoGame;
use crate::demo::spawn::DEFAULT_SPAWN_HALF;
use crate::kit::room::OpenRoom;

impl OpenRoom<DemoGame> {
    /// Build the open room over the default 100×100 arena (bit-identical
    /// spawn distribution to the pre-config rooms).
    pub fn new() -> Self {
        Self::with_spawn_half(DEFAULT_SPAWN_HALF)
    }

    /// Build the open room over a square spawn map of half-size `half`
    /// (entities spawn uniformly in `[-half, half]²`). The load
    /// generator's `spread` profile pairs this with its home distribution
    /// so spawn points and targets live on the same (possibly "wide")
    /// map.
    pub fn with_spawn_half(half: f32) -> Self {
        Self::with_game(DemoGame::new(half))
    }

    /// Attach the economy service handle (the RPC pattern's external-I/O
    /// half; the demo's economy service). The room delegates `ECONOMY`
    /// requests to it; the answer arrives on a later tick through the
    /// room's completion channel.
    pub fn with_economy(mut self, economy: EconomyService) -> Self {
        self.game_mut().set_economy(economy);
        self
    }
}

impl Default for OpenRoom<DemoGame> {
    fn default() -> Self {
        Self::new()
    }
}
