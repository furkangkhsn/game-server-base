//! `ConnectionClosedBy` refines `ConnectionClosed` (BACKLOG F28): built
//! from the connection's verdict, folded back by `coarse`.

use crate::conn::ServerClose;
use crate::room::DisconnectCause;

#[test]
fn a_closed_connection_names_its_verdict_and_folds_back() {
    for v in ServerClose::ALL {
        let cause = DisconnectCause::closed(Some(v));
        assert_eq!(cause, DisconnectCause::ConnectionClosedBy(v));
        assert_eq!(cause.verdict(), Some(v));
        assert_eq!(cause.coarse(), DisconnectCause::ConnectionClosed);
    }
    let plain = DisconnectCause::closed(None);
    assert_eq!(plain, DisconnectCause::ConnectionClosed);
    assert_eq!(plain.verdict(), None);
    for own in [
        DisconnectCause::ConnectionClosed,
        DisconnectCause::IdleInput,
        DisconnectCause::Kicked,
    ] {
        assert_eq!(own.coarse(), own, "{own:?} refines nothing");
        assert_eq!(own.verdict(), None);
    }
}
