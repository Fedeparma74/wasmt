//! WebSocket networking for the browser and Web Workers.
//!
//! An opt-in (`net` feature) WebSocket client modeled on the
//! **tokio-tungstenite** ecosystem, implemented over `web-sys` like
//! **gloo-net**:
//!
//! - [`connect`] / [`connect_with_protocols`] return a [`WebSocketStream`]
//!   that implements [`futures::Stream`]`<Item = Result<`[`Message`]`, `[`Error`]`>>`
//!   and [`futures::Sink`]`<`[`Message`]`>` — the tokio-tungstenite-wasm shape.
//! - [`WsStream`] wraps a [`WebSocketStream`] into a byte stream implementing
//!   `futures::io::{AsyncRead, AsyncWrite, AsyncBufRead}` — the
//!   `ws_stream_tungstenite` shape.
//!
//! Split a [`WebSocketStream`] into independent read/write halves with
//! [`futures::StreamExt::split`], exactly as you would a
//! tokio-tungstenite `WebSocketStream`.
//!
//! # Threading
//!
//! A browser `WebSocket` and its event callbacks are bound to the JS realm
//! that created them, so [`WebSocketStream`] is **`!Send`/`!Sync`**. Create
//! and drive it on one realm: the main thread (via
//! [`crate::spawn_local`] / [`crate::spawn_on_main`]) or a single pool worker
//! (via [`crate::spawn_pinned`]). It cannot move between threads the way a
//! `tokio::net::TcpStream` can.
//!
//! # Binary data
//!
//! Receives use `binaryType = "arraybuffer"` and are parsed synchronously.
//! Sends copy the bytes out of wasm shared memory into a fresh JS
//! `Uint8Array` and transmit via `send_with_array_buffer` — the copy is
//! mandatory because wasm linear memory is a `SharedArrayBuffer` and browsers
//! reject `SharedArrayBuffer`-backed views passed to `WebSocket.send`.
//!
//! # Limitations
//!
//! - No `Ping`/`Pong`/raw `Frame` — the browser WebSocket API never surfaces
//!   them (the browser answers pings itself).
//! - The receive queue is unbounded; a fast server with a slow consumer grows
//!   memory.
//! - No send backpressure: there is no browser "drain" event, and honoring
//!   `bufferedAmount` would require polling.
//!
//! # Example
//!
//! ```no_run
//! use futures::{SinkExt, StreamExt};
//! use wasmt::net::{connect, Message};
//!
//! # async fn run() -> Result<(), wasmt::net::Error> {
//! let mut ws = connect("wss://echo.websocket.org").await?;
//! ws.send(Message::Text("hello".into())).await?;
//! if let Some(Ok(msg)) = ws.next().await {
//!     // got the echo back
//!     let _ = msg;
//! }
//! # Ok(())
//! # }
//! ```

mod io;
mod websocket;

pub use io::WsStream;
pub use websocket::{WebSocketStream, connect, connect_with_protocols};

/// A WebSocket message.
///
/// The browser WebSocket API only ever surfaces text, binary, and close;
/// `Ping`/`Pong` are handled by the browser and never delivered here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A UTF-8 text frame.
    Text(String),
    /// A binary frame.
    Binary(Vec<u8>),
    /// The close frame. Yielded by the [`Stream`](futures::Stream) as the
    /// final `Ok` item (carrying the peer's code/reason when present), after
    /// which the stream ends.
    Close(Option<CloseFrame>),
}

/// Details of a WebSocket close handshake.
///
/// Browser-slim: the status is a raw `u16` rather than tungstenite's
/// `CloseCode` enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFrame {
    /// The numeric [close code](https://developer.mozilla.org/docs/Web/API/CloseEvent/code).
    pub code: u16,
    /// The human-readable close reason (may be empty).
    pub reason: String,
}

/// The `readyState` of a [`WebSocketStream`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Socket created, not yet open.
    Connecting,
    /// Open and ready to communicate.
    Open,
    /// Going through the closing handshake.
    Closing,
    /// Closed, or could not be opened.
    Closed,
}

/// Errors surfaced by [`WebSocketStream`].
///
/// Intentionally minimal — a browser-slimmed analogue of
/// `tungstenite::Error` (there is no TLS/HTTP/capacity/URL layer to fail in a
/// browser, and the `error` event is opaque for security reasons).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The `error` event fired, or `WebSocket::new` / a send threw.
    ConnectionError(String),
    /// A send was attempted after the socket started closing or closed.
    AlreadyClosed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::ConnectionError(m) => write!(f, "websocket connection error: {m}"),
            Error::AlreadyClosed => f.write_str("websocket already closed"),
        }
    }
}

impl std::error::Error for Error {}
