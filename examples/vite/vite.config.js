import { defineConfig } from 'vite';
import wasm from 'vite-plugin-wasm';

// SharedArrayBuffer (and therefore wasmt) requires cross-origin
// isolation — set the COOP/COEP response headers on every request.
const crossOriginIsolation = {
    name: 'cross-origin-isolation',
    configureServer(server) {
        server.middlewares.use((_req, res, next) => {
            res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
            res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
            next();
        });
    },
    configurePreviewServer(server) {
        server.middlewares.use((_req, res, next) => {
            res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
            res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
            next();
        });
    },
};

export default defineConfig({
    plugins: [wasm(), crossOriginIsolation],
    server: { port: 5173 },
    optimizeDeps: { exclude: ['./pkg/wasmt_vite_sample.js'] },
});
