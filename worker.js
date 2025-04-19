import init, * as wasmPkg from 'hydra-node';

self.onmessage = async event => {
    console.log('[Worker] Received message:', event.data);
    const [module, memory, ptr, isAsync] = event.data;

    const initialised = await init(module, memory).catch(err => {
        setTimeout(() => { throw err; });
        throw err;
    });

    console.log('[Worker] Initialised WASM package:', initialised);

    if (isAsync) {
        await wasmPkg.async_worker_entry_point(ptr);
    } else {
        wasmPkg.worker_entry_point(ptr);
    }

    initialised.__wbindgen_thread_destroy();
    close();
};

self.onerror = err => {
    console.error('[Worker] Error:', err);
};

export function includeWorker() { }

export default class MyWorker {
    constructor() {
        return new Worker(new URL('./worker.js', import.meta.url), {
            type: 'module',
            name: 'MyWorker'
        });
    }
}
