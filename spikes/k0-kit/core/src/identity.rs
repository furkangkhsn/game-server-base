//! The wire identity and its single minter — as `gsb-kit`'s
//! `identity.rs`, minus the bevy `Component` derive: a binding that needs
//! the id ON an entity wraps it in its own component type
//! (`k0_kit_bevy_ecs::KitWire`; hecs takes any `'static` type).

/// A record's wire identity. Private field, no constructor: the only
/// source is [`Minter`] (the kit's structural lock, unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WireId(u64);

impl WireId {
    /// The serial.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A room's identity counter (the sequential form only — the spike runs
/// one world).
#[derive(Debug, Clone, Default)]
pub struct Minter {
    used: u64,
}

impl Minter {
    /// Mint the next wire identity.
    pub fn mint(&mut self) -> WireId {
        self.used += 1;
        WireId(self.used)
    }
}
