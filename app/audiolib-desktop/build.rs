// app/audiolib-desktop/build.rs
fn main() {
    // ElementHandle-based tests need Slint metadata even in optimized builds.
    // Unset overrides retain the normal profile-dependent packaging default.
    let emit_debug_info = match std::env::var("SLINT_EMIT_DEBUG_INFO").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => std::env::var("PROFILE").as_deref() != Ok("release"),
    };
    println!("cargo:rerun-if-env-changed=SLINT_EMIT_DEBUG_INFO");
    slint_build::compile_with_config(
        "ui/app-window.slint",
        slint_build::CompilerConfiguration::new()
            .with_style("fluent".into())
            .with_debug_info(emit_debug_info),
    )
    .expect("compile AudioLib desktop UI");
    println!("cargo:rerun-if-changed=ui/app-window.slint");
    println!("cargo:rerun-if-changed=ui/search-task-tabs.slint");
    println!("cargo:rerun-if-changed=ui/audition-panel.slint");
    println!("cargo:rerun-if-changed=ui/icons");
    println!("cargo:rerun-if-changed=ui/surfaces");
    println!("cargo:rerun-if-changed=ui/theme.slint");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=../../resources/icons/AudioLibExplorer.ico");
    println!("cargo:rerun-if-changed=../../resources/icons/AudioLibExplorer.png");
    // Unoptimized builds overflow the default 1 MiB main-thread stack at startup.
    // Reserve 16 MiB (address space only, committed on use) for every profile, so
    // debug runs need no editbin and both profiles start the window the same way.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:16777216");
    }
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        winresource::WindowsResource::new()
            .set_icon("../../resources/icons/AudioLibExplorer.ico")
            .set_manifest_file("app.manifest")
            .set("ProductName", "AudioLib Explorer")
            .set("FileDescription", "AudioLib Explorer sound library")
            .set("OriginalFilename", "AudioLibExplorer.exe")
            .compile()
            .expect("compile Windows application icon and manifest");
    }
}
