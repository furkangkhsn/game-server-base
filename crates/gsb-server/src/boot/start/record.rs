//! `[record]` (BACKLOG B170): the registry's record plan from the
//! config — or a refused startup.

use crate::{Config, ServerError};

/// What the registry records: `None` without a `[record]` table.
#[cfg(feature = "record")]
pub(super) fn plan(
    cfg: &Config,
    game: &str,
) -> Result<Option<gsb_core::registry::RecordPlan>, ServerError> {
    use gsb_core::id::RoomId;
    use gsb_core::registry::{RecordPlan, RecordRooms};
    let Some(section) = &cfg.record else {
        return Ok(None);
    };
    if section.buffer == 0 {
        return Err(ServerError::BadRecord("`buffer` must be at least 1".into()));
    }
    if section.dir.is_empty() {
        return Err(ServerError::BadRecord("`dir` is empty".into()));
    }
    std::fs::create_dir_all(&section.dir)
        .map_err(|e| ServerError::BadRecord(format!("`dir` {:?}: {e}", section.dir)))?;
    let mut plan = RecordPlan::new(
        &section.dir,
        section.game_version.as_deref().unwrap_or(game),
    );
    plan.buffer = section.buffer;
    if let Some(ids) = &section.rooms {
        plan.rooms = RecordRooms::Only(ids.iter().copied().map(RoomId).collect());
    }
    tracing::info!(
        dir = %section.dir,
        rooms = ?section.rooms,
        game = %plan.game,
        "recording input logs (one file per room incarnation)"
    );
    Ok(Some(plan))
}

/// Without the `record` feature a `[record]` table refuses startup.
#[cfg(not(feature = "record"))]
pub(super) fn plan(cfg: &Config, _game: &str) -> Result<Option<()>, ServerError> {
    match cfg.record {
        Some(_) => Err(ServerError::RecordNotBuilt),
        None => Ok(None),
    }
}
