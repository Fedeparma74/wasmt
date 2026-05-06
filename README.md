# wasmt

A multi-threaded async runtime for `wasm32-unknown-unknown`, shaped after Tokio.

`wasmt` runs futures on a long-lived pool of Web Workers backed by
`SharedArrayBuffer`. The scheduler is work-stealing with per-worker
local FIFO deques, a single-task LIFO slot for message-passing
locality, bounded local deques that overflow to a global injector
when a producer goes wild, and `Atomics.wait`/`notify` parking. A
dynamic blocking pool serves `spawn_blocking`. A single timer driver
on the main thread handles all `sleep` / `timeout` / `interval`
calls. `spawn_pinned` runs `!Send` futures (anything `JsValue`-bearing
— `reqwest`, `gloo`, `web-sys`, `js-sys`, …) on the pool with each
future stuck to one worker; pool workers yield to their JS event
loop while pinned tasks are alive so JS Promises resolve correctly.

## At a glance

```rust
use std::time::Duration;

#[wasm_bindgen(start)]
async fn main() {
    // Send work — runs on the work-stealing pool.
    let h = wasmt::spawn(async {
        wasmt::time::sleep(Duration::from_millis(100)).await;
        42u32
    });
    log::info!("got {}", h.join().await.unwrap());

    // !Send work — pinned to a pool worker.
    let h = wasmt::spawn_pinned(|| async {
        let resp = reqwest::get("https://example.com").await?;
        Ok::<_, reqwest::Error>(resp.status().as_u16())
    });
    log::info!("status {}", h.join().await.unwrap().unwrap());

    // Blocking work — runs on a dedicated worker.
    let h = wasmt::spawn_blocking(|| {
        std::thread::sleep(Duration::from_millis(50));
        7u32
    });
    log::info!("blocking returned {}", h.join().await.unwrap());

    // Sync primitives are re-exported from tokio::sync.
    let (tx, mut rx) = wasmt::sync::mpsc::channel::<u32>(8);
    wasmt::spawn(async move { tx.send(99).await.unwrap(); });
    log::info!("got {}", rx.recv().await.unwrap());
}
```

## Build requirements

Compiled with **nightly** (build-std needed) and the atomics target
feature. Drop this into `.cargo/config.toml`:

```toml
[unstable]
build-std = ['std', 'panic_abort']

[target.wasm32-unknown-unknown]
runner = 'wasm-bindgen-test-runner'

[build]
target = "wasm32-unknown-unknown"
rustflags = [
    '-C', 'target-feature=+atomics,+bulk-memory,+mutable-globals',
    '-C', 'link-arg=--shared-memory',
    '-C', 'link-arg=--import-memory',
    '-C', 'link-arg=--max-memory=4294967296',
    '-C', 'link-arg=--export=__wasm_init_tls',
    '-C', 'link-arg=--export=__tls_size',
    '-C', 'link-arg=--export=__tls_align',
    '-C', 'link-arg=--export=__tls_base',
]
```

## Hosting requirements (cross-origin isolation)

Pages must be served with these headers so `SharedArrayBuffer` is
available:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

Without them, the runtime panics at startup with a clear diagnostic.
(`Builder::build` and the lazy `default_handle()` both verify
`crossOriginIsolated` before spawning any worker.)

## Browser support

`wasmt` uses standard wasm threading + Worker APIs. Anything that
ships those works:

| Browser                  | Minimum         | Notes                                                                                                                |
| ------------------------ | --------------- | -------------------------------------------------------------------------------------------------------------------- |
| Chrome / Edge / Chromium | 91+             | Stable; `SharedArrayBuffer` requires COOP/COEP.                                                                      |
| Firefox                  | 79+             | Stable; same COOP/COEP                                                                                               |
| Safari (macOS / iOS)     | 16.4+           | First version with `SharedArrayBuffer` + module workers + `Atomics.wait/notify`. Earlier versions are not supported. |
| Mobile Chrome / Firefox  | follows desktop |                                                                                                                      |

The runtime depends on:
- `WebAssembly.Memory` with `shared: true` (cross-origin isolated)
- Module-type Web Workers (`new Worker(URL, {type: 'module'})`)
- `Atomics.wait` / `Atomics.notify` (and the wasm `memory.atomic.*` ops)
- `postMessage` of `WebAssembly.Module` and `Memory`

If your audience includes Safari < 16.4 or any browser without
`SharedArrayBuffer`, the runtime will fail to start (with a clear
message). Either keep a non-threaded fallback path or require modern
browsers.

## API surface

### Spawning

```rust
wasmt::spawn(future)                                 // Send + 'static
wasmt::spawn_blocking(closure)                       // Send + 'static
wasmt::spawn_local(future)                           // 'static, no Send
wasmt::spawn_pinned(|| async { ... })                // !Send future on a pool worker
wasmt::yield_now().await                             // cooperative yield
```

