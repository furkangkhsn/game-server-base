//! The action's spellings, and its default.

use super::*;
use crate::room::RoomConfig;

/// Off by default: the ceiling only ends the membership (today's
/// behaviour), whatever else the room sets.
#[test]
fn the_default_is_leave_room() {
    assert_eq!(AfkAction::default(), AfkAction::LeaveRoom);
    assert_eq!(RoomConfig::default().afk_action, AfkAction::LeaveRoom);
}

/// Each spelling reads back as its action; anything else is no action.
#[test]
fn the_spellings_round_trip() {
    for a in AfkAction::ALL {
        assert_eq!(AfkAction::parse(a.label()), Some(a));
    }
    assert_eq!(AfkAction::parse("leave_room"), Some(AfkAction::LeaveRoom));
    assert_eq!(AfkAction::parse("disconnect"), Some(AfkAction::Disconnect));
    for bad in ["", "kick", "Disconnect", "leave-room", " disconnect"] {
        assert_eq!(AfkAction::parse(bad), None, "{bad:?}");
    }
}
