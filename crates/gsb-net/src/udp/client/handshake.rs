//! The client half of the stateless cookie handshake, loss included:
//! each step is re-sent until the server answers it, and the handshake
//! ends only on the server's evidence that it holds the session — or
//! with a clean `TimedOut` at the bound. A child module, so it reaches
//! [`super::UdpClient`]'s private state directly.

use super::*;

impl UdpClient {
    /// Run the handshake on this (not yet established) client.
    ///
    /// Two steps, one loop, one clock:
    ///
    /// 1. **challenge request** `HELLO{nonce, 0}` until a challenge with
    ///    our nonce comes back;
    /// 2. **proof** `HELLO{nonce, cookie}` until the server shows it holds
    ///    the session. Its accept is an `ACK{1}`; ANY session datagram
    ///    (ACK, REL, RAW, FRAG) is the same evidence, because the server
    ///    sends those only to a peer in its session table — and one that
    ///    arrives here is handed to the ordinary inbound path, not lost.
    ///
    /// A step is re-sent every [`HANDSHAKE_RTO`] without an answer; the
    /// whole handshake gives up at `within` (the default is
    /// [`HANDSHAKE_DEADLINE`], inside one cookie slot, so every proof
    /// re-send carries a cookie the server still accepts). Every re-send
    /// reuses the FIRST cookie: a second challenge (the answer to a
    /// re-sent request) is ignored, so the server sees one proof value.
    pub(super) async fn handshake(&mut self, nonce: u64, within: Duration) -> std::io::Result<()> {
        let deadline = Instant::now() + within;
        let mut cookie: Option<u64> = None;
        let mut resend = false;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    match cookie {
                        None => "rUDP handshake: no challenge within the deadline",
                        Some(_) => "rUDP handshake: the server never accepted the proof",
                    },
                ));
            }
            if resend {
                match cookie {
                    None => self.stats.challenge_retries += 1,
                    Some(_) => self.stats.proof_retries += 1,
                }
            }
            let hello = encode_hello(nonce, cookie.unwrap_or(0));
            self.sock.send_to(&hello, self.peer).await?;
            // Unless an answer moves us to the next step, the same step is
            // sent again when this one's interval ends.
            resend = true;
            let step_ends = (now + HANDSHAKE_RTO).min(deadline);
            while let Some(wait) = step_ends.checked_duration_since(Instant::now()) {
                let n = match tokio::time::timeout(wait, self.sock.recv_from(&mut self.buf)).await {
                    Err(_) => break, // no answer within the interval
                    Ok(Err(e)) => return Err(e),
                    Ok(Ok((n, from))) if from == self.peer && n > 0 => n,
                    Ok(Ok(_)) => continue, // a stray peer
                };
                match (self.buf[0], cookie) {
                    (KIND_HELLO, None) if n >= 18 && self.buf[1..9] == nonce.to_le_bytes() => {
                        // The challenge: the proof is the next step, sent
                        // at once (not a re-send).
                        cookie = Some(u64::from_le_bytes(self.buf[9..17].try_into().unwrap()));
                        resend = false;
                        break;
                    }
                    // A duplicate or late challenge, or a HELLO for
                    // another nonce: the first cookie stands.
                    (KIND_HELLO, _) => {}
                    (_, Some(_)) => {
                        let d = self.buf[..n].to_vec();
                        self.process_datagram(&d);
                        self.established = true;
                        self.ack_progress = Instant::now();
                        return Ok(());
                    }
                    // Nothing proven yet: this cannot be our session.
                    (_, None) => {}
                }
            }
        }
    }
}
