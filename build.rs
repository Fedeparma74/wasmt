use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=WASMT_WASM_PKG");
    println!("cargo:rerun-if-changed=worker.js.tpl");

    let pkg_name = std::env::var("WASMT_WASM_PKG")
        .unwrap_or_else(|_| std::env::var("CARGO_PKG_NAME").unwrap());

    let template = std::fs::read_to_string("worker.js.tpl")
        .expect("failed to read worker.js.tpl — is the file missing from the crate root?");

    let generated = template.replace("__WASMT_WASM_PKG__", &pkg_name);

    let dest = Path::new("worker.js");
    // Only write if content changed to avoid unnecessary rebuilds
    let needs_write = std::fs::read_to_string(dest)
        .map(|existing| existing != generated)
        .unwrap_or(true);

    if needs_write {
        std::fs::write(dest, generated).expect("failed to write worker.js");
    }
}
