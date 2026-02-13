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
  const worker = new MyWorker();
  worker.postMessage([module, memory, ptr, isAsync]);
  return worker;
}