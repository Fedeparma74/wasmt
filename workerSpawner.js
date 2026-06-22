// JS bootstrap for the wasmt runtime. Imported from Rust via
// `#[wasm_bindgen(module = "/workerSpawner.js")]`. The companion
// `worker.js` (also imported from Rust so wasm-bindgen copies it
// next to this file) is the actual worker entry.

const SPAWNER_URL = new URL(import.meta.url);
const WORKER_URL = new URL('./worker.js', SPAWNER_URL);

let cachedWasmJsUrl = null;

// wasm-bindgen package name (from the `WASMT_WASM_PKG` build env), set
// from Rust via `setWasmPkgName` before the first spawn. Lets us derive
// the glue URL purely from this snippet's own location — no DOM walking,
// no app-side plumbing — for the standard wasm-bindgen `--target web`
// layout where the glue sits at `<glue-dir>/<pkg>.js` and this snippet
// at `<glue-dir>/snippets/<crate>-<hash>/workerSpawner.js`.
let wasmPkgName = null;

/**
 * Override the wasm-bindgen-glue URL the worker imports. Call this
 * once if autodetection picks the wrong file for your bundle.
 *
 * @param {string|URL} url
 */
export function setWasmJsUrl(url) {
    cachedWasmJsUrl = String(url);
}

/**
 * Record the wasm-bindgen package name so the glue URL can be derived
 * relative to this snippet. Called from Rust with `WASMT_WASM_PKG`.
 *
 * @param {string} name
 */
export function setWasmPkgName(name) {
    wasmPkgName = String(name);
}

// Filenames that should never be picked as the wasm-bindgen glue
// itself (they are wasmt's own snippets). `run.js` is excluded from
// being *picked*, but we still inspect it to follow its imports.
const NOT_GLUE = /\/(workerSpawner|worker)\.js$/i;

// Heuristic that a script's *content* looks like the wasm-bindgen
// output: it has the `__wbg_init` symbol and exports `default`.
const LOOKS_LIKE_GLUE = /__wbg_init/;

