fn main() {
    // Whisper on this device: the `local-speech` feature everywhere but Android, where the phone
    // only records and the foreman server reads what it heard.
    println!("cargo:rustc-check-cfg=cfg(local_speech)");
    if std::env::var_os("CARGO_FEATURE_LOCAL_SPEECH").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android")
    {
        println!("cargo:rustc-cfg=local_speech");
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
