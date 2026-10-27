// crates/audiolib-windows/src/window_redraw.rs
//! Keep a desktop HWND visible while Slint 1.17 rebuilds its native menu.
//!
//! Slint sends WM_SETREDRAW(false/true) around menu changes. DefWindowProc removes
//! WS_VISIBLE for false (despite leaving the old pixels on screen), so input can
//! reach the application underneath. True can also reveal an intentionally hidden
//! window. Consume this paint optimization on our top-level window and schedule a
//! normal asynchronous repaint instead. Never activate, raise, show or minimize it.
//! https://learn.microsoft.com/windows/win32/gdi/wm-setredraw

#![cfg(windows)]

use std::{ffi::c_void, io};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    Graphics::Gdi::{RDW_FRAME, RDW_INVALIDATE, RedrawWindow},
    System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
    UI::{
        Shell::{DefSubclassProc, GetWindowSubclass, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{GetWindowThreadProcessId, WM_NCDESTROY, WM_SETREDRAW},
    },
};

const SUBCLASS_ID: usize = 1;

/// Install on this process's HWND from its owning UI thread. Returns true only
/// for a new installation; repeated calls are harmless. No pointer-owned state
/// outlives this call, and WM_NCDESTROY removes the static callback automatically.
/// The executable must activate Common Controls v6 in its manifest.
pub fn install(own_hwnd: usize) -> io::Result<bool> {
    let hwnd = HWND(own_hwnd as *mut c_void);
    let mut process = 0;
    // SAFETY: Win32 validates the scalar HWND; the out-parameter is writable for
    // this synchronous query. No window data or foreign pointer is dereferenced.
    let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
    // SAFETY: These functions only return identifiers for the calling process/thread.
    if thread == 0
        || process != unsafe { GetCurrentProcessId() }
        || thread != unsafe { GetCurrentThreadId() }
    {
        return Err(io::Error::other(
            "redraw guard requires this thread's own HWND",
        ));
    }
    // SAFETY: Ownership/thread affinity was checked above. The callback is static,
    // uses no borrowed state (refdata = 0), and removes itself at WM_NCDESTROY.
    unsafe {
        if GetWindowSubclass(hwnd, Some(subclass), SUBCLASS_ID, None).as_bool() {
            return Ok(false);
        }
        if !SetWindowSubclass(hwnd, Some(subclass), SUBCLASS_ID, 0).as_bool() {
            return Err(io::Error::other("could not install desktop redraw guard"));
        }
    }
    Ok(true)
}

unsafe extern "system" fn subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    // SAFETY: Windows calls this static procedure on the verified HWND owner
    // thread. It never dereferences message payloads, allocates or unwinds.
    unsafe {
        if message == WM_SETREDRAW {
            if wparam.0 != 0 {
                let _ = RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_FRAME);
            }
            return LRESULT(0);
        }
        if message == WM_NCDESTROY {
            let _ = RemoveWindowSubclass(hwnd, Some(subclass), id);
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }
}
