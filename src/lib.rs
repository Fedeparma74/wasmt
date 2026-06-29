#![feature(stdarch_wasm_atomic_wait)]

//! `wasmt` — a multi-threaded async runtime for `wasm32-unknown-unknown`.
//!
//! Provides Tokio-shaped [`spawn`], [`spawn_blocking`], [`spawn_local`],
//! [`JoinHandle`], [`AbortHandle`], async [`sleep`], and a worker-pool
//! [`Runtime`] backed by Web Workers + `SharedArrayBuffer`. The
//! scheduler is work-stealing with per-worker FIFO deques, a LIFO
//! "next-task" slot for message-passing locality, and `Atomics.wait`/
//! `notify`-based parking.
//!
//! # Build requirements
//!
//! - `wasm32-unknown-unknown` with `+atomics +bulk-memory +mutable-globals`
//! - Linked with `--shared-memory --import-memory`
//! - `build-std = ['std', 'panic_abort']` (nightly + `rust-src`)
//!
//! See `.cargo/config.toml` in the crate for the canonical setup.
//!
//! # Hosting requirements
//!
//! Pages must be served with cross-origin isolation so
//! `SharedArrayBuffer` is available:
//!
//! ```text
//! Cross-Origin-Opener-Policy: same-origin
//! Cross-Origin-Embedder-Policy: require-corp
//! ```
//!
//! # Bundler / dev-server compatibility
//!
//! `wasmt` ships two JS snippets (`workerSpawner.js` and `worker.js`)
//! that wasm-bindgen places under `<glue-dir>/snippets/<crate>/...`.
//! Workers are spawned with `new Worker(<same-origin url>, {type:
//! 'module'})` and load the wasm-bindgen JS glue via dynamic
//! `import()` at runtime — there is no build-time templating, no
//! per-consumer string substitution, and no Vite-specific syntax.
//!
//! At spawn time the JS bootstrap autodetects the wasm-bindgen glue
//! URL by walking the page's `<script type=module>` tags and reading
//! their source. This works out of the box for:
//!
//! - **Plain `wasm-pack build --target web`** output served from any
//!   static host (the glue is the entry script).
//! - **Vite** (`vite-plugin-wasm` or similar) — Vite's entry script
//!   imports the glue, autodetect resolves it.
//! - **Webpack 5 / esbuild / Rollup** — any layout where a
//!   `<script type=module>` import chain reaches the glue.
//!
//! For non-standard setups (custom loaders, service-worker-driven
//! installs, or pages without `<script>` tags) call
//! [`runtime::set_wasm_js_url`] once before the first spawn,
//! pointing at the wasm-bindgen JS file that exports
//! `default(module, memory)`.

#[cfg(not(target_arch = "wasm32"))]
compile_error!("wasmt only supports wasm32-unknown-unknown");

#[cfg(not(all(
    target_feature = "atomics",
    target_feature = "bulk-memory",
    target_feature = "mutable-globals"
)))]
compile_error!(
    "wasmt requires `+atomics +bulk-memory +mutable-globals`. Set RUSTFLAGS or .cargo/config.toml accordingly."
);

#[cfg(feature = "net")]
pub mod net;
pub mod runtime;
pub mod sync;
pub mod task;
pub mod time;
pub mod utils;

#[cfg(feature = "net")]
pub use net::{
    CloseFrame, Error, Message, State, WebSocketStream, WsStream, connect, connect_with_protocols,
};
pub use runtime::{Builder, Handle, Runtime, block_on, set_wasm_js_url, spawn_on_main};
pub use task::{
    AbortHandle, JoinError, JoinHandle, JoinSet, LocalJoinHandle, spawn, spawn_blocking,
    spawn_local, spawn_pinned, yield_now,
};
pub use time::{sleep, sleep_blocking};
pub use utils::{ThreadId, available_parallelism, thread_id};
