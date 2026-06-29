//! [`WebSocketStream`] — the message-level `Stream` + `Sink`, plus
//! [`connect`] / [`connect_with_protocols`].

use std::cell::RefCell;
use std::collections::VecDeque;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use futures::channel::oneshot;
use futures::sink::Sink;
use futures::stream::{FusedStream, Stream};
use futures::task::AtomicWaker;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::JsValue;

use super::{CloseFrame, Error, Message, State};

/// What the event closures push into the [`Inbox`]. Decoupled from [`Message`]
/// so the close/error events can carry their own variants without being
/// mistaken for data frames.
enum StreamMessage {
    Message(Message),
    Close(CloseFrame),
    Error(String),
}

/// Single-realm receive queue shared between the event closures (producers)
/// and the [`Stream`] (consumer). The socket is `!Send` and JS is
/// single-threaded — closures never preempt the consumer — so a plain
/// `RefCell<VecDeque>` + [`AtomicWaker`] is sound and avoids the per-message
/// heap node of a general-purpose MPSC channel. FIFO push/pop preserves
/// arrival order.
#[derive(Default)]
struct Inbox {
    queue: RefCell<VecDeque<StreamMessage>>,
    waker: AtomicWaker,
}

impl Inbox {
    fn pop(&self) -> Option<StreamMessage> {
        self.queue.borrow_mut().pop_front()
    }

    /// Producer side: enqueue and wake the consumer if it is parked.
    fn push(&self, msg: StreamMessage) {
        self.queue.borrow_mut().push_back(msg);
        self.waker.wake();
    }

    /// Consumer side: pop one item, or register `cx` and return `Pending`.
    fn poll(&self, cx: &Context<'_>) -> Poll<StreamMessage> {
        if let Some(m) = self.pop() {
            return Poll::Ready(m);
        }
        self.waker.register(cx.waker());
        // Re-check after registering so a push between the first pop and the
        // register is never missed (defensive; on one realm there is no such
        // interleave, but it mirrors the crate's other waker users).
        match self.pop() {
            Some(m) => Poll::Ready(m),
            None => Poll::Pending,
        }
    }
}

/// Shared one-shot slot resolving the `connect` future. Whichever of the
/// `open`/`error` events fires first `take()`s it; the loser becomes a no-op.
type ConnectSlot = Rc<RefCell<Option<oneshot::Sender<Result<(), Error>>>>>;

/// The event-listener closures, held alive for the socket's lifetime and
/// cleared in [`WebSocketStream`]'s `Drop` before they are freed.
struct WsClosures {
    _on_open: Closure<dyn FnMut()>,
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_close: Closure<dyn FnMut(web_sys::CloseEvent)>,
    _on_error: Closure<dyn FnMut(JsValue)>,
}

/// A connected WebSocket.
///
/// Implements [`futures::Stream`]`<Item = Result<`[`Message`]`, `[`Error`]`>>`
/// for reading and [`futures::Sink`]`<`[`Message`]`>` for writing. Split into
/// independent halves with [`futures::StreamExt::split`].
///
/// `!Send`/`!Sync`: drive it on the realm that created it (see the
/// [module docs](crate::net)).
pub struct WebSocketStream {
    ws: web_sys::WebSocket,
    inbox: Rc<Inbox>,
    /// Set once a `Close`/`Error` item has been yielded; the stream then ends.
    terminated: bool,
    _closures: WsClosures,
}

/// Open a WebSocket connection to `url` and resolve once it is open.
///
/// The returned future completes only after the browser fires the `open`
/// event (or `Err` if `open` never arrives — the `error` event fired or the
/// URL was rejected).
pub async fn connect(url: &str) -> Result<WebSocketStream, Error> {
    let ws = web_sys::WebSocket::new(url).map_err(|e| Error::ConnectionError(js_err(&e)))?;
    setup(ws).await
}

/// Like [`connect`], but negotiating one of `protocols` as the subprotocol.
pub async fn connect_with_protocols(
    url: &str,
    protocols: &[&str],
) -> Result<WebSocketStream, Error> {
    let arr = js_sys::Array::new();
    for p in protocols {
        arr.push(&JsValue::from_str(p));
    }
    let ws = web_sys::WebSocket::new_with_str_sequence(url, arr.as_ref())
        .map_err(|e| Error::ConnectionError(js_err(&e)))?;
    setup(ws).await
}

