// crates/audiolib-windows/src/paint_exposure.rs
//! Tell native exposure apart from the application's own redraw requests.
//!
//! winit delivers both as `RedrawRequested`: the toolkit asks for a frame with
//! `RedrawWindow(RDW_INTERNALPAINT)`, which leaves the update region empty, while
//! Windows invalidates pixels (restore, uncovering without composition, display
//! changes) by adding to it. A window subclass sees each `WM_PAINT` before winit
//! validates the region and records whether pixels were invalidated, so a caller
//! can repaint the whole surface only after real exposure and keep ordinary
//! animation frames partial.
//!
//! Own HWND and UI thread only, one window per thread. The subclass forwards
//! every message untouched, removes itself on `WM_NCDESTROY`, and never retains
//! message pointers. It calls only `SetWindowSubclass`, `RemoveWindowSubclass` and
//! `DefSubclassProc`, which the system comctl32 exports by name even when the
//! process has no comctl32 v6 manifest (test binaries have none).

#![cfg(windows)]

use std::{cell::Cell, ffi::c_void};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    Graphics::Gdi::GetUpdateRect,
    System::Threading::GetCurrentThreadId,
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{GetWindowThreadProcessId, WM_NCDESTROY, WM_PAINT},
    },
};

/// Subclass identity ("ALPX"); unique among this process's subclasses.
const SUBCLASS_ID: usize = 0x414c_5058;

thread_local! {
    /// The one watched window on this UI thread and whether Windows invalidated
    /// its pixels since the last `take_exposure`. Kept here rather than in the
    /// subclass reference data: `GetWindowSubclass` is not exported by name from
    /// the comctl32 that loads without a v6 manifest, so the watcher must never
    /// need to query its own subclass.
    static WATCHED: Cell<Option<(usize, bool)>> = const { Cell::new(None) };
}

/// Return whether Windows invalidated pixels of `hwnd` since the previous call,
/// and clear that record. Installs the watcher on first use and then reports
/// `true`, because paints before installation were not observed. Any handle that
/// cannot be watched (null, foreign, other thread, a second window on this
/// thread, failed install) also reports `true`, so callers fall back to
/// repainting everything.
pub fn take_exposure(hwnd: usize) -> bool {
    let handle = HWND(hwnd as *mut c_void);
    if handle.0.is_null() {
        return true;
    }
    let mut process_id = 0;
    // SAFETY: scalar identity queries; a foreign HWND or owner thread is refused.
    let (owner_thread, current_thread) = unsafe {
        (
            GetWindowThreadProcessId(handle, Some(&mut process_id)),
            GetCurrentThreadId(),
        )
    };
    if owner_thread == 0 || owner_thread != current_thread || process_id != std::process::id() {
        return true;
    }
    match WATCHED.with(Cell::get) {
        Some((watched, exposed)) if watched == hwnd => {
            WATCHED.with(|state| state.set(Some((watched, false))));
            exposed
        }
        Some(_) => true,
        None => {
            // SAFETY: own UI-thread HWND. The subclass keeps no reference data;
            // its state is the thread-local above, cleared on WM_NCDESTROY.
            if unsafe { SetWindowSubclass(handle, Some(subclass_proc), SUBCLASS_ID, 0) }.as_bool() {
                WATCHED.with(|state| state.set(Some((hwnd, false))));
            }
            true
        }
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    if id == SUBCLASS_ID {
        let handle = hwnd.0 as usize;
        if message == WM_PAINT {
            // SAFETY: read-only query of this window's pending update region,
            // before winit's handler validates it. Nothing is erased or painted.
            if unsafe { GetUpdateRect(hwnd, None, false) }.as_bool() {
                WATCHED.with(|state| {
                    if state.get().is_some_and(|(watched, _)| watched == handle) {
                        state.set(Some((handle, true)));
                    }
                });
            }
        } else if message == WM_NCDESTROY {
            // SAFETY: remove exactly this procedure/ID on the owning thread.
            unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
            }
            WATCHED.with(|state| {
                if state.get().is_some_and(|(watched, _)| watched == handle) {
                    state.set(None);
                }
            });
        }
    }
    // SAFETY: forward each native message exactly once and untouched.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::{
        Graphics::Gdi::{InvalidateRect, RDW_INTERNALPAINT, RedrawWindow, ValidateRect},
        UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, SendMessageW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
            WS_POPUP, WS_VISIBLE,
        },
    };
    use windows::core::w;

    #[test]
    fn internal_paint_requests_are_not_exposure_but_invalidation_is() {
        // STATIC windows on this thread: no class registration. Windows keeps no
        // update region for a hidden window, so they are visible but off-screen,
        // never activated and kept off the taskbar. WM_PAINT is sent directly so
        // the result does not depend on the message queue.
        let create = || {
            // SAFETY: plain window creation; destroyed at the end of the test.
            unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    w!(""),
                    WS_POPUP | WS_VISIBLE,
                    -32000,
                    -32000,
                    64,
                    64,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .expect("create an off-screen test window")
        };
        let hwnd = create();
        let other = create();
        let handle = hwnd.0 as usize;
        let other_handle = other.0 as usize;
        let paint = |window: HWND| unsafe {
            SendMessageW(window, WM_PAINT, None, None);
        };
        // Installation reports exposure: earlier paints were not observed.
        assert!(take_exposure(handle));
        assert!(!take_exposure(handle));
        // A second window on the same thread is not watched: always a full frame.
        assert!(take_exposure(other_handle));
        assert!(take_exposure(other_handle));
        // The toolkit's own frame request leaves the update region empty. Any
        // region left from creation is validated first.
        // SAFETY: own window on this thread.
        unsafe {
            let _ = ValidateRect(Some(hwnd), None);
            let _ = RedrawWindow(Some(hwnd), None, None, RDW_INTERNALPAINT);
        }
        paint(hwnd);
        assert!(!take_exposure(handle));
        // Native invalidation is exposure, reported once.
        // SAFETY: own window on this thread.
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        paint(hwnd);
        assert!(take_exposure(handle));
        assert!(!take_exposure(handle));
        // SAFETY: own window; WM_NCDESTROY removes the subclass and its state.
        unsafe { DestroyWindow(hwnd) }.expect("destroy the test window");
        // A destroyed handle cannot be watched and keeps the full repaint.
        assert!(take_exposure(handle));
        // With the first window gone, the other one can be watched.
        assert!(take_exposure(other_handle));
        assert!(!take_exposure(other_handle));
        // SAFETY: own window on this thread.
        unsafe { DestroyWindow(other) }.expect("destroy the second test window");
        // Other handles are never watched.
        assert!(take_exposure(0));
    }
}
