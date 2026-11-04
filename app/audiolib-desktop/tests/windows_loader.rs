// app/audiolib-desktop/tests/windows_loader.rs
//! Exercise the real Windows executable, including imports and its manifest.
#![cfg(windows)]

use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn desktop_executable_loads_before_initializing_user_data() {
    // --version exits before loading preferences, opening a library or audio.
    let mut child = Command::new(env!("CARGO_BIN_EXE_AudioLibExplorer"))
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the built desktop executable");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().expect("query desktop exit").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            // Only this test's --version child is terminated. A loader dialog
            // must fail the test instead of hanging an unattended check.
            let _ = child.kill();
            let _ = child.wait();
            panic!("desktop executable did not reach its --version entry point");
        }
        thread::sleep(Duration::from_millis(25));
    }
    let output = child.wait_with_output().expect("collect desktop version");
    assert!(
        output.status.success(),
        "desktop loader failed: {:?}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        concat!("AudioLibExplorer ", env!("CARGO_PKG_VERSION"))
    );
}