/// Wire up the event listeners on `ws` and await `open`.
///
/// The full [`WebSocketStream`] is built *before* awaiting, so that if this
/// future is dropped mid-connect (cancellation) — or `open` fails — the
/// stream's `Drop` clears the listeners and closes the socket.
async fn setup(ws: web_sys::WebSocket) -> Result<WebSocketStream, Error> {
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

    let inbox = Rc::new(Inbox::default());
    let (open_tx, open_rx) = oneshot::channel::<Result<(), Error>>();
    // Fired once by whichever of open/error wins; `take()` makes the loser a
    // no-op and lets a post-open `error` fall through to the inbox.
    let open_slot: ConnectSlot = Rc::new(RefCell::new(Some(open_tx)));

    let open_slot_open = open_slot.clone();
    let on_open = Closure::<dyn FnMut()>::new(move || {
        if let Some(tx) = open_slot_open.borrow_mut().take() {
            let _ = tx.send(Ok(()));
        }
    });

    let inbox_msg = inbox.clone();
    let on_message =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            let data = e.data();
            // `binaryType = "arraybuffer"`, so binary frames arrive as an
            // `ArrayBuffer` and parse synchronously. Text frames arrive as a JS
            // string. Anything else is ignored.
            let msg = if let Some(s) = data.as_string() {
                Message::Text(s)
            } else if let Ok(ab) = data.dyn_into::<js_sys::ArrayBuffer>() {
                Message::Binary(js_sys::Uint8Array::new(&ab).to_vec())
            } else {
                return;
            };
            inbox_msg.push(StreamMessage::Message(msg));
        });

    let inbox_close = inbox.clone();
    let open_slot_close = open_slot.clone();
    let on_close = Closure::<dyn FnMut(web_sys::CloseEvent)>::new(move |e: web_sys::CloseEvent| {
        let frame = CloseFrame {
            code: e.code(),
            reason: e.reason(),
        };
        if let Some(open) = open_slot_close.borrow_mut().take() {
            // Closed during the handshake, before `open` — without this, the
            // connect future would await `open` forever (some servers/proxies
            // close mid-handshake without first firing `error`).
            let _ = open.send(Err(Error::ConnectionError(format!(
                "closed before open (code {}, reason {:?})",
                frame.code, frame.reason
            ))));
        } else {
            inbox_close.push(StreamMessage::Close(frame));
        }
    });

    let open_slot_err = open_slot.clone();
    let inbox_err = inbox.clone();
    let on_error = Closure::<dyn FnMut(JsValue)>::new(move |_e: JsValue| {
        // The WebSocket `error` event is intentionally information-poor for
        // security reasons, so there is nothing useful to extract from `_e`.
        let msg = "websocket error".to_string();
        if let Some(open) = open_slot_err.borrow_mut().take() {
            // Pre-open failure → reject `connect`.
            let _ = open.send(Err(Error::ConnectionError(msg)));
        } else {
            // Post-open failure → surface to the stream.
            inbox_err.push(StreamMessage::Error(msg));
        }
    });

    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    let stream = WebSocketStream {
        ws,
        inbox,
        terminated: false,
        _closures: WsClosures {
            _on_open: on_open,
            _on_message: on_message,
            _on_close: on_close,
            _on_error: on_error,
        },
    };

    // On `Err`/cancellation, `stream` drops here → listeners cleared + close.
    match open_rx.await {
        Ok(Ok(())) => Ok(stream),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Error::ConnectionError("connect cancelled".into())),
    }
}

impl WebSocketStream {
    /// The socket's current `readyState`.
    pub fn state(&self) -> State {
        let st = self.ws.ready_state();
        if st == web_sys::WebSocket::CONNECTING {
            State::Connecting
        } else if st == web_sys::WebSocket::OPEN {
            State::Open
        } else if st == web_sys::WebSocket::CLOSING {
            State::Closing
        } else {
            State::Closed
        }
    }

