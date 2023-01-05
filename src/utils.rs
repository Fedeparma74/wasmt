use wasm_bindgen::JsCast;
use web_sys::WorkerGlobalScope;

pub fn is_worker_scope() -> bool {
    js_sys::global().dyn_into::<WorkerGlobalScope>().is_ok()
}

#[cfg(test)]
mod tests {
    use crate::thread;

    use super::*;

    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    fn test_is_worker_scope() {
        assert!(!is_worker_scope());
        thread::spawn(async move {
            assert!(is_worker_scope());
        });
        thread::spawn_local(async move {
            assert!(!is_worker_scope());
        });
    }
}
