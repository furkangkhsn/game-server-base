//! Noise NK on top of the stateless cookie handshake (0 extra RTT):
//!
//! ```text
//! client                                   server
//! HELLO{n,0}                    ──▶
//!                               ◀──        HELLO{n,cookie}       (no state, no DH)
//! proof + msg1 (-> e, es)       ──▶        cookie check FIRST, then DH
//!                               ◀──        accept + msg2 (<- e, ee; {cid, reset token})
//! SEALED REL AUTH(ticket)       ──▶
//! ```
//!
//! The responder API makes the order explicit: [`Msg1::parse`] only
//! checks the length; the DH runs in [`Msg1::cookie_verified`], which the
//! caller reaches only after its cookie check passed.

use snow::{Builder, HandshakeState};
use zeroize::Zeroize;

use super::identity::{
    Accept, HANDSHAKE_HASH_LEN, HandshakeError, KEY_LEN, MSG1_LEN_MAX, MSG1_LEN_MIN,
    MSG1_PAYLOAD_MAX, MSG2_LEN, StaticKey, map_snow, params, prologue,
};
use super::opener::Opener;
use super::sealer::Sealer;
use super::wire::Direction;

pub(super) fn builder(prologue: &[u8]) -> Result<Builder<'_>, HandshakeError> {
    Builder::new(params()).prologue(prologue).map_err(map_snow)
}

/// The client side. Keep it until message 2 authenticates; re-send
/// [`msg1`](Self::msg1) byte-for-byte when the proof is re-sent.
pub struct Initiator {
    hs: HandshakeState,
    msg1: Vec<u8>,
}

impl Initiator {
    /// Writes message 1 for the server whose pinned public key is
    /// `server_public`. `context` joins the Noise prologue: B5a passes the
    /// HELLO nonce and cookie, binding the handshake to that exchange.
    pub fn new(
        server_public: &[u8; KEY_LEN],
        context: &[u8],
        payload: &[u8],
    ) -> Result<Self, HandshakeError> {
        let prologue = prologue(context);
        Self::start(builder(&prologue)?, server_public, payload)
    }

    pub(super) fn start(
        b: Builder<'_>,
        server_public: &[u8; KEY_LEN],
        payload: &[u8],
    ) -> Result<Self, HandshakeError> {
        if payload.len() > MSG1_PAYLOAD_MAX {
            return Err(HandshakeError::PayloadTooLarge);
        }
        let b = b.remote_public_key(server_public).map_err(map_snow)?;
        let mut hs = b.build_initiator().map_err(map_snow)?;
        let mut msg1 = vec![0u8; MSG1_LEN_MIN + payload.len()];
        let n = hs.write_message(payload, &mut msg1).map_err(map_snow)?;
        msg1.truncate(n);
        Ok(Initiator { hs, msg1 })
    }

    /// Message 1 (at most [`MSG1_LEN_MAX`] bytes).
    pub fn msg1(&self) -> &[u8] {
        &self.msg1
    }

    /// Reads message 2. A failure leaves the initiator usable, so a forged
    /// accept injected by a third party does not end the handshake: keep
    /// waiting for the genuine one.
    pub fn finish(&mut self, msg2: &[u8]) -> Result<(Accept, Session), HandshakeError> {
        if msg2.len() != MSG2_LEN {
            return Err(HandshakeError::Malformed);
        }
        let mut payload = [0u8; MSG2_LEN];
        let n = self.read(msg2, &mut payload)?;
        let accept = Accept::decode(&payload[..n]).ok_or(HandshakeError::Malformed)?;
        Ok((accept, session(&mut self.hs, Some(accept.cid))))
    }

    pub(super) fn read(&mut self, msg2: &[u8], out: &mut [u8]) -> Result<usize, HandshakeError> {
        self.hs.read_message(msg2, out).map_err(map_snow)
    }

    #[cfg(test)]
    pub(super) fn state(&mut self) -> &mut HandshakeState {
        &mut self.hs
    }
}

