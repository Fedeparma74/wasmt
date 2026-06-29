//! [`WsStream`] — a byte-stream view over a [`WebSocketStream`].
//!
//! Wraps the message-level socket into `futures::io::{AsyncRead, AsyncWrite,
//! AsyncBufRead}` (the `ws_stream_tungstenite` shape). The stream is
//! **binary-only**: each [`AsyncWrite`] write becomes one binary frame, reads
//! reassemble binary frames into a contiguous byte stream, and message
//! boundaries are invisible to the byte consumer. A text frame on the wire is
//! a protocol violation and surfaces as `io::ErrorKind::InvalidData`.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use futures::io::{AsyncBufRead, AsyncRead, AsyncWrite};
use futures::sink::Sink;
use futures::stream::Stream;

use super::{Message, WebSocketStream};

/// A byte-stream adapter over a [`WebSocketStream`].
///
/// Construct with [`WsStream::new`]. Binary frames carry the bytes; text
/// frames are rejected as invalid data; a close frame (or end of stream) is
/// reported as EOF.
pub struct WsStream {
    inner: WebSocketStream,
    /// Bytes received but not yet handed to a reader.
    read_buf: VecDeque<u8>,
}

impl WsStream {
    /// Wrap a [`WebSocketStream`] as a byte stream.
    pub fn new(inner: WebSocketStream) -> Self {
        WsStream {
            inner,
            read_buf: VecDeque::new(),
        }
    }

    /// Recover the underlying [`WebSocketStream`]. Any bytes buffered from a
    /// partially-consumed binary frame are discarded.
    pub fn into_inner(self) -> WebSocketStream {
        self.inner
    }
}

fn to_io(e: super::Error) -> io::Error {
    io::Error::other(e.to_string())
}

/// Poll the inner message stream until the read buffer holds bytes, or EOF.
/// `Ready(Ok(false))` means EOF (close/end); `Ready(Ok(true))` means
/// `read_buf` is non-empty.
fn fill(
    inner: &mut WebSocketStream,
    read_buf: &mut VecDeque<u8>,
    cx: &mut Context<'_>,
) -> Poll<io::Result<bool>> {
    while read_buf.is_empty() {
        match ready!(Pin::new(&mut *inner).poll_next(cx)) {
            // `read_buf` is empty here (loop invariant), so adopt the frame's
            // allocation directly — `VecDeque::from(Vec)` is O(1), no byte copy.
            // Skip empty frames so a peer streaming them can't spin the loop.
            Some(Ok(Message::Binary(b))) => {
                if !b.is_empty() {
                    *read_buf = VecDeque::from(b);
                }
            }
            Some(Ok(Message::Text(_))) => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "received a text frame on a binary byte stream",
                )));
            }
            Some(Ok(Message::Close(_))) | None => return Poll::Ready(Ok(false)),
            Some(Err(e)) => return Poll::Ready(Err(to_io(e))),
        }
    }
    Poll::Ready(Ok(true))
}

impl AsyncRead for WsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // A zero-length read must complete immediately as Ok(0), without
        // touching the source (and without being mistaken for EOF).
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let this = self.get_mut();
        if !ready!(fill(&mut this.inner, &mut this.read_buf, cx))? {
            return Poll::Ready(Ok(0)); // EOF
        }
        // `VecDeque<u8>: Read` drains from the front into `buf`.
        Poll::Ready(io::Read::read(&mut this.read_buf, buf))
    }
}

impl AsyncBufRead for WsStream {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        if !ready!(fill(&mut this.inner, &mut this.read_buf, cx))? {
            return Poll::Ready(Ok(&[])); // EOF
        }
        let (front, _) = this.read_buf.as_slices();
        Poll::Ready(Ok(front))
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();
        // Saturate rather than panic if a caller over-consumes the slice that
        // `poll_fill_buf` returned.
        let n = amt.min(this.read_buf.len());
        this.read_buf.drain(..n);
    }
}

impl AsyncWrite for WsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Don't emit an empty binary frame for a zero-length write.
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let this = self.get_mut();
        ready!(Pin::new(&mut this.inner).poll_ready(cx)).map_err(to_io)?;
        // One write == one binary frame. Send `buf` directly (single copy into
        // JS); no intermediate `Vec`.
        this.inner.send_binary(buf).map_err(to_io)?;
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Poll::Ready(ready!(Pin::new(&mut this.inner).poll_flush(cx)).map_err(to_io))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Poll::Ready(ready!(Pin::new(&mut this.inner).poll_close(cx)).map_err(to_io))
    }
}
