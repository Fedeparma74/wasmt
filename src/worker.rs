use std::future::Future;
use std::pin::Pin;
use wasm_bindgen::prelude::{JsValue, wasm_bindgen};

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
        is_async: bool,
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
    // 1. Prepare the pointer to the work to be executed
    //    Double-boxing because `dyn FnOnce` is unsized and so `Box<dyn FnOnce()>` has
    //    an undefined layout (although I think in practice its a pointer and a length?).
    let ptr = Box::into_raw(Box::new(Box::new(f) as Box<dyn FnOnce() -> T>));

    // 2. Get references to the WASM module and memory
    //    These are provided by the main thread (wasm-bindgen magic)
    let module_val = wasm_bindgen::module();
    let memory_val = wasm_bindgen::memory();

    // 3. Call the imported JavaScript function to create the worker
    //    and send the initial data. 'catch' in #[wasm_bindgen] intercepts JS errors
    //    and converts them to JsValue errors in Rust.
    //    If the worker creation or message sending fails, we need to clean up the pointer.
    match spawn_worker_and_send_data(&module_val, &memory_val, ptr as u32, false) {
        Ok(worker) => worker,
        Err(err) => {
            // If the worker couldn't be created or the message couldn't be sent,
            // we need to clean up the pointer ourselves, as the worker won't do it.
            web_sys::console::error_1(
                &"JavaScript failed to spawn worker or post message. Cleaning up Rust pointer."
                    .into(),
            );
            std::mem::drop(unsafe { Box::from_raw(ptr) }); // Clean up the Box<dyn FnOnce()>
            panic!("Failed to spawn worker: {:?}", err);
        }
    }
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

    // 3. Call the imported JavaScript function to create the worker
    //    and send the initial data. 'catch' in #[wasm_bindgen] intercepts JS errors
    //    and converts them to JsValue errors in Rust.
    //    If the worker creation or message sending fails, we need to clean up the pointer.
    match spawn_worker_and_send_data(&module_val, &memory_val, ptr as u32, true) {
        Ok(worker) => worker,
        Err(err) => {
            // If the worker couldn't be created or the message couldn't be sent,
            // we need to clean up the pointer ourselves, as the worker won't do it.
            web_sys::console::error_1(
                &"JavaScript failed to spawn worker or post message. Cleaning up Rust pointer."
                    .into(),
            );
            std::mem::drop(unsafe { Box::from_raw(ptr) }); // Clean up the Box<Pin<Box<dyn Future>>>
            panic!("Failed to spawn worker: {:?}", err);
        }
    }
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
