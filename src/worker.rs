use std::future::Future;
use std::pin::Pin;
use wasm_bindgen::{
    JsCast,
    prelude::{JsValue, wasm_bindgen},
};
use web_sys::{Blob, Url, WorkerOptions};

#[wasm_bindgen(module = "/workerSpawner.js")]
extern "C" {
    // Define the signature of the JS function
    // Return type is web_sys::Worker. On errors, JS throws, which becomes a JsValue error in Rust.
    // Alternative: Return type Result<web_sys::Worker, JsValue> and adjust error handling in JS.
    #[wasm_bindgen(js_name = spawnWorkerAndSendData, catch)] // catch intercepts JS exceptions
    fn spawn_worker_and_send_data(
        module: &JsValue,
        memory: &JsValue,
        ptr: u32,
        script_path: &str,
    ) -> Result<web_sys::Worker, JsValue>;
}

#[wasm_bindgen(module = "/worker.js")]
extern "C" {
    #[wasm_bindgen(js_name = includeWorker)]
    fn include_worker();
}

pub fn spawn_blocking<T>(f: impl FnOnce() -> T + 'static) -> web_sys::Worker
where
    T: 'static,
{
    let script = format!(
        "
        import init, * as wasm_bindgen from '{}';
        globalThis.wasm_bindgen = wasm_bindgen;
        self.onmessage = async event => {{
            const [module, memory, ptr] = event.data;

            let initialised = await init(module, memory).catch(err => {{
                // Propagate to main `onerror`:
                setTimeout(() => {{
                    throw err;
                }});
                // Rethrow to keep promise rejected and prevent execution of further commands:
                throw err;
            }});

            wasm_bindgen.worker_entry_point(ptr);

            // Clean up thread resources. Depending on what you're doing with the thread, this might
            // not be what you want. (For example, if the thread spawned some javascript tasks
            // and exited, this is going to cancel those tasks.) But if you're using threads in the
            // usual native way (where you spin one up to do some work until it finisheds) then
            // you'll want to clean up the thread's resources.
          
            // Free memory (stack, thread-locals) held (in the wasm linear memory) by the thread.
            initialised.__wbindgen_thread_destroy();
            // Tell the browser to stop the thread.
            close();
        }};

        self.onerror = err => {{
            console.error(err);
        }};
        ",
        get_script_path().unwrap()
    );
    let blob_property_bag = web_sys::BlobPropertyBag::new();
    blob_property_bag.set_type("application/javascript");
    let blob = Blob::new_with_str_sequence_and_options(
        &js_sys::Array::of1(&JsValue::from_str(&script)),
        &blob_property_bag,
    )
    .expect("Unable to create blob with JavaScript glue code.");
    let worker_options = WorkerOptions::new();
    worker_options.set_type(web_sys::WorkerType::Module);
    let worker = web_sys::Worker::new_with_options(
        Url::create_object_url_with_blob(&blob)
            .expect("failed to create object url")
            .as_str(),
        &worker_options,
    )
    .expect("failed to create worker");
    // Double-boxing because `dyn FnOnce` is unsized and so `Box<dyn FnOnce()>` has
    // an undefined layout (although I think in practice its a pointer and a length?).
    let ptr = Box::into_raw(Box::new(Box::new(f) as Box<dyn FnOnce() -> T>));

    // See worker script for the format of this message.
    let msg: js_sys::Array = [
        &wasm_bindgen::module(),
        &wasm_bindgen::memory(),
        &JsValue::from(ptr as u32),
    ]
    .into_iter()
    .collect();

    if let Err(e) = worker.post_message(&msg) {
        // We expect the worker to deallocate the box, but if there was an error then
        // we'll do it ourselves.
        std::mem::drop(unsafe { Box::from_raw(ptr) });
        panic!("failed to post message: {e:?}");
    }

    worker
}

pub fn spawn<F>(future: F) -> web_sys::Worker
where
    F: Future<Output = ()> + 'static,
{
    // 1. Prepare the pointer to the work to be executed
    let ptr = Box::into_raw(Box::new(
        Box::pin(future) as Pin<Box<dyn Future<Output = ()>>>
    ));

    // 2. Get references to the WASM module and memory
    //    These are provided by the main thread (wasm-bindgen magic)
    let module_val = wasm_bindgen::module();
    let memory_val = wasm_bindgen::memory();

    let base_url = if let Some(window) = web_sys::window() {
        window.location().origin().unwrap_or_default()
    } else if let Ok(worker) = js_sys::global().dyn_into::<web_sys::WorkerGlobalScope>() {
        worker.origin()
    } else {
        unreachable!("Not in a worker or window context")
    };

    // 3. Call the imported JavaScript function to create the worker
    //    and send the initial data. 'catch' in #[wasm_bindgen] intercepts JS errors.
    let result = spawn_worker_and_send_data(&module_val, &memory_val, ptr as u32, &base_url);

    // 4. Error handling in case the JS function fails
    if result.is_err() {
        // If the worker couldn't be created or the message couldn't be sent,
        // we need to clean up the pointer ourselves, as the worker won't do it.
        // IMPORTANT: This cleanup logic is crucial!
        web_sys::console::error_1(
            &"JavaScript failed to spawn worker or post message. Cleaning up Rust pointer.".into(),
        );
        std::mem::drop(unsafe { Box::from_raw(ptr) }); // Clean up the Box<Pin<Box<dyn Future>>>
    }

    // Return the result (Ok(Worker) or Err(JsValue))
    result.expect("Failed to spawn worker or post message")
}

fn get_script_path() -> Option<String> {
    js_sys::eval(
        r"
        (() => {
            try {
                throw new Error();
            } catch (e) {
                let parts = e.stack.match(/(?:\(|@)(\S+):\d+:\d+/);
                return parts[1];
            }
        })()
        ",
    )
    .ok()?
    .as_string()
}

#[wasm_bindgen]
pub fn worker_entry_point(ptr: u32) {
    let work = unsafe { Box::from_raw(ptr as *mut Box<dyn FnOnce()>) };
    (*work)();
}

#[wasm_bindgen]
pub async fn async_worker_entry_point(ptr: u32) {
    let work = unsafe { Box::from_raw(ptr as *mut Pin<Box<dyn Future<Output = ()>>>) };
    (*work).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::WorkerGlobalScope;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    fn test_spawn() {
        let worker = spawn(async {
            assert!(js_sys::global().dyn_into::<WorkerGlobalScope>().is_ok());
        });

        assert!(worker.is_object());
        assert!(worker.to_string().as_string().unwrap().contains("Worker"));

        worker.terminate();
    }

    #[wasm_bindgen_test]
    fn test_spawn_blocking() {
        let worker = spawn_blocking(|| {
            assert!(js_sys::global().dyn_into::<WorkerGlobalScope>().is_ok());
        });

        assert!(worker.is_object());
        assert!(worker.to_string().as_string().unwrap().contains("Worker"));

        worker.terminate();
    }
}
