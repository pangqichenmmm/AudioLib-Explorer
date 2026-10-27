// crates/audiolib-windows/tests/window_redraw.rs
//! Native off-screen fixtures; no input injection or working application data.
#![cfg(windows)]

use audiolib_windows::window_redraw::install;
use windows::Win32::{
    Foundation::{HWND, WPARAM},
    UI::WindowsAndMessaging::WM_SETREDRAW,
};
use windows::{
    Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, IsIconic, IsWindowVisible, SW_HIDE, SW_MINIMIZE,
        SW_SHOWNOACTIVATE, SendMessageW, ShowWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
    },
    core::w,
};

struct OwnedWindow(HWND);
impl OwnedWindow {
    fn new() -> Self {
        // SAFETY: Built-in class, no borrowed create data, no owner. The
        // scratch popup is off screen, never activated and only this test's.
        Self(unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("AudioLib redraw fixture"),
                WS_POPUP,
                -30000,
                -30000,
                16,
                16,
                None,
                None,
                None,
                None,
            )
            .unwrap()
        })
    }
    fn redraw(&self, enabled: bool) {
        // SAFETY: Synchronous scalar-only message to this thread's fixture.
        unsafe {
            SendMessageW(
                self.0,
                WM_SETREDRAW,
                Some(WPARAM(usize::from(enabled))),
                None,
            );
        }
    }
    fn visible(&self) -> bool {
        // SAFETY: The RAII fixture owns this live HWND.
        unsafe { IsWindowVisible(self.0).as_bool() }
    }
}
impl Drop for OwnedWindow {
    fn drop(&mut self) {
        // SAFETY: Destruction occurs once on the fixture's creating thread.
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

#[test]
fn menu_redraw_preserves_visibility_and_explicit_hide_and_minimize_still_work() {
    let fixture = OwnedWindow::new();
    // SAFETY: Only this nonactivating off-screen fixture is shown/hidden.
    unsafe {
        let _ = ShowWindow(fixture.0, SW_SHOWNOACTIVATE);
    }
    assert!(fixture.visible());
    fixture.redraw(false);
    assert!(
        !fixture.visible(),
        "baseline reproduces DefWindowProc's hidden style"
    );
    fixture.redraw(true);
    assert!(fixture.visible());

    assert!(install(fixture.0.0 as usize).unwrap());
    assert!(!install(fixture.0.0 as usize).unwrap());
    for _ in 0..20 {
        fixture.redraw(false);
        assert!(
            fixture.visible(),
            "menu updates must never expose the window underneath"
        );
        fixture.redraw(true);
        assert!(fixture.visible());
    }
    // SAFETY: The fixture remains on its owner thread; no other HWND is changed.
    unsafe {
        let _ = ShowWindow(fixture.0, SW_HIDE);
    }
    fixture.redraw(false);
    fixture.redraw(true);
    assert!(
        !fixture.visible(),
        "a late menu update must not reveal a hidden window"
    );
    // SAFETY: This test owns the nonactivating fixture throughout.
    unsafe {
        let _ = ShowWindow(fixture.0, SW_SHOWNOACTIVATE);
        let _ = ShowWindow(fixture.0, SW_MINIMIZE);
    }
    fixture.redraw(false);
    fixture.redraw(true);
    // SAFETY: Read-only query of the live owned fixture.
    assert!(unsafe { IsIconic(fixture.0).as_bool() });
}

#[test]
fn redraw_guard_rejects_invalid_handles_and_other_threads() {
    assert!(install(0).is_err());
    let fixture = OwnedWindow::new();
    let hwnd = fixture.0.0 as usize;
    assert!(
        std::thread::spawn(move || install(hwnd).is_err())
            .join()
            .unwrap()
    );
}
