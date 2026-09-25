//! The game's session payload (`Private.game`) reaches the decoder
//! ([`ClientDecoder::session_private`]) whatever arm the frame carries;
//! a payload the decoder rejects rejects the frame and changes nothing.

use super::*;

/// The fixture's decoder, keeping every session payload it is handed
/// (and rejecting one that starts with `0xFF`).
#[derive(Default)]
struct Told {
    inner: FixDecoder,
    told: Vec<Vec<u8>>,
}

impl ClientDecoder for Told {
    type Record = (i32, i32);
    type Cell = Cell;

    fn record(&self, body: &[u8]) -> Result<(u64, (i32, i32)), ClientError> {
        self.inner.record(body)
    }
    fn cell_of(&self, record: &(i32, i32)) -> Cell {
        self.inner.cell_of(record)
    }
    fn cell_exit(&self, body: &[u8]) -> Result<Cell, ClientError> {
        self.inner.cell_exit(body)
    }
    fn session_private(&mut self, body: &[u8]) -> Result<(), ClientError> {
        if body.first() == Some(&0xFF) {
            return Err(ClientError::Malformed("not a greeting"));
        }
        self.told.push(body.to_vec());
        Ok(())
    }
}

fn with_game(payload: Option<proto::private::Payload>, game: &[u8]) -> Vec<u8> {
    proto::Private {
        payload,
        game: game.to_vec(),
        ..Default::default()
    }
    .encode_to_vec()
}

/// Beside an ack, beside a one-shot full, or alone: the decoder is told.
#[test]
fn the_decoder_is_told_the_session_payload_beside_any_arm() {
    let mut view = ClientView::new(Told::default());
    let ack = proto::private::Payload::Ack(proto::InputAck { processed_up_to: 3 });
    assert_eq!(
        view.apply_private(&with_game(Some(ack), &[1])),
        Ok(PrivateEvent::Ack(3))
    );
    let full = proto::private::Payload::Snapshot(Frame::full(5, &[(1, 2, 3)]).message());
    assert_eq!(
        view.apply_private(&with_game(Some(full), &[2, 2])),
        Ok(PrivateEvent::Full { sequence: 5 })
    );
    assert_eq!(view.get(1), Some(&(2, 3)));
    assert_eq!(
        view.apply_private(&with_game(None, &[3])),
        Ok(PrivateEvent::Empty)
    );
    // An empty payload on the wire (tag + zero length) is still a payload.
    assert_eq!(view.apply_private(&[0x22, 0x00]), Ok(PrivateEvent::Empty));
    // No field 4: nothing to tell.
    assert_eq!(
        view.apply_private(&with_game(None, &[])),
        Ok(PrivateEvent::Empty)
    );
    assert_eq!(view.decoder().told, [vec![1], vec![2, 2], vec![3], vec![]]);
    assert_eq!(view.counters().errors, 0);
}

/// A payload the decoder rejects: the frame is an error, its one-shot
/// full is not applied, the view keeps its baseline.
#[test]
fn a_rejected_session_payload_rejects_the_frame() {
    let mut view = ClientView::new(Told::default());
    view.apply_snapshot(&Frame::full(4, &[(1, 0, 0)]).kit())
        .expect("a full");
    let full = proto::private::Payload::Snapshot(Frame::full(9, &[(7, 1, 1)]).message());
    assert!(view.apply_private(&with_game(Some(full), &[0xFF])).is_err());
    assert_eq!(view.counters().errors, 1);
    assert_eq!(sorted_ids(&view), [1], "the full was not applied");
    assert_eq!(view.last_sequence(), Some(4));
    // Field 4 under a wrong wire type is a malformed envelope.
    assert!(view.apply_private(&[0x20, 0x01]).is_err());
    assert!(view.decoder().told.is_empty());
}

fn sorted_ids<D: ClientDecoder>(view: &ClientView<D>) -> Vec<u64> {
    let mut ids: Vec<u64> = view.ids().collect();
    ids.sort_unstable();
    ids
}
