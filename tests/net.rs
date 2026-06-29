//! WebSocket (`net` feature) integration tests.
//!
//! These talk to a real echo server, so they are gated behind the
//! `net-tests` feature and need both a browser test runner and network
//! access. Point them at your own server with `WS_ECHO_URL` at build time:
//!
//! ```text
//! WS_ECHO_URL=ws://127.0.0.1:9001 cargo test --features net-tests
//! ```
//!
//! The default is a public echo server that prepends a greeting text frame,
//! so the helpers below skip frames until the expected echo arrives.
#![cfg(feature = "net-tests")]

use std::time::Duration;

use futures::{SinkExt, Stream, StreamExt};
use wasm_bindgen_test::*;
use wasmt::net::{Message, WsStream, connect};
use wasmt::time::timeout;

wasm_bindgen_test_configure!(run_in_browser);

/// Echo server URL. Override with `WS_ECHO_URL` at build time.
const ECHO_URL: &str = match option_env!("WS_ECHO_URL") {
    Some(u) => u,
    None => "wss://echo.websocket.org",
};

const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// Await the next stream item, failing the test on timeout / stream end.
async fn next_msg<S>(ws: &mut S) -> Message
where
    S: Stream<Item = Result<Message, wasmt::net::Error>> + Unpin,
{
    match timeout(RECV_TIMEOUT, ws.next()).await {
        Ok(Some(Ok(m))) => m,
        Ok(Some(Err(e))) => panic!("stream error: {e}"),
        Ok(None) => panic!("stream ended before a message arrived"),
        Err(_) => panic!("timed out waiting for a message"),
    }
}

/// Read frames until the exact `want` echo arrives (skipping greetings),
/// bounded by a frame budget.
async fn expect_echo<S>(ws: &mut S, want: &Message)
where
    S: Stream<Item = Result<Message, wasmt::net::Error>> + Unpin,
{
    for _ in 0..8 {
        let got = next_msg(ws).await;
        if &got == want {
            return;
        }
        // Otherwise it's a server greeting / keepalive; keep reading.
    }
    panic!("never received the expected echo: {want:?}");
}

#[wasm_bindgen_test]
async fn connect_and_text_echo() {
    let mut ws = connect(ECHO_URL).await.expect("connect failed");
    let payload = Message::Text("wasmt-hello".into());
    ws.send(payload.clone()).await.expect("send failed");
    expect_echo(&mut ws, &payload).await;
}

#[wasm_bindgen_test]
async fn binary_echo_roundtrips_through_shared_memory() {
    // Guards the shared-memory send path: bytes must be copied out of the
    // SharedArrayBuffer before send_with_array_buffer.
    let mut ws = connect(ECHO_URL).await.expect("connect failed");
    let payload = Message::Binary((0u8..=255).collect());
    ws.send(payload.clone()).await.expect("send failed");
    expect_echo(&mut ws, &payload).await;
}

#[wasm_bindgen_test]
async fn interleaved_text_and_binary_keep_order() {
    let mut ws = connect(ECHO_URL).await.expect("connect failed");
    let seq = [
        Message::Text("a".into()),
        Message::Binary(vec![1, 2, 3]),
        Message::Text("b".into()),
        Message::Binary(vec![4, 5, 6]),
        Message::Text("c".into()),
    ];
    for m in &seq {
        ws.send(m.clone()).await.expect("send failed");
    }
    // Drain the greeting first (if any), then assert exact order of our frames.
    let mut got = Vec::new();
    while got.len() < seq.len() {
        let m = next_msg(&mut ws).await;
        // Ignore a server greeting text that isn't one of ours.
        let ours = seq.iter().any(|s| s == &m);
        if ours {
            got.push(m);
        }
    }
    assert_eq!(got, seq, "frames arrived out of order");
}

#[wasm_bindgen_test]
async fn split_read_and_write_halves() {
    let ws = connect(ECHO_URL).await.expect("connect failed");
    let (mut sink, mut stream) = ws.split();
    let payload = Message::Text("split-echo".into());
    sink.send(payload.clone()).await.expect("send failed");
    expect_echo(&mut stream, &payload).await;
}

