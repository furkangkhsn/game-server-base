//! A sealed door's demux driven directly (B5a): the Noise handshake over
//! a real client socket (every answer read where it went), the record
//! path, its refusals and the migration rule. Children: `handshake`
//! (the proof's refusals, idempotence), `records` (refusals, limits),
//! `rule` (migration), `budget` (the DH budget), `cost` (a timing probe).

use super::*;
use crate::seal::{Accept, Initiator, Sealer, Session, StaticKey};
use crate::udp::sealed::{DoorSeal, context, encode_sealed_proof};
use gsb_core::channel::{FrameBatch, Inbox};

/// A sealed, migration-on demux and its endpoint queue.
pub(super) fn sealed_demux(
    sock: Arc<UdpSocket>,
    per_sec: Option<u32>,
) -> (Demux, crossbeam_channel::Receiver<Queued>, [u8; 32]) {
    let (mut d, end_rx) = demux_bare(sock);
    d.migration = true;
    let key = Arc::new(StaticKey::generate().expect("a key"));
    let public = key.public();
    d.seal = Some(DoorSeal::new(key, per_sec));
    (d, end_rx, public)
}

/// A client socket: a std one, read with a REAL timeout — on a paused
/// tokio clock a `timeout` around a tokio read can advance the clock past
/// the DH budget's refill while the datagram is already queued.
pub(super) fn client() -> Client {
    let s = Client::bind("127.0.0.1:0").expect("bind a client");
    s.set_read_timeout(Some(Duration::from_millis(150)))
        .expect("a read timeout");
    s
}

/// A client socket (see [`client`]).
pub(super) type Client = std::net::UdpSocket;

/// The next datagram `s` receives within 150 ms (any size).
pub(super) async fn recv(s: &Client) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 2048];
    s.recv_from(&mut buf).ok().map(|(n, _)| buf[..n].to_vec())
}

/// The challenge exchange for `client`: its cookie.
pub(super) async fn cookie_for(d: &mut Demux, client: &Client, nonce: u64) -> u64 {
    feed(d, addr(client), &encode_hello(nonce, 0));
    let ch = recv(client).await.expect("the challenge");
    assert_eq!(ch.len(), 18, "the challenge is the request's size");
    u64_at(&ch, 9).unwrap()
}

/// The client's proof (pinning `server`) for `nonce`/`cookie`.
pub(super) fn proof(server: &[u8; 32], nonce: u64, cookie: u64) -> (Initiator, Vec<u8>) {
    let ini = Initiator::new(server, &context(nonce, cookie), &[]).unwrap();
    let p = encode_sealed_proof(nonce, cookie, CAP_CID, ini.msg1());
    (ini, p)
}

pub(super) fn addr(s: &Client) -> SocketAddr {
    s.local_addr().unwrap()
}

/// One sealed session on a sealed demux, the client's halves in hand.
pub(super) struct Rig {
    pub(super) d: Demux,
    pub(super) a: Client,
    pub(super) b: Client,
    pub(super) key: SessionKey,
    pub(super) cid: u64,
    /// The client's c→s sealer.
    pub(super) tx: Sealer,
    pub(super) inbox: Inbox<ConnIn>,
    pub(super) outbox: Inbox<FrameBatch>,
    pub(super) accept: Vec<u8>,
    pub(super) proof: Vec<u8>,
}

/// A full handshake: `(accept, session, the accept bytes, the proof)`.
pub(super) async fn handshake(
    d: &mut Demux,
    client: &Client,
    server: &[u8; 32],
) -> (Accept, Session, Vec<u8>, Vec<u8>) {
    let nonce = 0xA5A5_0000 ^ u64::from(addr(client).port());
    let cookie = cookie_for(d, client, nonce).await;
    let (mut ini, p) = proof(server, nonce, cookie);
    feed(d, addr(client), &p);
    let acc = recv(client).await.expect("the accept");
    let (accept, session) = ini.finish(&acc[5..]).expect("message 2 authenticates");
    (accept, session, acc, p)
}

