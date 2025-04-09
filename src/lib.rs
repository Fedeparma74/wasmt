pub mod task;
pub mod time;
pub mod utils;
mod worker;

#[cfg(not(target_arch = "wasm32"))]
compile_error!("This crate can only be compiled for wasm32-unknown-unknown target");
#[cfg(not(any(
    target_feature = "atomics",
    target_feature = "bulk-memory",
    target_feature = "mutable-globals"
)))]
compile_error!(
    "Make sure to build std with `RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals'`"
);
