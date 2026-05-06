// Vite entry — imports the wasm-bindgen output. The default export
// is `init(module, memory)`, which boots the wasm and runs the
// #[wasm_bindgen(start)] function. wasmt's workerSpawner.js
// autodetects this script's URL by walking the page's <script>
// tags and resolving the first ES-module import path it finds — so
// no extra plumbing is needed in the common case.
import init from './pkg/wasmt_vite_sample.js';

await init();
