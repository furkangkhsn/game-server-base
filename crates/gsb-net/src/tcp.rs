//! The default transport: TCP + `[u32 LE length][u16 LE opcode][payload]`.
//!
//! This is the de-facto industry-standard framing for a tokio game server
//! stack. Frame bodies are handed to the actor layer via
//! [`gsb_protocol::FrameBody`]; the 4-byte length prefix lives here, inside
//! the transport, and never leaks out.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::ready;

use bytes::Buf;
use bytes::BytesMut;
use futures::Sink;
use futures::Stream;
use futures::StreamExt;
use tokio::io::AsyncWrite;
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::FramedRead;
use tokio_util::codec::LengthDelimitedCodec;
use tracing::debug;

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use gsb_protocol::FrameBody;

use crate::pump::spawn_pumps;
use crate::transport::{BoxFuture, Endpoint, Listener, Transport};

/// Maximum frame body size (opcode + payload). Defaults to 1 MiB.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Default TCP transport.
#[derive(Clone)]
pub struct TcpTransport {
    pub max_frame_bytes: usize,
}

impl Default for TcpTransport {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
        }
    }
}

struct TcpListenerHandle {
    listener: TcpListener,
    max_frame_bytes: usize,
}

impl Transport for TcpTransport {
    fn bind(
        self: Arc<Self>,
        addr: std::net::SocketAddr,
    ) -> BoxFuture<'static, std::io::Result<Arc<dyn Listener>>> {
        Box::pin(async move {
            let listener = TcpListener::bind(addr).await?;
            debug!(%addr, "TCP listener bound");
            Ok(Arc::new(TcpListenerHandle {
                listener,
                max_frame_bytes: self.max_frame_bytes,
            }) as Arc<dyn Listener>)
        })
    }
}

impl Listener for TcpListenerHandle {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, std::io::Result<Endpoint>> {
        Box::pin(async move {
            let (stream, peer) = self.listener.accept().await?;
            stream.set_nodelay(true)?;
            debug!(%peer, "connection accepted");
            Ok(self.make_endpoint(stream))
        })
    }

    fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.listener.local_addr().ok()
    }
}

impl TcpListenerHandle {
    fn make_endpoint(&self, stream: TcpStream) -> Endpoint {
        let (read_half, write_half) = stream.into_split();
        let reader = TcpReader {
            inner: LengthDelimitedCodec::builder()
                .little_endian()
                .max_frame_length(self.max_frame_bytes)
                .new_read(read_half),
        };
        let writer = TcpWriter {
            inner: write_half,
            buf: BytesMut::with_capacity(1024),
        };
        Endpoint::new(
            move |conn: ConnectionId, in_tx: Mailbox<ConnIn>, out_rx: Inbox<FrameBatch>| {
                spawn_pumps(conn, reader, writer, in_tx, out_rx)
            },
        )
    }
}

/// Streaming view over a length-delimited read half; yields parsed frames.
struct TcpReader {
    inner: FramedRead<OwnedReadHalf, LengthDelimitedCodec>,
}

impl Stream for TcpReader {
    type Item = io::Result<FrameBody>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match ready!(this.inner.poll_next_unpin(cx)) {
            None => Poll::Ready(None),
            Some(Err(e)) => Poll::Ready(Some(Err(e))),
            Some(Ok(chunk)) => Poll::Ready(Some(
                FrameBody::decode(chunk.freeze())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            )),
        }
    }
}

/// Sink view over a write half; appends `[u32 LE len][frame body]` per frame
/// and flushes through the socket buffer.
struct TcpWriter {
    inner: OwnedWriteHalf,
    buf: BytesMut,
}

