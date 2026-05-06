// Long-lived worker entry. Loaded via `new Worker(URL, {type: 'module'})`
// where URL is `new URL('./worker.js', workerSpawner.js url)` — i.e. a
// regular same-origin file copied next to workerSpawner.js by
// wasm-bindgen. Imports the wasm-bindgen JS glue at the URL the
// spawning side passes in `msg.wasmJsUrl`.

// wasm-bindgen's generated glue (and wasm-bindgen-test setup) make
// bare references to globals that don't exist in a dedicated worker:
// `SharedWorker`, `document`, etc. Stub them with no-op shims so
// `instanceof` / property-access don't throw ReferenceError.
if (typeof SharedWorker === 'undefined') self.SharedWorker = class {};
if (typeof document === 'undefined') {
    const noop = new Proxy(function () {}, {
        get() { return noop; },
        set() { return true; },
        apply() { return noop; },
        construct() { return noop; },
    });
    self.document = noop;
}
if (typeof window === 'undefined') self.window = self;

self.onmessage = async event => {
    const msg = event.data;
    // Stash the glue URL so this worker's own workerSpawner.js
    // (loaded by the wasm-bindgen import below) can find it for
    // any nested spawn_* it makes.
    self.__wasmt_wasm_js_url = msg.wasmJsUrl;
    let initialised;
    try {
        const wasmPkg = await import(msg.wasmJsUrl);
        const initFn = wasmPkg.default || wasmPkg.__wbg_init || wasmPkg.init;
        if (typeof initFn !== 'function') {
            throw new Error(
                '[wasmt worker] no init function exported by ' + msg.wasmJsUrl +
                '; exports = ' + Object.keys(wasmPkg).join(',')
            );
        }
        initialised = await initFn(msg.module, msg.memory);
        switch (msg.kind) {
            case 'runtime':
                // Async: the worker yields to the JS event loop while
                // pinned tasks are alive so their `Promise.then`
                // callbacks fire. Resolves on shutdown.
                await wasmPkg.runtime_worker_main(msg.handlePtr);
                break;
            case 'blocking-pool':
                wasmPkg.blocking_pool_main(msg.poolPtr);
                break;
            default:
                throw new Error('[wasmt worker] unknown message kind: ' + msg.kind);
        }
    } catch (err) {
        console.error('[wasmt worker] failed:', err);
        setTimeout(() => { throw err; });
        return;
    } finally {
        try { initialised && initialised.__wbindgen_thread_destroy(); } catch (_) {}
        close();
    }
};

self.onerror = err => {
    console.error('[wasmt worker] error:', err);
};

// Marker so wasm-bindgen copies this file into its JS output bundle
// alongside workerSpawner.js. Imported from Rust via `extern "C"`.
export function includeWorker() { /* intentionally empty */ }
