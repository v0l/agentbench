fn main() {
    // The camera that scans a pairing code lives in its own NDK library, which `ndk-sys` does not
    // link for us.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-lib=camera2ndk");
    }
    println!("cargo:rerun-if-changed=../../packaging/windows/icon.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../packaging/windows/icon.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon not embedded: {e}");
        }
    }
}