impl Sink<FrameBody> for TcpWriter {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        // Ready to accept more frames as soon as the queued bytes can be
        // written out (keeps the socket buffer from growing unbounded).
        let this = self.get_mut();
        Self::drain(this, cx)
    }

    fn start_send(self: Pin<&mut Self>, item: FrameBody) -> Result<(), io::Error> {
        let this = self.get_mut();
        let body = item.encode();
        // The 4-byte LE length prefix covers the frame body (opcode +
        // payload). Size limits are enforced by the reader's codec
        // (`max_frame_bytes`), so there is nothing to guard here.
        this.buf
            .extend_from_slice(&(body.len() as u32).to_le_bytes());
        this.buf.extend_from_slice(&body);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.get_mut();
        Self::drain(this, cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.get_mut();
        ready!(TcpWriter::drain(this, cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

impl TcpWriter {
    /// Write all queued bytes to the socket; Pending when the socket would
    /// block.
    fn drain(this: &mut TcpWriter, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            if this.buf.is_empty() {
                return Pin::new(&mut this.inner).poll_flush(cx);
            }
            let n = ready!(Pin::new(&mut this.inner).poll_write(cx, &this.buf))?;
            this.buf.advance(n);
        }
    }
}

impl TcpReader {
    /// Test helper: build a reader/writer pair over a live TCP stream.
    #[cfg(test)]
    pub fn for_stream(
        stream: TcpStream,
        max_frame_bytes: usize,
    ) -> (
        impl Stream<Item = io::Result<FrameBody>> + 'static,
        impl Sink<FrameBody, Error = io::Error> + 'static,
    ) {
        let (r, w) = stream.into_split();
        (
            TcpReader {
                inner: LengthDelimitedCodec::builder()
                    .little_endian()
                    .max_frame_length(max_frame_bytes)
                    .new_read(r),
            },
            TcpWriter {
                inner: w,
                buf: BytesMut::with_capacity(1024),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::SinkExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::task::JoinHandle;

    /// A loopback peer that accepts one connection and echoes everything it
    /// receives until the client closes the write side.
    fn echo_server(listener: TcpListener) -> JoinHandle<()> {
        tokio::spawn(async move {
            let (peer, _) = listener.accept().await.expect("accept");
            let (mut r, mut w) = peer.into_split();
            let mut buf = vec![0u8; 65536];
            loop {
                match r.read(&mut buf).await {
                    Ok(0) | Err(_) => break, // EOF or error: client went away
                    Ok(n) => {
                        w.write_all(&buf[..n]).await.expect("echo write");
                        w.flush().await.expect("echo flush");
                    }
                }
            }
        })
    }

    async fn connect(listener: TcpListener) -> (TcpStream, JoinHandle<()>) {
        let addr = listener.local_addr().unwrap();
        let server = echo_server(listener);
        let stream = TcpStream::connect(addr).await.unwrap();
        (stream, server)
    }

    /// Write a frame the wire way, then read the echoed frame back through
    /// TcpReader.
    #[tokio::test]
    async fn framing_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (stream, server) = connect(listener).await;

        let (mut reader, mut writer) = TcpReader::for_stream(stream, 1024);

        // Encode via TcpWriter.
        writer
            .send(FrameBody::new(7, b"payload".as_slice()))
            .await
            .unwrap();
        writer.flush().await.unwrap();

        // Decode via TcpReader (echoed by the peer).
        let frame = reader.next().await.expect("frame").expect("io");
        assert_eq!(frame.op, 7);
        assert_eq!(frame.payload, b"payload".as_slice());

        drop(writer);
        server.await.expect("echo server");
    }

    /// Multiple frames in one write are re-assembled correctly, and EOF is
    /// observed once the peer closes.
    #[tokio::test]
    async fn multiple_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (stream, server) = connect(listener).await;

        let (mut reader, mut writer) = TcpReader::for_stream(stream, 1024);
        writer
            .send(FrameBody::new(1, b"a".as_slice()))
            .await
            .unwrap();
        writer
            .send(FrameBody::new(2, b"bb".as_slice()))
            .await
            .unwrap();
        writer
            .send(FrameBody::new(3, b"ccc".as_slice()))
            .await
            .unwrap();
        writer.flush().await.unwrap();

        for (i, op) in [1u16, 2, 3].into_iter().enumerate() {
            let frame = reader.next().await.expect("frame").expect("io");
            assert_eq!(frame.op, op, "frame {i}");
        }
        // Close our write side (OwnedWriteHalf half-closes on drop) so the
        // echo server exits and the reader observes EOF.
        drop(writer);
        server.await.expect("echo server");
        assert!(reader.next().await.is_none(), "stream ended");
    }

    /// A length prefix that exceeds `max_frame_bytes` must produce an error,
    /// never a panic or a memory blow-up.
    #[tokio::test]
    async fn oversized_frame_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            // Claim a 10 MiB body (max is 8), then close.
            peer.write_all(&10_485_760u32.to_le_bytes()).await.unwrap();
            drop(peer);
        });
        let stream = TcpStream::connect(addr).await.unwrap();
        server.await.unwrap();

        let (mut reader, _writer) = TcpReader::for_stream(stream, 8);
        let result = reader.next().await.expect("codec result");
        assert!(result.is_err(), "reader must reject the oversized frame");
    }
}