// Strip JS line and block comments from `src` so the from-import
// regex below doesn't pick up `from "..."` patterns that appear
// inside a docstring or a // comment.
function stripJsComments(src) {
    return src
        // /* ... */ block comments (non-greedy, dotall via [\s\S]).
        .replace(/\/\*[\s\S]*?\*\//g, '')
        // // line comments to end-of-line.
        .replace(/\/\/[^\n\r]*/g, '');
}

// A path candidate is "obviously bogus" if it's just placeholder-y
// punctuation that no real bundler would emit.
const OBVIOUS_PLACEHOLDER = /^[.\s]*$/;

function detectWasmJsUrl() {
    if (cachedWasmJsUrl) return cachedWasmJsUrl;

    // Fast path: a worker's own bootstrap stashed the URL on `self`
    // before loading us. Nested spawn_* calls from workers (no DOM)
    // resolve through here.
    if (typeof self !== 'undefined' && self.__wasmt_wasm_js_url) {
        cachedWasmJsUrl = String(self.__wasmt_wasm_js_url);
        return cachedWasmJsUrl;
    }

    // Fast path #2: page can pre-stash the URL on `globalThis`
    // (e.g., a Vite/Webpack bundle entry that knows where the glue
    // lives). Avoids any XHR on browsers where synchronous XHR is
    // restricted (Firefox aggressively throttles it on main, which
    // can manifest as a multi-second UI freeze on first spawn).
    if (typeof globalThis !== 'undefined' && globalThis.__wasmt_wasm_js_url) {
        cachedWasmJsUrl = String(globalThis.__wasmt_wasm_js_url);
        return cachedWasmJsUrl;
    }

    // Primary auto path: derive the glue URL from this snippet's own
    // location plus the package name (`WASMT_WASM_PKG`). wasm-bindgen
    // `--target web` always emits `<glue-dir>/<pkg_snake>.js` with our
    // snippets under `<glue-dir>/snippets/<crate>-<hash>/`, so the glue
    // is `../../<pkg_snake>.js` from here. This needs no DOM and works
    // under bundlers (Nuxt/Vite/Webpack) that load the glue via dynamic
    // `import()` rather than a discoverable `<script type=module>`.
    if (wasmPkgName) {
        const pkgSnake = wasmPkgName.replace(/-/g, '_');
        const resolved = new URL(`../../${pkgSnake}.js`, SPAWNER_URL).href;
        // Guard against a degenerate name that would resolve back to one
        // of our own snippets.
        if (!NOT_GLUE.test(resolved)) {
            cachedWasmJsUrl = resolved;
            return resolved;
        }
    }

    // From main: walk every <script type=module>. Try in order:
    //   1. If the script tag has inline content (no `src`), inspect it.
    //   2. If `src`, sync-fetch and inspect — but bounded by a
    //      timeout so a slow dev server can't freeze the UI.
    // Pick the first non-snippet URL whose content imports glue or
    // looks like glue itself.
    if (typeof document !== 'undefined') {
        const candidates = Array.from(
            document.querySelectorAll('script[type=module]')
        );
        for (const tag of candidates) {
            const src = tag.getAttribute('src');
            const entryUrl = src
                ? new URL(src, document.baseURI).href
                : document.baseURI;
            if (src && entryUrl === SPAWNER_URL.href) continue;

            // Content source: inline text first, then sync XHR.
            let text = src ? null : (tag.textContent || '');
            if (src) {
                try {
                    const xhr = new XMLHttpRequest();
                    xhr.open('GET', entryUrl, false);
                    // Cap the wait; Firefox honors timeouts on sync
                    // XHR (3s is generous for a script that's
                    // already on the page).
                    try { xhr.timeout = 3000; } catch (_) { /* ignore */ }
                    xhr.send();
                    if (xhr.status !== 200) continue;
                    text = xhr.responseText;
                } catch (_) {
                    // Sync XHR forbidden / timed out / errored —
                    // continue to next candidate. If autodetect
                    // ultimately fails, the user-facing error below
                    // points at setWasmJsUrl.
                    continue;
                }
            }
            if (!text) continue;

            // Strip comments so `from "..."` inside a JS comment
            // isn't matched as a real import.
            const stripped = stripJsComments(text);
            const matches = [...stripped.matchAll(/from\s+['"]([^'"]+)['"]/g)];
            for (const m of matches) {
                const path = m[1];
                if (NOT_GLUE.test(path)) continue;
                if (OBVIOUS_PLACEHOLDER.test(path)) continue;
                const resolved = new URL(path, entryUrl).href;
                if (resolved === SPAWNER_URL.href) continue;
                cachedWasmJsUrl = resolved;
                return resolved;
            }

            if (LOOKS_LIKE_GLUE.test(stripped)) {
                cachedWasmJsUrl = entryUrl;
                return entryUrl;
            }
        }
    }

    throw new Error(
        '[wasmt] could not autodetect the wasm-bindgen JS glue URL. ' +
        'Call setWasmJsUrl(url) from your bundle entry, pointing at ' +
        'the wasm-bindgen JS file that exports `default(module, memory)`. ' +
        'Alternatively set `globalThis.__wasmt_wasm_js_url` before the ' +
        'first wasmt::spawn call.'
    );
}

function newWorker() {
    return new Worker(WORKER_URL, { type: 'module', name: 'wasmt-worker' });
}

/**
 * Spawn a long-lived blocking-pool worker. The worker boots the wasm
 * module and enters `blocking_pool_main(poolPtr)`, draining the pool's
 * job queue until idle-timeout or shutdown.
 */
export function spawnBlockingPoolWorker(module, memory, poolPtr) {
    const worker = newWorker();
    worker.postMessage({
        kind: 'blocking-pool',
        module, memory, poolPtr,
        wasmJsUrl: detectWasmJsUrl(),
    });
    return worker;
}

/**
 * Spawn a long-lived runtime worker. The worker boots the wasm module
 * and enters `runtime_worker_main(handlePtr)` until shutdown.
 */
export function spawnRuntimeWorker(module, memory, handlePtr) {
    const worker = newWorker();
    worker.postMessage({
        kind: 'runtime',
        module, memory, handlePtr,
        wasmJsUrl: detectWasmJsUrl(),
    });
    return worker;
}
