export function includeWorker() { }

self.onmessage = async event => {
    {
        console.log('[Worker] Received message:', event.data);

        const [module, memory, ptr, baseUrl] = event.data;

        console.log('[Worker] Base url from main thread:', baseUrl);
        console.log('[Worker] import.meta.url:', import.meta.url);
        console.log('[Worker] globalThis.location.origin url:', globalThis.location.origin);
        console.log('[Worker] globalThis.location.href url:', globalThis.location.href);
        console.log('[Worker] import.meta.env.BASE_URL:', import.meta.env.BASE_URL);

        // const wasmUrl = new URL("../hydra-node/hydra_node.js", globalThis.location.href);

        // console.log('[Worker] Loading WASM module from: ' + wasmUrl);

        let wasmPkg;
        try {
            wasmPkg = await import("../hydra-node/hydra_node.js");

            console.log('[Worker] Loaded WASM package from relative path');
        } catch (err) {

            console.error('[Worker] Failed to load WASM package:', err);

            wasmPkg = await import('hydra-node');

            console.log('[Worker] Loaded WASM package from hydra-node alias');
        }

        console.log('[Worker] Loaded WASM package');

        const init = wasmPkg.default;
        const initialised = await init(module, memory).catch(err => {
            {
                // Propagate to main `onerror`:
                setTimeout(() => {
                    {
                        throw err;
                    }
                });
                // Rethrow to keep promise rejected and prevent execution of further commands:
                throw err;
            }
        });

        console.log('[Worker] Initialised WASM package:', initialised);

        await wasmPkg.async_worker_entry_point(ptr);

        // Clean up thread resources. Depending on what you're doing with the thread, this might
        // not be what you want. (For example, if the thread spawned some javascript tasks
        // and exited, this is going to cancel those tasks.) But if you're using threads in the
        // usual native way (where you spin one up to do some work until it finisheds) then
        // you'll want to clean up the thread's resources.

        // Free memory (stack, thread-locals) held (in the wasm linear memory) by the thread.
        initialised.__wbindgen_thread_destroy();
        // Tell the browser to stop the thread.
        close();
    }
};

self.onerror = err => {
    {
        console.error('[Worker] Error:', err);
    }
};