    /// Start the closing handshake, optionally with a code and reason.
    ///
    /// Convenience over the [`Sink`] close path. Consumes the stream; for a
    /// graceful drain that observes the peer's close frame, instead poll the
    /// [`Stream`] to completion before dropping it.
    pub async fn close(self, frame: Option<CloseFrame>) -> Result<(), Error> {
        let res = match &frame {
            Some(f) => self.ws.close_with_code_and_reason(f.code, &f.reason),
            None => self.ws.close(),
        };
        res.map_err(|e| Error::ConnectionError(js_err(&e)))
    }

    /// Send a text frame. Shared by the [`Sink`] impl.
    pub(crate) fn send_text(&self, s: &str) -> Result<(), Error> {
        self.ws
            .send_with_str(s)
            .map_err(|e| Error::ConnectionError(js_err(&e)))
    }

    /// Send a binary frame in a single copy: `bytes` are copied out of wasm
    /// shared memory into a fresh (non-shared) JS array, because a
    /// `SharedArrayBuffer`-backed view would be rejected by `WebSocket.send`.
    /// Shared by the [`Sink`] impl and [`WsStream`](super::WsStream)'s
    /// `poll_write` (which passes its `&[u8]` straight through — no `to_vec`).
    pub(crate) fn send_binary(&self, bytes: &[u8]) -> Result<(), Error> {
        let arr = js_sys::Uint8Array::from(bytes);
        self.ws
            .send_with_array_buffer(&arr.buffer())
            .map_err(|e| Error::ConnectionError(js_err(&e)))
    }
}

impl Stream for WebSocketStream {
    type Item = Result<Message, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }
        match self.inbox.poll(cx) {
            Poll::Ready(StreamMessage::Message(m)) => Poll::Ready(Some(Ok(m))),
            Poll::Ready(StreamMessage::Close(f)) => {
                self.terminated = true;
                Poll::Ready(Some(Ok(Message::Close(Some(f)))))
            }
            Poll::Ready(StreamMessage::Error(m)) => {
                self.terminated = true;
                Poll::Ready(Some(Err(Error::ConnectionError(m))))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl FusedStream for WebSocketStream {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl Sink<Message> for WebSocketStream {
    type Error = Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let st = self.ws.ready_state();
        // `connect` only returns post-open, so `CONNECTING` is effectively
        // unreachable here; treat it as ready rather than stalling forever
        // (there is no transient waker to register against).
        if st == web_sys::WebSocket::OPEN || st == web_sys::WebSocket::CONNECTING {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(Error::AlreadyClosed))
        }
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Error> {
        match item {
            Message::Text(s) => self.send_text(&s),
            Message::Binary(b) => self.send_binary(&b),
            Message::Close(frame) => {
                let res = match frame {
                    Some(f) => self.ws.close_with_code_and_reason(f.code, &f.reason),
                    None => self.ws.close(),
                };
                res.map_err(|e| Error::ConnectionError(js_err(&e)))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        // The browser owns the send buffer and exposes no drain event, so
        // there is nothing to await without polling `bufferedAmount`.
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let _ = self.ws.close();
        Poll::Ready(Ok(()))
    }
}

impl Drop for WebSocketStream {
    fn drop(&mut self) {
        // Clear the listeners *before* the `Closure`s in `_closures` are freed
        // (fields drop after this method), so the JS WebSocket never holds a
        // dangling function pointer.
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
        self.ws.set_onerror(None);
        let st = self.ws.ready_state();
        if st == web_sys::WebSocket::CONNECTING || st == web_sys::WebSocket::OPEN {
            let _ = self.ws.close();
        }
    }
}

/// Best-effort string from a thrown `JsValue`: a JS `Error.message`, a
/// `DOMException`/object `message` property (what `WebSocket::new` throws),
/// a bare string, or a debug fallback.
fn js_err(e: &JsValue) -> String {
    if let Some(err) = e.dyn_ref::<js_sys::Error>() {
        return String::from(err.message());
    }
    if let Ok(msg) = js_sys::Reflect::get(e, &JsValue::from_str("message"))
        && let Some(s) = msg.as_string()
    {
        return s;
    }
    if let Some(s) = e.as_string() {
        return s;
    }
    format!("{e:?}")
}
