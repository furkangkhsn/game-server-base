//! The fake WebSocket client the protocol tests drive the server with:
//! a raw TcpStream that speaks masked client frames, so the suite needs
//! no client-side WebSocket dependency.

use super::*;

/// A deterministic mask-key generator: RFC masking exists so proxies
/// cannot guess payload bytes, not for secrecy — fixed keys are fine in
/// tests and keep failures reproducible.
pub(super) struct MaskGen(pub(super) u32);
impl MaskGen {
    pub(super) fn next(&mut self) -> [u8; 4] {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0.to_be_bytes()
    }
}

/// Encode one client-to-server WS frame (masked unless `mask` says no).
pub(super) fn encode_client_frame(
    fin: bool,
    opcode: u8,
    payload: &[u8],
    key: [u8; 4],
    mask: bool,
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.push(if fin { 0x80 } else { 0 } | opcode);
    let mask_bit = if mask { 0x80 } else { 0 };
    if payload.len() < 126 {
        frame.push(mask_bit | payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(mask_bit | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(mask_bit | 127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    if mask {
        frame.extend_from_slice(&key);
        let masked: Vec<u8> = payload
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ key[i & 3])
            .collect();
        frame.extend_from_slice(&masked);
    } else {
        frame.extend_from_slice(payload);
    }
    frame
}

/// A raw-TCP fake client speaking enough of RFC 6455 to exercise the
/// server: real handshake (accept key verified against the RFC vector),
/// masked frames, fragmentation, close.
pub(super) struct FakeWsClient {
    stream: TcpStream,
    masks: MaskGen,
}

impl FakeWsClient {
    pub(super) async fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).await.expect("client connect");
        Self::handshake(stream, addr).await
    }

    /// The handshake over a stream the caller connected (e.g. one with a
    /// shrunk receive buffer).
    pub(super) async fn handshake(mut stream: TcpStream, addr: SocketAddr) -> Self {
        let request = format!(
            "GET /gsb HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {RFC_KEY}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("handshake write");
        let head = read_http_head(&mut stream).await;
        assert!(
            head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
            "want 101, got: {head}"
        );
        let accept = header_value(&head, "sec-websocket-accept")
            .expect("101 response must carry Sec-WebSocket-Accept");
        assert_eq!(accept, RFC_ACCEPT, "accept key must match the RFC vector");
        Self {
            stream,
            masks: MaskGen(42),
        }
    }

    /// Hand the upgraded socket back for raw byte-level reading.
    pub(super) fn into_stream(self) -> TcpStream {
        self.stream
    }

    /// Raw variant used by the malformed-handshake tests.
    pub(super) async fn raw(addr: SocketAddr) -> TcpStream {
        TcpStream::connect(addr).await.expect("raw connect")
    }

    pub(super) async fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.expect("raw write");
    }

    pub(super) async fn send_frame(&mut self, fin: bool, opcode: u8, payload: &[u8], mask: bool) {
        let key = self.masks.next();
        let frame = encode_client_frame(fin, opcode, payload, key, mask);
        self.send_raw(&frame).await;
    }

    /// Send one game frame inside one masked binary WS message.
    pub(super) async fn send_game(&mut self, frame: &FrameBody) {
        self.send_ws_binary(&encode_game_envelope(frame)).await;
    }

    pub(super) async fn send_ws_binary(&mut self, body: &[u8]) {
        self.send_frame(true, OP_BIN, body, true).await;
    }

    /// Read one server frame: `(fin, opcode, payload)` — unmasked.
    pub(super) async fn read_frame(&mut self) -> (bool, u8, Vec<u8>) {
        let mut head = [0u8; 2];
        self.stream.read_exact(&mut head).await.expect("frame head");
        let fin = head[0] & 0x80 != 0;
        assert_eq!(head[1] & 0x80, 0, "server must never mask");
        let l7 = (head[1] & 0x7f) as usize;
        let len = match l7 {
            0x7e => {
                let mut ext = [0u8; 2];
                self.stream.read_exact(&mut ext).await.expect("ext16");
                u16::from_be_bytes(ext) as usize
            }
            0x7f => {
                let mut ext = [0u8; 8];
                self.stream.read_exact(&mut ext).await.expect("ext64");
                u64::from_be_bytes(ext) as usize
            }
            n => n,
        };
        let mut payload = vec![0u8; len];
        if len > 0 {
            self.stream.read_exact(&mut payload).await.expect("payload");
        }
        (fin, head[0] & 0x0f, payload)
    }

    /// Read one echoed game frame.
    pub(super) async fn read_game(&mut self) -> FrameBody {
        let (fin, opcode, payload) = self.read_frame().await;
        assert!(fin, "echoed messages are single-frame");
        assert_eq!(opcode, OP_BIN, "game frames ride binary messages");
        let declared =
            u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        assert_eq!(payload.len(), 4 + declared, "exactly one frame per message");
        FrameBody::decode(Bytes::copy_from_slice(&payload[4..])).expect("echo decode")
    }

    /// Read one frame; it must be the close. Returns its status code,
    /// then asserts the connection ends (EOF, or a reset if the server
    /// failed before draining everything we pipelined).
    pub(super) async fn expect_close_then_eof(&mut self) -> u16 {
        let (_, opcode, payload) = self.read_frame().await;
        assert_eq!(opcode, OP_CLOSE, "next frame must be the close");
        let code = match payload.as_slice() {
            [] => 1005, // no status present
            [hi, lo] => u16::from_be_bytes([*hi, *lo]),
            other => panic!(
                "close payload must be empty or 2 bytes, got {}",
                other.len()
            ),
        };
        self.expect_eof().await;
        code
    }

    /// The socket must be over: clean EOF, or a reset when the server
    /// tore down with unread bytes still in flight (RST beats FIN).
    pub(super) async fn expect_eof(&mut self) {
        let mut eof = [0u8; 1];
        match self.stream.read(&mut eof).await {
            Ok(0) => {}
            Ok(n) => panic!("expected end of stream, got {n} byte(s)"),
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(e) => panic!("unexpected read error at teardown: {e}"),
        }
    }
}
