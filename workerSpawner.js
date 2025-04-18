/**
 * Creates the worker and sends the initial data.
 * Called from Rust.
 * @param {WebAssembly.Module} module - The compiled WASM module.
 * @param {WebAssembly.Memory} memory - The WASM memory (often SharedArrayBuffer).
 * @param {number} ptr - A pointer for the worker entry point.
 * @param {string} scriptPath - The path to the worker script.
 * @returns {Worker} The created Worker instance.
 */
export function spawnWorkerAndSendData(module, memory, ptr, scriptPath) {
  console.log("[JS Helper] Spawning worker...");
  try {
    const workerString = `
      self.onmessage = async event => {
        const baseUrl = globalThis.location.origin;

        const [module, memory, ptr, scriptPath] = event.data;

        const wasmPkg = await import(new URL(scriptPath, baseUrl));

        const init = wasmPkg.default;
        let initialised = await init(module, memory).catch(err => {
            setTimeout(() => {
                throw err;
            });
            // Rethrow to keep promise rejected and prevent execution of further commands:
            throw err;
        });

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
      };

      self.onerror = err => {
          console.error('[Worker] Error:', err);
      };
    `;

    const blob = new Blob([workerString], { type: 'application/javascript' });
    const workerURL = URL.createObjectURL(blob);
    const worker = new Worker(workerURL, {
      type: 'module' // Important for ES Modules in the worker
    });

    // const worker = new Worker(new URL('./worker.js', import.meta.url), {
    //   type: 'module' // Important for ES Modules in the worker
    // });

    // Send the initial data to the worker
    // The worker expects [module, memory, ptr]
    worker.postMessage([module, memory, ptr, scriptPath]);
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