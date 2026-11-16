pub mod composer;
pub mod form;
pub mod transcript;
pub mod viewer;
pub mod widgets;

#[cfg(all(feature = "agent", not(target_arch = "wasm32")))]
pub mod chat;
#[cfg(all(feature = "agent", not(target_arch = "wasm32")))]
mod host;
#[cfg(all(feature = "agent", not(target_arch = "wasm32")))]
pub mod pane;
#[cfg(all(feature = "agent", not(target_arch = "wasm32")))]
pub mod settings;

#[cfg(all(feature = "agent", not(target_arch = "wasm32")))]
pub use host::*;
