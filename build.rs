use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=WASMT_WASM_PKG");

    let manifest_dir =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));

    let tpl_path = manifest_dir.join("worker.js.tpl");
    let dest_path = manifest_dir.join("worker.js");

    println!("cargo:rerun-if-changed={}", tpl_path.display());

    let pkg_name = match std::env::var("WASMT_WASM_PKG") {
        Ok(name) if !name.is_empty() => name,
        _ => {
            let fallback = std::env::var("CARGO_PKG_NAME").unwrap();
            println!(
                "cargo:warning=wasmt: WASMT_WASM_PKG not set, defaulting to \
                 CARGO_PKG_NAME=\"{fallback}\". If wasmt is used as a dependency, \
                 set WASMT_WASM_PKG to the consumer's npm package name \
                 (e.g. WASMT_WASM_PKG=hydra-node)."
            );
            fallback
        }
    };

    let template = std::fs::read_to_string(&tpl_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", tpl_path.display()));

    let generated = template.replace("__WASMT_WASM_PKG__", &pkg_name);

    // Only write if content changed to avoid unnecessary rebuilds
    let needs_write = std::fs::read_to_string(&dest_path)
        .map(|existing| existing != generated)
        .unwrap_or(true);

    if needs_write {
        match std::fs::write(&dest_path, &generated) {
            Ok(_) => {
                println!("cargo:warning=wasmt: generated worker.js with import from '{pkg_name}'")
            }
            Err(e) => panic!(
                "wasmt: failed to write {}: {e}\n\
                 If wasmt is a git/registry dependency the source is read-only.\n\
                 Use wasmt as a path dependency or provide worker.js manually.",
                dest_path.display()
            ),
        }
    }
}
