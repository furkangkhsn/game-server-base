//! The end of a client's run: the input flood (`--flood-id`), and the
//! graceful leave with its bounded wait for the answer.

use super::*;
use gsb_protocol::base::LeaveRoomResult;
use tokio::io::AsyncWriteExt;

/// The input flood: write the bot's flood input (the demo's MOVE_TO) as
/// fast as the socket accepts, until the deadline. The server-side chain
/// (reader pump → conn inbox → conn actor → action channel → room pull
/// budget) bounds what actually reaches the tick; the excess is dropped
/// on the flooder's OWN full action channel (attributed to it). The
/// flood stays UNNUMBERED (seq 0, legacy): it probes the
/// drop-attribution guardrails, not the sequence rule (a numbered flood
/// would only spin the high-water mark).
pub(super) async fn flood(wire: &mut Conn, p: &ClientParams, rep: &mut ClientReport) {
    let (flood_op, payload) = p.bot.flood_input();
    match wire {
        // WebSocket: every frame is its own masked message (fresh key
        // each), fed unflushed as fast as the socket takes it.
        Conn::Stream { rx, tx } if rx.is_ws() => {
            let n = ws_message_bytes(Dir::Out, payload.len());
            while Instant::now() < p.deadline {
                if tx.feed(flood_op, &payload).await.is_err() {
                    break; // peer gone
                }
                rep.moves += 1;
                rep.bytes_out += n;
            }
        }
        // One encoded frame, written unflushed as fast as the socket
        // takes it.
        Conn::Stream { tx, .. } => {
            let f = gsb_client::frame::encode(flood_op, &payload);
            while Instant::now() < p.deadline {
                if tx.get_mut().write_all(&f).await.is_err() {
                    break; // peer gone
                }
                rep.moves += 1;
                rep.bytes_out += f.len() as u64;
            }
        }
        Conn::Udp(c) => {
            // rUDP: the client is ONE task (read and write share the
            // socket), so the flood interleaves NON-BLOCKING read-drains;
            // the flood frames travel the lossy game band, so retransmit
            // state never gets in the way.
            while Instant::now() < p.deadline {
                if c.send_frame(flood_op, payload.clone()).await.is_err() {
                    break;
                }
                rep.moves += 1;
                rep.bytes_out += wire_in_bytes(flood_op, payload.len());
                while c.recv_frame(Duration::ZERO).await.ok().flatten().is_some() {}
            }
        }
    }
}

/// Graceful leave (counted by the server's join/leave metrics) and the
/// wait for its answer: without it, the socket close — and any server
/// shutdown that follows — can race ahead of the leave, and the server
/// never counts it. Only a seated client leaves: one never seated could
/// only be answered `NotInRoom`.
///
/// The wait ends at the answer, at the wire's end, or after
/// [`PROTOCOL_WAIT`] of the client's own waiting (see `wait.rs`: the
/// rUDP leave is a control-band frame re-sent until the server ACKs it,
/// for up to that bound; the old 500 ms was shorter than one 1 s
/// `MAX_RTO`, B88). A `NotInRoom` answer (the room had already ended the
/// membership) ends it too, counted.
///
/// (Not `session::leave`: this wait counts every frame's bytes and reads
/// past other frames, as the measurement always has.)
pub(super) async fn leave(
    wire: &mut Conn,
    rep: &mut ClientReport,
    mut rpc: Option<&mut RpcClient>,
    private_op: u16,
) {
    let leave = session::leave_req();
    rep.bytes_out += frame_bytes(wire, Dir::Out, leave.op, leave.payload.len());
    if wire.send(leave.op, &leave.payload).await.is_err() {
        return;
    }
    let mut wait = Wait::new(PROTOCOL_WAIT);
    while let Some(slice) = wait.slice() {
        let at = Instant::now();
        let got = wire.recv(slice).await;
        wait.charge(slice, at);
        let f = match got {
            Ok(Recv::Frame(f)) => f,
            Ok(Recv::Quiet) => continue,
            Ok(Recv::Closed) | Err(_) => return,
        };
        rep.bytes_in += frame_bytes(wire, Dir::In, f.op, f.payload.len());
        // Answers still arriving before the leave's ack count too.
        if let Some(r) = rpc.as_deref_mut().filter(|_| f.op == private_op) {
            r.on_private(&f.payload, Instant::now());
        }
        if f.op == op::base::LEAVE_ROOM_RESULT {
            let _ = LeaveRoomResult::decode(&f.payload[..]);
            rep.left = true;
            return;
        }
        if f.op == op::base::ERROR
            && ServerError::decode_lossy(&f.payload).code == ErrorCode::NotInRoom
        {
            rep.errors.not_in_room += 1;
            return;
        }
    }
}
