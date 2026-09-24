//! gsb-kit — pluggable game components for gsb (KIT-ARCHITECTURE).
//!
//! This round the crate carries only the kit's own wire envelope
//! ([`proto`]); the strategies move in once their tests run on the kit's
//! own fixture game.

/// The kit's envelope messages (package `gsb.kit`, file `kit.proto`):
/// `WorldSnapshot`, `Private`, `InputAck`. Entity records and cell exits
/// are opaque game bytes here; a game's own proto declares the typed
/// mirror its clients decode with (KIT-ARCHITECTURE §5).
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/gsb.kit.rs"));

    #[cfg(test)]
    mod tests;
}
