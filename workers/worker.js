self.onmessage = async event => {
  const [module, memory, ptr, name] = event.data;

  let base_url = globalThis.location.origin;

  let js;
  try {
    js = await import(new URL('./pkg/' + name + '.js', base_url));
  } catch (_) {
    try {
      js = await import(new URL('./wasm-bindgen-test.js', base_url));
    } catch (_) {
      console.error("couldn't find " + new URL('./pkg/' + name + '.js', base_url));
      close();
    }
  }

  const init = js.default;
  const worker_entry_point = js.worker_entry_point;

  let initOutput = await init(module, memory).catch(err => {
    // Propagate to main `onerror`:
    setTimeout(() => {
      throw err;
    });
    // Rethrow to keep promise rejected and prevent execution of further commands:
    throw err;
  });

  worker_entry_point(ptr);

  // Clean up thread resources. Depending on what you're doing with the thread, this might
  // not be what you want. (For example, if the thread spawned some javascript tasks
  // and exited, this is going to cancel those tasks.) But if you're using threads in the
  // usual native way (where you spin one up to do some work until it finisheds) then
  // you'll want to clean up the thread's resources.

  // Free memory (stack, thread-locals) held (in the wasm linear memory) by the thread.
  initOutput.__wbindgen_thread_destroy();
  // Tell the browser to stop the thread.
  close();
}