pub(super) async fn rig() -> Rig {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock.writable().await.expect("writable");
    let (mut d, end_rx, server) = sealed_demux(sock, None);
    let a = client();
    let b = client();
    let (accept, session, acc, p) = handshake(&mut d, &a, &server).await;
    let (tx, _rx) = session.into_halves();
    let mut ep = end_rx.try_recv().expect("an endpoint").into_endpoint();
    let (_in_tx, inbox) = ep.take_inbox(16);
    let (_out_tx, outbox) = ep.take_outbox(16);
    let key = d.sessions.key_at(&addr(&a)).expect("the session");
    Rig {
        d,
        a,
        b,
        key,
        cid: accept.cid,
        tx,
        inbox,
        outbox,
        accept: acc,
        proof: p,
    }
}

impl Rig {
    /// The client's next record carrying `inner`.
    pub(super) fn seal(&mut self, inner: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        self.tx.seal(inner, &mut out).expect("seal");
        out
    }

    /// A sealed RAW game frame carrying `p`.
    pub(super) fn raw(&mut self, p: &[u8]) -> Vec<u8> {
        let inner = encode_raw(&FrameBody::new(1000, Bytes::copy_from_slice(p)));
        self.seal(&inner)
    }

    /// The payload of the session's next game frame (past move notices).
    pub(super) fn frame(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.inbox.try_recv() {
                Ok(ConnIn::Frame(f)) => return Some(f.payload.to_vec()),
                Ok(ConnIn::PeerChanged { .. }) => continue,
                _ => return None,
            }
        }
    }

    /// The writer's next `UDP_SEND` request: destination and inner.
    pub(super) fn send_request(&mut self) -> Option<(Option<SocketAddr>, Vec<u8>)> {
        while let Ok(batch) = self.outbox.try_recv() {
            for f in batch {
                if f.op == gsb_protocol::op::base::UDP_SEND {
                    let (to, inner) = crate::udp::sealed::decode_send(&f.payload)?;
                    return Some((to, inner.to_vec()));
                }
            }
        }
        None
    }

    pub(super) fn counts(&self) -> crate::udp::sealed::Counts {
        self.d.seal.as_ref().unwrap().counts
    }
}

/// The sealed handshake on the demux: 18 → 18 → 67 → 77 bytes; the
/// session carries the CID message 2 gave the client (always, the routing
/// key), its opener, and the accept kept for a re-sent proof; a record
/// from the client opens and its frame reaches the actor; its REL is
/// acknowledged THROUGH the writer (a `UDP_SEND` to the session's
/// address — the demux never puts an unsealed byte on the wire again).
#[tokio::test]
async fn a_sealed_handshake_establishes_a_session_whose_records_open() {
    let mut r = rig().await;
    assert_eq!((r.proof.len(), r.accept.len()), (67, 77));
    assert_eq!(&r.accept[..5], &[KIND_ACK, 1, 0, 0, 0]);
    let s = r.d.sessions.get(r.key).unwrap();
    assert_eq!(s.cid, Some(r.cid), "message 2's CID is the session's");
    assert!(s.seal.as_ref().is_some_and(|x| x.accept.is_some()));
    assert_eq!(r.d.mig.cids_assigned, 1);

    let rec = r.raw(b"hi");
    assert_eq!(
        rec.len(),
        1 + 8 + 8 + 5 + 16,
        "kind, cid, counter, RAW, tag"
    );
    assert_eq!(rec[0], crate::seal::wire::KIND_SEALED);
    assert_eq!(rec[1..9], r.cid.to_le_bytes(), "the CID at bytes 1..9");
    assert_eq!(rec[9..17], 0u64.to_le_bytes(), "the first counter");
    feed(&mut r.d, addr(&r.a), &rec);
    assert_eq!(r.frame().as_deref(), Some(&b"hi"[..]));
    let s = r.d.sessions.get(r.key).unwrap();
    assert!(
        s.seal.as_ref().is_some_and(|x| x.accept.is_none()),
        "the first record confirms: no proof is re-answered after it"
    );

    let rel = encode_rel(1, &FrameBody::new(7, Bytes::new()));
    let rec = r.seal(&rel);
    feed(&mut r.d, addr(&r.a), &rec);
    assert!(matches!(r.inbox.try_recv(), Ok(ConnIn::Frame(f)) if f.op == 7));
    assert_eq!(r.send_request(), Some((None, encode_ack(2))));
    assert!(recv(&r.a).await.is_none(), "nothing unsealed on the wire");
    assert_eq!(r.counts().refused, [0; 6]);
}

mod budget;
mod cost;
mod handshake;
mod records;
mod reset;
mod rule;
