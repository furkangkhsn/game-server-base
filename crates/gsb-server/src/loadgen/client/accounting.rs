//! The client-side byte accounting (`bytes_in` / `bytes_out`, RESULT's
//! `client_in_bps` / `client_out_bps`): every frame a client reads or
//! writes, at the size its DATA unit has on this wire — the
//! length-prefixed frame on a stream (TCP, TLS), the datagram on rUDP,
//! the WebSocket message (its frame header, the client's mask key, the
//! length-prefixed frame inside) on WebSocket. Below that unit nothing
//! is counted on any wire: no TCP/IP or UDP headers, no TLS records, no
//! handshakes (rUDP's cookie, the WS upgrade), no transport-only frames
//! (rUDP ACKs, WS pings, pongs and close frames).

use gsb_client::Conn;
use gsb_client::frame::wire_len;
use gsb_protocol::op;

/// Which way a frame crossed the wire (WebSocket masks only what the
/// client sends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dir {
    /// Server → client.
    In,
    /// Client → server.
    Out,
}

/// The bytes one frame of `payload_len` costs on `wire`, going `dir`.
pub(crate) fn frame_bytes(wire: &Conn, dir: Dir, op: u16, payload_len: usize) -> u64 {
    if wire.is_udp() {
        wire_in_bytes(op, payload_len)
    } else if wire.is_ws() {
        ws_message_bytes(dir, payload_len)
    } else {
        wire_len(payload_len) as u64
    }
}

/// The rUDP datagram size of one frame (client-side byte accounting
/// mirrors the bytes actually sent: RAW = kind + op + payload; REL =
/// kind + seq + op + payload).
pub(crate) fn wire_in_bytes(op: u16, payload_len: usize) -> u64 {
    let header = if (1..=64).contains(&op) && op != op::base::UDP_ACK {
        5
    } else {
        1
    };
    (header + 2 + payload_len) as u64
}

/// The WebSocket message carrying one frame: the length-prefixed frame
/// (`4 + 2 + payload`) as ONE unfragmented binary message — RFC 6455
/// §5.2's 2-byte header, plus a 2-byte (a body of 126..=65535 bytes) or
/// 8-byte (longer) extended length, plus the 4-byte mask key on what
/// the client sends. The gsb door sends each frame as one FIN message
/// and so does `gsb_client`.
pub(crate) fn ws_message_bytes(dir: Dir, payload_len: usize) -> u64 {
    let body = wire_len(payload_len);
    let ext_len = match body {
        0..=125 => 0,
        126..=65535 => 2,
        _ => 8,
    };
    let mask = match dir {
        Dir::In => 0,
        Dir::Out => 4,
    };
    (2 + ext_len + mask + body) as u64
}

#[cfg(test)]
mod tests;
