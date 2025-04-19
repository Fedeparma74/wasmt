import MyWorker from './worker?worker';

/**
 * Creates the worker and sends the initial data.
 * Called from Rust.
 * @param {WebAssembly.Module} module - The compiled WASM module.
 * @param {WebAssembly.Memory} memory - The WASM memory (often SharedArrayBuffer).
 * @param {number} ptr - A pointer for the worker entry point.
 * @param {boolean} isAsync - Whether the function to run in the worker is async.
 * @returns {Worker} The created Worker instance.
 */
export function spawnWorkerAndSendData(module, memory, ptr, isAsync) {
  console.log("[JS Helper] Spawning worker...");
  try {
    const worker = new MyWorker();

    // Send the initial data to the worker
    // The worker expects [module, memory, ptr]
    worker.postMessage([module, memory, ptr, isAsync]);
    console.log("[JS Helper] Initial data posted to worker.");

    // Optional: Add listener for messages FROM the worker
    worker.onmessage = (event) => {
      console.log('[Main Thread] Received from worker:', event.data);
    };

    worker.onerror = (event) => {
      console.error('[Main Thread] Error received from worker:', event);
    };

    return worker; // Return the Worker instance

  } catch (e) {
    console.error("[JS Helper] Failed to spawn worker:", e);
    // Re-throw error so Rust can handle it
    throw e;
  }
}