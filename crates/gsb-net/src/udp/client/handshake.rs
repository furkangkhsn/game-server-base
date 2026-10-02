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
    /// A step is re-sent when its timer expires without an answer: the
    /// reliable band's own timer ([`Rto`]), starting at `rel::INITIAL_RTO`
    /// and doubling per re-send — but never past [`HANDSHAKE_MAX_RTO`]
    /// (200 ms, BACKLOG B86; see [`step_interval`]). A step answered
    /// without a re-send is an RTT sample (Karn's rule), so the band
    /// starts with the path's estimate — and ONLY with it: the steps'
    /// backoff stays here ([`Rto::seed`]).
    /// The whole handshake gives up at `within` (the default is
    /// [`HANDSHAKE_DEADLINE`], inside one cookie slot, so every proof
    /// re-send carries a cookie the server still accepts). Every re-send
    /// reuses the FIRST cookie: a second challenge (the answer to a
    /// re-sent request) is ignored, so the server sees one proof value.
    pub(super) async fn handshake(&mut self, nonce: u64, within: Duration) -> std::io::Result<()> {
        // The tokio clock (a paused test clock drives the schedule).
        let deadline = tokio::time::Instant::now() + within;
        let mut cookie: Option<u64> = None;
        let mut resend = false;
        // The step's timer, and when the step was first sent (`None` once
        // it was re-sent: Karn's rule, its answer is no sample).
        let mut rto = Rto::default();
        let mut first_sent: Option<tokio::time::Instant>;
        loop {
            let now = tokio::time::Instant::now();
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
                rto.timed_out();
                first_sent = None;
            } else {
                first_sent = Some(now);
            }
            let hello = encode_hello(nonce, cookie.unwrap_or(0));
            self.sock.send_to(&hello, self.peer).await?;
            // Unless an answer moves us to the next step, the same step is
            // sent again when this one's timer expires.
            resend = true;
            let step_ends = (now + step_interval(&rto)).min(deadline);
            while let Some(wait) = step_ends.checked_duration_since(tokio::time::Instant::now()) {
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
                        if let Some(sent) = first_sent {
                            rto.sample(sent.elapsed());
                        }
                        resend = false;
                        break;
                    }
                    // A duplicate or late challenge, or a HELLO for
                    // another nonce: the first cookie stands.
                    (KIND_HELLO, _) => {}
                    (_, Some(_)) => {
                        if let Some(sent) = first_sent {
                            rto.sample(sent.elapsed());
                        }
                        // The band starts here, with the handshake's
                        // estimate — a clean step's sample, or none — and
                        // without its backoff (B86): a lost handshake step
                        // must not make the AUTH/JOIN behind it late.
                        self.rel = RelSend::new(Instant::now(), rto.seed());
                        let d = self.buf[..n].to_vec();
                        self.process_datagram(&d);
                        self.established = true;
                        return Ok(());
                    }
                    // Nothing proven yet: this cannot be our session.
                    (_, None) => {}
                }
            }
        }
    }
}

/// How long a handshake step waits for its answer before it is re-sent:
/// the step's timer, capped at [`HANDSHAKE_MAX_RTO`] (BACKLOG B86). An
/// 18-byte step is cheap to repeat and expensive to wait for, so the
/// backoff that protects the reliable band (up to [`MAX_RTO`]) stops
/// here at 200 ms: 50, 100, 200, 200, … ms.
pub(in crate::udp) fn step_interval(rto: &Rto) -> Duration {
    rto.current().min(HANDSHAKE_MAX_RTO)
}
