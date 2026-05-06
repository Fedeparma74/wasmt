# wasmt — Vite sample

Minimal Vite project that exercises every spawn primitive (`spawn`,
`spawn_blocking`, `spawn_local`, `spawn_pinned`), `JoinSet`, and
`wasmt::sync::mpsc`. Logs go to the browser console.

## Run it

```bash
# from this directory:
npm install
npm run dev      # builds the wasm with wasm-pack, then starts vite
```

Open http://localhost:5173/ and watch the devtools console.

## What's set up

- `.cargo/config.toml` configures the atomics target features and
  shared-memory linker flags.
- `vite.config.js`:
  - `vite-plugin-wasm` for `import init from "./pkg/...js"`.
  - A tiny middleware that adds `Cross-Origin-Opener-Policy:
    same-origin` and `Cross-Origin-Embedder-Policy: require-corp`
    on every dev/preview response so `SharedArrayBuffer` is
    available (without these the wasm can't boot).
- `main.js` imports the wasm-bindgen output and calls `init()`.
  Vite picks this up as a `<script type="module">` on
  `index.html`, which is what wasmt's runtime walks to autodetect
  the wasm-bindgen JS-glue URL.

## Production builds

`npm run build` (or `vite build`) emits a static bundle. Make sure
your hosting layer also sets the COOP/COEP headers on responses —
without them, browsers will refuse to give the wasm access to
shared memory and the runtime will fail to boot.
