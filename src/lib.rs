pub mod task;
pub mod time;
pub mod utils;
#[cfg(all(
    target_feature = "atomics",
    target_feature = "bulk-memory",
    target_feature = "mutable-globals"
))]
mod worker;

#[cfg(not(target_arch = "wasm32"))]
compile_error!("This crate can only be compiled for wasm32-unknown-unknown target");
