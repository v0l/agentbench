#[cfg(all(feature = "speech", not(target_arch = "wasm32")))]
pub mod speech;
pub mod voice;
