//! The verb's own rules: the reason bound, the message, the queue.

use super::*;

/// A reason over the cap is cut on a `char` boundary: a multi-byte
/// character straddling the cap is dropped whole, never split.
#[test]
fn the_reason_is_cut_on_a_char_boundary() {
    // 255 ASCII bytes, then a 2-byte 'ş' straddling byte 256.
    let reason = format!("{}şş", "a".repeat(KICK_REASON_MAX_BYTES - 1));
    let kept = bound_reason(reason);
    assert_eq!(kept.len(), KICK_REASON_MAX_BYTES - 1, "the straddler goes");
    assert!(kept.chars().all(|c| c == 'a'));
    // 254 ASCII bytes + 'ş' ends exactly at the cap: kept whole.
    let exact = format!("{}ş", "a".repeat(KICK_REASON_MAX_BYTES - 2));
    assert_eq!(bound_reason(exact.clone()), exact);
    // A 4-byte character three bytes before the cap end: cut before it.
    let emoji = format!("{}🦀tail", "a".repeat(KICK_REASON_MAX_BYTES - 3));
    assert_eq!(bound_reason(emoji).len(), KICK_REASON_MAX_BYTES - 3);
}

/// A short reason is kept as given.
#[test]
fn a_short_reason_is_kept() {
    assert_eq!(bound_reason("speed hack".into()), "speed hack");
}

/// The message names the verdict; an empty reason still populates it.
#[test]
fn the_message_is_prefixed_and_never_empty() {
    assert_eq!(kick_message("speed hack"), "kicked: speed hack");
    assert_eq!(kick_message(""), "kicked");
}

/// The queue keeps every kick, in order, bounded as asked; `take`
/// empties it.
#[test]
fn the_queue_keeps_the_kicks_in_order() {
    let q = KickQueue::default();
    let k = q.kicks();
    k.kick(PlayerId(2), "b");
    k.kick(PlayerId(1), "x".repeat(KICK_REASON_MAX_BYTES + 10));
    let got = q.take();
    assert_eq!(got.len(), 2);
    assert_eq!(
        got[0],
        Kick {
            player: PlayerId(2),
            reason: "b".into()
        }
    );
    assert_eq!(got[1].player, PlayerId(1));
    assert_eq!(
        got[1].reason.len(),
        KICK_REASON_MAX_BYTES,
        "bounded when asked"
    );
    assert!(q.take().is_empty(), "taken");
}

/// The default handle (a hand-built context's) keeps nothing.
#[test]
fn the_default_handle_is_inert() {
    Kicks::default().kick(PlayerId(1), "nobody hears this");
}

/// The close request a kick sends: the kicked cause, the prefixed reason.
#[test]
fn the_close_request_carries_the_kick() {
    let req = kick_close(ConnectionId(3), RoomId(4), 5, true, "afk");
    assert_eq!(
        (req.conn, req.room, req.entity, req.parked, req.cause),
        (ConnectionId(3), RoomId(4), 5, true, ServerClose::Kicked)
    );
    assert_eq!(req.reason, "kicked: afk");
}