#[wasm_bindgen_test]
async fn byte_adapter_roundtrips_bytes() {
    use futures::{AsyncReadExt, AsyncWriteExt};

    let mut ws = connect(ECHO_URL).await.expect("connect failed");
    // The echo server's greeting is a *text* frame, which the binary-only
    // byte stream rejects. Drain it at the message level first.
    // Best-effort: pull one frame if the server greets; ignore on timeout.
    let _ = timeout(Duration::from_secs(2), ws.next()).await;

    let mut io = WsStream::new(ws);
    let data = b"byte-stream-payload";
    io.write_all(data).await.expect("write failed");

    let mut buf = vec![0u8; data.len()];
    timeout(RECV_TIMEOUT, io.read_exact(&mut buf))
        .await
        .expect("read timed out")
        .expect("read failed");
    assert_eq!(&buf, data);
}

#[wasm_bindgen_test]
async fn close_yields_close_frame_then_terminates() {
    use futures::stream::FusedStream;

    let mut ws = connect(ECHO_URL).await.expect("connect failed");
    // Client-initiated close: the browser runs the handshake and fires
    // `onclose`, which we surface in-band as `Ok(Message::Close(_))`.
    SinkExt::close(&mut ws).await.expect("close failed");

    let mut saw_close = false;
    for _ in 0..8 {
        match timeout(RECV_TIMEOUT, ws.next()).await {
            Ok(Some(Ok(Message::Close(_)))) => {
                saw_close = true;
                break;
            }
            Ok(Some(Ok(_))) => continue, // greeting / late echo
            Ok(Some(Err(e))) => panic!("unexpected stream error: {e}"),
            Ok(None) => break,
            Err(_) => panic!("timed out waiting for close"),
        }
    }
    assert!(saw_close, "never observed Message::Close");
    // After Close the fused stream is terminated and yields None.
    assert!(ws.is_terminated(), "stream not terminated after Close");
    assert!(ws.next().await.is_none(), "stream did not end after Close");
    // And a send after close is rejected rather than silently dropped.
    assert_eq!(
        ws.send(Message::Text("after-close".into())).await,
        Err(wasmt::net::Error::AlreadyClosed),
    );
}

#[wasm_bindgen_test]
async fn byte_stream_rejects_text_frame() {
    use futures::AsyncReadExt;

    let mut ws = connect(ECHO_URL).await.expect("connect failed");
    // Make sure a text frame is in flight (the server also greets with text).
    ws.send(Message::Text("not-bytes".into()))
        .await
        .expect("send failed");

    let mut io = WsStream::new(ws);
    let mut buf = [0u8; 64];
    let res = timeout(RECV_TIMEOUT, io.read(&mut buf))
        .await
        .expect("read timed out");
    assert!(
        matches!(&res, Err(e) if e.kind() == std::io::ErrorKind::InvalidData),
        "binary byte stream must reject a text frame as InvalidData, got {res:?}",
    );
}

#[wasm_bindgen_test]
async fn connect_to_bad_url_errors() {
    // Nothing listening here; connect must reject rather than hang.
    let res = timeout(
        Duration::from_secs(10),
        connect("ws://127.0.0.1:1/definitely-not-listening"),
    )
    .await
    .expect("connect did not settle in time");
    assert!(res.is_err(), "expected a connection error");
}

#[wasm_bindgen_test]
async fn echo_works_on_a_pool_worker() {
    // The socket is !Send, so build and drive it entirely inside one pinned
    // worker task; only the (Send) success flag crosses back.
    let ok = wasmt::spawn_pinned(|| async {
        let mut ws = connect(ECHO_URL).await.expect("connect failed");
        let payload = Message::Text("worker-echo".into());
        ws.send(payload.clone()).await.expect("send failed");
        for _ in 0..8 {
            if next_msg(&mut ws).await == payload {
                return true;
            }
        }
        false
    })
    .join()
    .await
    .expect("pinned task panicked");
    assert!(ok, "echo did not round-trip on a pool worker");
}
