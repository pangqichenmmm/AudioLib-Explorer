// crates/audiolib-windows/src/macos_lifecycle.rs
//! Route native application termination through the desktop's asynchronous close drain.
//!
//! Slint's native App menu invokes `-[NSApplication terminate:]`, which does not
//! ask the window delegate to close. Keep Winit's delegate and add only its missing
//! termination query; an unknown delegate or existing foreign implementation is
//! an initialization error rather than an unsafe attempt to replace it.

use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{ffi, sel};
use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};
use objc2_foundation::MainThreadMarker;
use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicPtr, Ordering};

thread_local! {
    static CLOSE_HANDLER: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
}

static INSTALLED_CLASS: AtomicPtr<AnyClass> = AtomicPtr::new(std::ptr::null_mut());

/// Hold the main-thread native Quit handler until the desktop has safely shut down.
/// Native termination is always cancelled; the callback must request the ordinary
/// close path, which drains writes and then stops the Slint event loop itself.
/// This guard cannot move to another thread because AppKit callbacks use the main thread.
#[must_use]
pub struct NativeQuitGuard {
    _main_thread: MainThreadMarker,
}

impl Drop for NativeQuitGuard {
    fn drop(&mut self) {
        CLOSE_HANDLER.with(|slot| *slot.borrow_mut() = None);
        // Objective-C has no method-removal operation. The installed query remains
        // fail-closed after this guard drops, until the Rust process exits normally.
    }
}

/// Intercept App-menu Quit, Cmd+Q and Dock Quit without replacing Winit's delegate.
/// Call after constructing the Slint window and before entering any event loop.
/// The callback runs on the main thread and must retain the window during its drain.
pub fn install_native_quit_handler(
    on_close: impl Fn() + 'static,
) -> Result<NativeQuitGuard, String> {
    let main_thread = MainThreadMarker::new()
        .ok_or_else(|| "Native Quit handling must be installed on the main thread".to_owned())?;
    if CLOSE_HANDLER.with(|slot| slot.borrow().is_some()) {
        return Err("Native Quit handling is already active".into());
    }
    let application = NSApplication::sharedApplication(main_thread);
    let delegate = application
        .delegate()
        .ok_or_else(|| "The desktop application delegate is unavailable".to_owned())?;
    let delegate_object: &AnyObject = delegate.as_ref();
    let class = delegate_object.class();
    if class.name().to_bytes() != b"WinitApplicationDelegate" {
        return Err("The desktop application delegate is not the pinned Winit delegate".into());
    }
    let class_pointer = std::ptr::from_ref(class).cast_mut();
    let selector = sel!(applicationShouldTerminate:);
    if let Some(method) = class.instance_method(selector) {
        if INSTALLED_CLASS.load(Ordering::Acquire) != class_pointer
            || !std::ptr::fn_addr_eq(method.implementation(), termination_implementation())
        {
            return Err("The desktop already has an unknown native termination handler".into());
        }
    } else {
        // SAFETY: This is the verified live Winit delegate class on the main
        // thread. Its class and this static callback live until process exit.
        // The absent selector takes self, _cmd and NSApplication*, returning
        // NSUInteger (Q on supported 64-bit macOS), exactly the callback ABI.
        // class_addMethod never overwrites an existing implementation.
        let added = unsafe {
            ffi::class_addMethod(
                class_pointer,
                selector,
                termination_implementation(),
                c"Q@:@".as_ptr(),
            )
        };
        if !added.as_bool() {
            return Err("Could not install the native desktop termination handler".into());
        }
        INSTALLED_CLASS.store(class_pointer, Ordering::Release);
    }
    CLOSE_HANDLER.with(|slot| *slot.borrow_mut() = Some(Rc::new(on_close)));
    Ok(NativeQuitGuard {
        _main_thread: main_thread,
    })
}

type TerminationImplementation = unsafe extern "C-unwind" fn(
    *mut AnyObject,
    Sel,
    *mut NSApplication,
) -> NSApplicationTerminateReply;

fn termination_implementation() -> Imp {
    // SAFETY: Objective-C stores an untyped IMP. Installation supplies the exact
    // method encoding and AppKit invokes this function with the documented ABI.
    unsafe { std::mem::transmute::<TerminationImplementation, Imp>(application_should_terminate) }
}

unsafe extern "C-unwind" fn application_should_terminate(
    _delegate: *mut AnyObject,
    _selector: Sel,
    _application: *mut NSApplication,
) -> NSApplicationTerminateReply {
    // No Rust panic may unwind through AppKit. Clone out of the TLS borrow before
    // calling application code so a nested close cannot retain a RefCell borrow.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if MainThreadMarker::new().is_some() {
            let handler = CLOSE_HANDLER.with(|slot| slot.borrow().clone());
            if let Some(handler) = handler {
                handler();
            }
        }
    }));
    NSApplicationTerminateReply::TerminateCancel
}

/// Send the real AppKit termination action to this application for an owned smoke probe.
/// Refuse the action unless this guard's verified interception is active, so a
/// missing hook cannot turn a test into an immediate application termination.
pub fn request_native_termination() -> Result<(), String> {
    let main_thread = MainThreadMarker::new()
        .ok_or_else(|| "Native Quit must be requested on the main thread".to_owned())?;
    if !CLOSE_HANDLER.with(|slot| slot.borrow().is_some()) {
        return Err("The guarded native Quit handler is not active".into());
    }
    let application = NSApplication::sharedApplication(main_thread);
    let delegate = application
        .delegate()
        .ok_or_else(|| "The desktop application delegate is unavailable".to_owned())?;
    let delegate_object: &AnyObject = delegate.as_ref();
    let class = delegate_object.class();
    let ours = class
        .instance_method(sel!(applicationShouldTerminate:))
        .is_some_and(|method| {
            INSTALLED_CLASS.load(Ordering::Acquire) == std::ptr::from_ref(class).cast_mut()
                && std::ptr::fn_addr_eq(method.implementation(), termination_implementation())
        });
    if !ours {
        return Err("The guarded native Quit handler has changed".into());
    }
    application.terminate(None);
    Ok(())
}
