//! Connection migration, client side (BACKLOG B3; module
//! `crate::udp::path`): ask for a connection id in the proof, tag every
//! datagram with it once granted, answer the server's path challenges,
//! and [`UdpClient::rebind`] — a new local socket, the same session. A
//! NAT rebinding needs nothing here: the client cannot see it, and every
//! tagged datagram names its session wherever it arrives from. A child
//! of [`super`], so it reaches the client's private state directly.

use super::*;

/// The client's migration state.
#[derive(Debug, Default)]
pub(super) struct Migration {
    /// Whether this client asks for a CID (`UdpClientConfig::migration`).
    want: bool,
    /// The CID the server granted (`None`: not asked, or an older server
    /// or one whose migration is off — the client never tags then).
    pub(super) cid: Option<u64>,
}

impl Migration {
    pub(super) fn new(want: bool) -> Self {
        Self { want, cid: None }
    }

    /// The capability byte the proof carries (0: none, the proof as it
    /// always was).
    pub(super) fn caps(&self) -> u8 {
        if self.want { CAP_CID } else { 0 }
    }
}

impl UdpClient {
    /// Whether the server granted this session a connection id: its
    /// address may change ([`Self::rebind`], a NAT rebinding) without
    /// ending it.
    pub fn migratable(&self) -> bool {
        self.path.cid.is_some()
    }

    /// The granted CID (the tests' view: on the wire it is a bearer
    /// token before B5a).
    #[cfg(test)]
    pub(in crate::udp) fn cid(&self) -> Option<u64> {
        self.path.cid
    }

    /// The CID of a sealed session (it came inside the server's message 2).
    pub(super) fn set_cid(&mut self, cid: u64) {
        self.path.cid = Some(cid);
    }

    /// The server's accept (`d`, the handshake's evidence): the CID after
    /// `ACK{1}`, when this client asked for one. Anything shorter, or
    /// another kind of evidence, grants nothing.
    pub(super) fn take_cid(&mut self, d: &[u8]) {
        if self.path.want && d.first() == Some(&KIND_ACK) {
            self.path.cid = u64_at(d, 5);
        }
    }

    /// A path challenge (`d` is the whole datagram): echo its nonce at
    /// once, tagged, from this socket — proof that the server's new path
    /// to this client works. A client without a CID ignores it (counted:
    /// a server only challenges a session that has one).
    pub(super) fn on_path_challenge(&mut self, d: &[u8]) {
        let (Some(_), Some(nonce)) = (self.path.cid, u64_at(d, 1)) else {
            self.stats.path_challenges_ignored += 1;
            return;
        };
        // `[8][nonce]`, through `wire`: tagged on a plaintext session
        // (`[0x88][cid][nonce]`, the 17-byte response), sealed on a sealed
        // one (where only the key holder can have read the nonce).
        let mut inner = vec![KIND_PATH_RESPONSE];
        inner.extend_from_slice(&nonce.to_le_bytes());
        let Some(response) = self.wire(inner) else {
            return;
        };
        match self.sock.try_send_to(&response, self.peer) {
            Ok(_) => self.stats.path_challenges_answered += 1,
            Err(_) => self.stats.path_responses_send_failed += 1,
        }
    }

    /// Move this session to a new local socket (a new port; on a device,
    /// the interface the OS now routes through — Wi-Fi ↔ cellular): the
    /// old socket is closed, and the cumulative ACK goes out at once from
    /// the new one, tagged, so the server starts validating the new path
    /// without waiting for this client's next frame. Until it validates,
    /// the server keeps sending on the old path (decision 10), so what it
    /// sends meanwhile is lost — the game band heals on the next
    /// snapshot, the control band by its retransmit. Returns the new
    /// local address.
    ///
    /// `Unsupported` (and nothing changes) when the server granted no
    /// CID ([`Self::migratable`]): such a session cannot move — the
    /// caller reconnects and resumes instead.
    pub async fn rebind(&mut self) -> std::io::Result<SocketAddr> {
        let Some(_) = self.path.cid else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "rUDP rebind: the server granted no connection id \
                 (migration off, or a server that predates it)",
            ));
        };
        let ip = self.sock.local_addr()?.ip();
        self.sock = UdpSocket::bind(SocketAddr::new(ip, 0)).await?;
        self.stats.rebinds += 1;
        let nudge = self
            .wire(encode_ack(self.in_expected))
            .ok_or_else(super::seal::exhausted)?;
        self.sock.send_to(&nudge, self.peer).await?;
        self.sock.local_addr()
    }
}
