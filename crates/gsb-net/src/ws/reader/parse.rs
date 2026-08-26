//! One frame off the front of the buffer: RFC 6455 header decoding,
//! the mandatory client mask, and the size ceiling enforced BEFORE a
//! declared payload is ever waited for.

use std::io;



use crate::ws::*;

impl super::WsReader {
    /// Try to take one complete (unmasked) frame off the front of `buf`.
    /// `Ok(None)` = incomplete, wait for more bytes. Protocol violations
    /// fail the connection immediately — including an oversized declared
    /// length, which is rejected BEFORE its payload is waited for.
    pub(super) fn next_frame(&mut self) -> io::Result<Option<RawFrame>> {
        if self.buf.len() < 2 {
            return Ok(None);
        }
        let b0 = self.buf[0];
        let b1 = self.buf[1];
        let fin = b0 & 0x80 != 0;
        if b0 & 0x70 != 0 {
            return Err(self.proto_fail(1002, "RSV bits set but no extension was negotiated"));
        }
        let opcode = b0 & 0x0f;
        // Client-to-server frames MUST be masked (RFC 6455 §5.1). Enforced
        // before the length even arrives: fail fast, allocate never.
        if b1 & 0x80 == 0 {
            return Err(self.proto_fail(1002, "client-to-server frame is not masked"));
        }
        let l7 = (b1 & 0x7f) as usize;
        let mut off = 2usize;
        let len = match l7 {
            0x7e => {
                if self.buf.len() < 4 {
                    return Ok(None);
                }
                off = 4;
                u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize
            }
            0x7f => {
                if self.buf.len() < 10 {
                    return Ok(None);
                }
                let mut raw = [0u8; 8];
                raw.copy_from_slice(&self.buf[2..10]);
                off = 10;
                let raw = u64::from_be_bytes(raw);
                if raw > self.max_message_bytes as u64 {
                    return Err(self.proto_fail(
                        1009,
                        format_args!("frame declares {raw} bytes, over the size ceiling"),
                    ));
                }
                raw as usize
            }
            n => n,
        };
        if len > self.max_message_bytes {
            return Err(self.proto_fail(
                1009,
                format_args!("frame declares {len} bytes, over the size ceiling"),
            ));
        }
        let mask_at = off;
        off += 4;
        if self.buf.len() < off + len {
            self.buf
                .reserve((off + len - self.buf.len()).min(READ_CHUNK));
            return Ok(None);
        }
        let mut whole = self.buf.split_to(off + len);
        let key = [
            whole[mask_at],
            whole[mask_at + 1],
            whole[mask_at + 2],
            whole[mask_at + 3],
        ];
        // split_off(off): everything from `off` on is the masked payload.
        let mut payload = whole.split_off(off).to_vec();
        apply_mask(&mut payload, key);
        Ok(Some(RawFrame {
            fin,
            opcode,
            payload,
        }))
    }
}
