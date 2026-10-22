#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    foreman::run_native(std::env::args().nth(1))
}

#[cfg(target_arch = "wasm32")]
fn main() {}