/// The server side's view of message 1: shape-checked, not yet DH'd.
pub struct Msg1<'a>(&'a [u8]);

/// What the server gets once message 1 authenticated.
pub struct Responded {
    /// Message 2 ([`MSG2_LEN`] bytes): store it per session and re-send
    /// the same bytes when the same proof arrives again (idempotent).
    pub msg2: Vec<u8>,
    /// The client's message-1 payload (replayable — see
    /// [`MSG1_PAYLOAD_MAX`]).
    pub payload: Vec<u8>,
    /// The established session.
    pub session: Session,
}

impl<'a> Msg1<'a> {
    /// Length check only: no DH, no state. Call before the cookie check.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, HandshakeError> {
        if (MSG1_LEN_MIN..=MSG1_LEN_MAX).contains(&bytes.len()) {
            Ok(Msg1(bytes))
        } else {
            Err(HandshakeError::Malformed)
        }
    }

    /// The caller verified the stateless cookie for this source address;
    /// only now do the X25519 operations run. `context` must equal the
    /// client's (the same HELLO nonce and cookie).
    pub fn cookie_verified(
        self,
        key: &StaticKey,
        context: &[u8],
        accept: &Accept,
    ) -> Result<Responded, HandshakeError> {
        let prologue = prologue(context);
        let (msg2, payload, mut hs) = self.respond(builder(&prologue)?, key, &accept.encode())?;
        let session = session(&mut hs, None);
        Ok(Responded {
            msg2,
            payload,
            session,
        })
    }

    pub(super) fn respond(
        self,
        b: Builder<'_>,
        key: &StaticKey,
        payload2: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>, HandshakeState), HandshakeError> {
        let b = b.local_private_key(key.private()).map_err(map_snow)?;
        let mut hs = b.build_responder().map_err(map_snow)?;
        let mut payload = vec![0u8; MSG1_PAYLOAD_MAX];
        let n = hs.read_message(self.0, &mut payload).map_err(map_snow)?;
        payload.truncate(n);
        let mut msg2 = vec![0u8; KEY_LEN + payload2.len() + 16];
        let m = hs.write_message(payload2, &mut msg2).map_err(map_snow)?;
        msg2.truncate(m);
        Ok((msg2, payload, hs))
    }
}

/// An established session, before it is split between two actors.
pub struct Session {
    sealer: Sealer,
    opener: Opener,
    handshake_hash: [u8; HANDSHAKE_HASH_LEN],
}

impl Session {
    /// The Noise handshake hash: identical on both sides, unique to this
    /// session — for channel binding (e.g. bind the AUTH ticket to it).
    pub fn handshake_hash(&self) -> [u8; HANDSHAKE_HASH_LEN] {
        self.handshake_hash
    }

    /// The send half (writer) and the receive half (demux/reader). They
    /// share no state: no lock, no `Arc`.
    pub fn into_halves(self) -> (Sealer, Opener) {
        (self.sealer, self.opener)
    }
}

/// Splits a finished handshake into the two halves. `cid` is the client's
/// (the initiator writes it into every c→s header); the server passes
/// `None`.
pub(super) fn session(hs: &mut HandshakeState, cid: Option<u64>) -> Session {
    let mut handshake_hash = [0u8; HANDSHAKE_HASH_LEN];
    handshake_hash.copy_from_slice(hs.get_handshake_hash());
    // Noise Split(): the first key protects initiator→responder.
    let (mut c2s, mut s2c) = hs.dangerously_get_raw_split();
    let (sealer, opener) = if hs.is_initiator() {
        (
            Sealer::new(&mut c2s, cid),
            Opener::new(&mut s2c, Direction::ServerToClient),
        )
    } else {
        (
            Sealer::new(&mut s2c, None),
            Opener::new(&mut c2s, Direction::ClientToServer),
        )
    };
    c2s.zeroize();
    s2c.zeroize();
    Session {
        sealer,
        opener,
        handshake_hash,
    }
}
