//! The conformance harness's opaque mapping: whole messages in and out,
//! with every RFC 6455 rule of the envelope mapping still in force.

use super::rig::*;
use super::*;

/// Payloads the game envelope would refuse (empty, short, a lying
/// length prefix) are delivered whole under op 0.
#[tokio::test]
async fn an_opaque_reader_delivers_any_binary_message_whole() {
    let mut rig = ReaderRig::with(DEFAULT_MAX_MESSAGE_BYTES, WsMessageMapping::Opaque).await;
    for payload in [&b""[..], b"xyz", &[0xff; 9]] {
        rig.send(true, OP_BIN, payload).await;
        let frame = rig.game().await;
        assert_eq!((frame.op, frame.payload.as_ref()), (0, payload));
    }
    rig.send(false, OP_BIN, b"frag").await;
    rig.send(true, OP_CONT, b"mented").await;
    assert_eq!(rig.game().await.payload.as_ref(), b"fragmented");
}

/// Only the message layer differs: the reader's rules are shared.
#[tokio::test]
async fn an_opaque_reader_enforces_the_same_rules() {
    let mut rig = ReaderRig::with(DEFAULT_MAX_MESSAGE_BYTES, WsMessageMapping::Opaque).await;
    rig.send(false, OP_BIN, b"open").await;
    rig.send(true, OP_BIN, b"intruder").await;
    assert_eq!(rig.failure_code().await, 1002);

    let mut rig = ReaderRig::with(DEFAULT_MAX_MESSAGE_BYTES, WsMessageMapping::Opaque).await;
    rig.send(true, OP_TEXT, b"hi").await;
    assert_eq!(rig.failure_code().await, 1003);
}

/// Through the real pumps and socket writer, an echo returns the exact
/// bytes as one binary message — no envelope added on the way out.
#[tokio::test]
async fn an_opaque_door_echoes_the_message_bytes_verbatim() {
    let transport = WsTransport {
        max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        mapping: WsMessageMapping::Opaque,
        ..WsTransport::default()
    };
    let mut client = FakeWsClient::connect(serve_echo_with(transport, None).await).await;
    let big: Vec<u8> = (0..70_000u32).map(|i| (i * 7) as u8).collect();
    for payload in [&b""[..], b"not an envelope", &big] {
        client.send_ws_binary(payload).await;
        let (fin, opcode, echoed) = client.read_frame().await;
        assert!(fin && opcode == OP_BIN);
        assert_eq!(echoed, payload);
    }
}