All four return a `JoinHandle<T>` that is `Send` when `T: Send`.
`JoinHandle` implements `Future`, so you can `.await` it directly or
call `.join().await` for parity with Tokio.

### Cancellation

```rust
let h = wasmt::spawn(...);
let abort = h.abort_handle();    // cloneable
abort.abort();                   // observed at next yield
abort.is_aborted();
abort.is_finished();
h.is_finished();
let _ = h.join().await;          // -> Err(JoinError::Cancelled)
```

### Time

```rust
wasmt::time::sleep(Duration)                       -> Sleep   (Send)
wasmt::time::sleep_until(Instant)                  -> Sleep   (Send)
wasmt::time::timeout(Duration, future)             -> Timeout (Send)
wasmt::time::timeout_at(Instant, future)           -> Timeout (Send)
wasmt::time::interval(Duration)                    -> Interval
wasmt::time::interval_at(Instant, Duration)        -> Interval
wasmt::time::sleep_blocking(Duration)              // worker only — never main
wasmt::time::Instant::now()                        -> Instant (Date.now-based; cross-thread comparable)
```

A single `setTimeout` arms the next-deadline; expired entries fire
in batch. Workers post a `wasmt_timer_kick` to main when they insert
an earlier deadline.

### Runtime / shutdown

```rust
let rt = wasmt::Builder::new_multi_thread()
    .worker_threads(8)
    .max_blocking_threads(32)
    .blocking_idle_timeout(Duration::from_secs(10))
    .build();

rt.spawn(async { ... });
rt.shutdown_timeout(Duration::from_secs(5)).await;
```

The default singleton (`wasmt::spawn(...)` etc.) is created lazily on
first use, sized to `navigator.hardwareConcurrency`.

### `JoinSet`

```rust
let mut set = wasmt::JoinSet::new();
for i in 0..16 {
    set.spawn(async move { i * 2 });
}
while let Some((id, result)) = set.join_next().await {
    log::info!("task {} -> {:?}", id.as_u64(), result);
}
```

Drops cancel pending tasks (matches Tokio).

### `wasmt::sync` (feature-gated, on by default)

Re-exports `tokio::sync` — `Mutex`, `RwLock`, `Notify`, `Semaphore`,
`OnceCell`, `mpsc`, `oneshot`, `broadcast`, `watch`. Same source as
on native, no extra Tokio runtime required.

### Filling `std` gaps on `wasm32-unknown-unknown`

A few `std` APIs are stubbed-out on this target. wasmt fills the
ones that fit naturally; for the rest, point at the established
ecosystem crate.

| `std` API (broken on `wasm32-unknown-unknown`)             | Use instead                                                                                                                                                                                                                     |
| ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `std::thread::available_parallelism()`                     | `wasmt::available_parallelism()`                                                                                                                                                                                                |
| `std::thread::current().id()`                              | `wasmt::thread_id()` returning `ThreadId::{Main, Pool(i), Other}`                                                                                                                                                               |
| `std::time::Instant::now()` / `SystemTime::now()` (panics) | [`web-time`](https://crates.io/crates/web-time) — drop-in replacement backed by `performance.now()` / `Date.now()`                                                                                                              |
| `std::thread::spawn`                                       | `wasmt::spawn_blocking` (semantically `.await` rather than `.join()`)                                                                                                                                                           |
| `std::fs`, `std::net`, `std::env`, `std::process`          | No browser analog; use [`gloo`](https://crates.io/crates/gloo), [`reqwest`](https://crates.io/crates/reqwest) (with `wasm` feature), [`web-sys`](https://crates.io/crates/web-sys), [`idb`](https://crates.io/crates/idb), etc. |

`std::sync::*` (Mutex, RwLock, Arc, OnceLock, …) works fine on
workers with `+atomics`; main-thread code must avoid contending on
those primitives because `Atomics.wait` is illegal there.

## Bundler / dev-server compatibility

`wasmt` ships two JS snippets (`workerSpawner.js` and `worker.js`)
that wasm-bindgen places under
`<glue-dir>/snippets/<crate>/...`. Workers are spawned via
`new Worker(<same-origin url>, { type: 'module' })` and load the
wasm-bindgen JS glue with a runtime `import()` — no build-time
templating, no Vite-specific syntax.

At spawn time the JS bootstrap autodetects the wasm-bindgen glue URL
by walking the page's `<script type="module">` tags and reading
their source for the first non-snippet `from "..."`. This works out
of the box for:

- Plain `wasm-pack build --target web` output served from any static host.
- Vite (with `vite-plugin-wasm` / `vite-plugin-cross-origin-isolation`).
- Webpack 5 / esbuild / Rollup.

For non-standard layouts (custom loaders, service-worker installs,
or pages with no `<script>` tag), call
`wasmt::runtime::set_wasm_js_url(url)` once before the first
spawn, pointing at the wasm-bindgen JS file that exports
`default(module, memory)`.

A complete Vite sample lives in [examples/vite/](examples/vite/).

## License

MIT OR Apache-2.0.
