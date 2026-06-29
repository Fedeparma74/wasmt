//! Generates `worker.js` — the long-lived worker entry that wasm-bindgen copies
//! next to `workerSpawner.js` as a snippet.
//!
//! When `WASMT_WASM_PKG` is set, the worker can STATICALLY import the
//! wasm-bindgen glue at `../../<pkg_snake>.js`. A bundler then bakes a decoupled
//! glue copy into the worker chunk, and a plain `--target web` host resolves the
//! same explicit relative file — one universal worker that works on dev servers,
//! production bundles, and with no bundler, with no runtime glue-URL detection
//! or app-side override. We produce it from `worker.bundled.js.tmpl` by
//! substituting the package name.
//!
//! When `WASMT_WASM_PKG` is unset we can't know the glue's filename, so we fall
//! back to `worker.runtime.js`, which autodetects the glue URL at spawn time.
//!
//! `WASMT_WASM_PKG` is also read by the crate via `option_env!` (see
//! `runtime::publish_wasm_pkg_name`), so a change to it recompiles the crate AND
//! reruns this script in lockstep — the regenerated `worker.js` is always the
//! one wasm-bindgen embeds.

use std::{env, fs, path::Path};

/// Replaced in `worker.bundled.js.tmpl` with `../../<pkg_snake>.js`.
const PLACEHOLDER: &str = "__WASMT_GLUE_MODULE__";

fn main() {
    println!("cargo:rerun-if-env-changed=WASMT_WASM_PKG");
    println!("cargo:rerun-if-changed=worker.bundled.js.tmpl");
    println!("cargo:rerun-if-changed=worker.runtime.js");

    let dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset");
    let dir = Path::new(&dir);
    let out = dir.join("worker.js");

    let contents = match env::var("WASMT_WASM_PKG") {
        Ok(pkg) if !pkg.trim().is_empty() => {
            let pkg = pkg.trim();
            // Guard the value we splice into a JS import string: a wasm-bindgen
            // package name is `[A-Za-z0-9_-]+`. Reject anything else so a stray
            // quote/slash can't break or inject into the generated import.
            assert!(
                pkg.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "WASMT_WASM_PKG = {pkg:?} is not a valid package name (expected [A-Za-z0-9_-]+)"
            );
            // wasm-bindgen names the glue `<pkg_snake>.js` (hyphens → underscores).
            let pkg_snake = pkg.replace('-', "_");
            let glue = format!("../../{pkg_snake}.js");
            let tmpl = fs::read_to_string(dir.join("worker.bundled.js.tmpl"))
                .expect("read worker.bundled.js.tmpl");
            assert!(
                tmpl.contains(PLACEHOLDER),
                "worker.bundled.js.tmpl is missing the {PLACEHOLDER} placeholder"
            );
            tmpl.replace(PLACEHOLDER, &glue)
        }
        _ => fs::read_to_string(dir.join("worker.runtime.js")).expect("read worker.runtime.js"),
    };

    // Write only when the content actually differs, so we don't bump the mtime
    // (and trigger a spurious wasm-bindgen snippet rebuild) on every `cargo`
    // invocation.
    let unchanged = fs::read_to_string(&out)
        .map(|cur| cur == contents)
        .unwrap_or(false);
    if !unchanged {
        fs::write(&out, contents).expect("write generated worker.js");
    }
}
