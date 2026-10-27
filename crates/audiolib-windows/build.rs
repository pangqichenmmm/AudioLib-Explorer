// crates/audiolib-windows/build.rs
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        // The native window integration test calls the same Comctl32 subclass
        // API as the desktop. Give that test its own v6 activation context.
        println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-tests=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
    println!("cargo:rerun-if-env-changed=CPAL_ASIO_DIR");
    println!("cargo:rerun-if-changed=src/asio_shim.cpp");
    if std::env::var_os("CARGO_FEATURE_ASIO").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
    {
        let sdk = std::env::var_os("CPAL_ASIO_DIR")
            .expect("Set CPAL_ASIO_DIR to the approved ASIO SDK path");
        cc::Build::new()
            .cpp(true)
            .file("src/asio_shim.cpp")
            .include(std::path::PathBuf::from(sdk).join("common"))
            .compile("audiolib_asio_shim");
    }
}
