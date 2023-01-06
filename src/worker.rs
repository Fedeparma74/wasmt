use futures::Future;
use std::pin::Pin;
use wasm_bindgen::prelude::{wasm_bindgen, JsValue};
use web_sys::{Blob, Url, WorkerOptions};

const PKG_NAME: Option<&str> = option_env!("PKG_NAME");

// TODO: is this even needed? Why not just wrap the closure in a future and use spawn<F>(future: F)?
// pub fn spawn(f: impl FnOnce() + Send + 'static) -> web_sys::Worker {
//     let worker_js = include_str!("../workers/worker.js");

//     let blob = Blob::new_with_str_sequence_and_options(
//         &js_sys::Array::of1(&JsValue::from_str(worker_js)),
//         web_sys::BlobPropertyBag::new().type_("application/javascript"),
//     )
//     .expect("failed to create blob");

//     let worker = web_sys::Worker::new_with_options(
//         Url::create_object_url_with_blob(&blob)
//             .expect("failed to create object url")
//             .as_str(),
//         WorkerOptions::new().type_(web_sys::WorkerType::Module),
//     )
//     .expect("failed to create worker");

//     // Double-boxing because `dyn FnOnce` is unsized and so `Box<dyn FnOnce()>` has
//     // an undefined layout (although I think in practice its a pointer and a length?).
//     let ptr = Box::into_raw(Box::new(Box::new(f) as Box<dyn FnOnce()>));

//     // See `worker.js` for the format of this message.
//     let msg: js_sys::Array = [
//         &wasm_bindgen::module(),
//         &wasm_bindgen::memory(),
//         &JsValue::from(ptr as u32),
//         &JsValue::from(NAME),
//     ]
//     .into_iter()
//     .collect();
//     if let Err(e) = worker.post_message(&msg) {
//         // We expect the worker to deallocate the box, but if there was an error then
//         // we'll do it ourselves.
//         std::mem::drop(unsafe { Box::from_raw(ptr) });
//         panic!("failed to post message: {e:?}");
//     }

//     worker
// }

pub fn spawn<F>(future: F) -> web_sys::Worker
where
    F: Future<Output = ()> + Send + 'static,
{
    let worker_js = include_str!("../workers/async-worker.js");

    let blob = Blob::new_with_str_sequence_and_options(
        &js_sys::Array::of1(&JsValue::from_str(worker_js)),
        web_sys::BlobPropertyBag::new().type_("application/javascript"),
    )
    .expect("failed to create blob");

    let worker = web_sys::Worker::new_with_options(
        Url::create_object_url_with_blob(&blob)
            .expect("failed to create object url")
            .as_str(),
        WorkerOptions::new().type_(web_sys::WorkerType::Module),
    )
    .expect("failed to create worker");

    // Double-boxing because `dyn FnOnce` is unsized and so `Box<dyn FnOnce()>` has
    // an undefined layout (although I think in practice its a pointer and a length?).
    let ptr = Box::into_raw(Box::new(
        Box::pin(future) as Pin<Box<dyn Future<Output = ()>>>
    ));

    let pkg_name = match PKG_NAME {
        Some(name) => name.to_string(),
        None => env!("CARGO_PKG_NAME").to_string(),
    };

    // See `worker.js` for the format of this message.
    let msg: js_sys::Array = [
        &wasm_bindgen::module(),
        &wasm_bindgen::memory(),
        &JsValue::from(ptr as u32),
        &JsValue::from(pkg_name),
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
}
