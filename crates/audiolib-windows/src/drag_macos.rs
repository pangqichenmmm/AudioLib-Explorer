//! macOS file drag source: AppKit `beginDraggingSession` with `NSURL` /
//! `NSPasteboardWriting` (`public.file-url`). Copy-only, real local files.
//! Blocks on a nested run loop until the session ends (parity with OLE `DoDragDrop`).

use super::DragOutcome;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSEventModifierFlags, NSEventTrackingRunLoopMode, NSEventType,
    NSImage, NSImageNameMultipleDocuments, NSPasteboardWriting, NSWindow,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSDate, NSDefaultRunLoopMode, NSObject, NSObjectProtocol, NSPoint,
    NSRect, NSRunLoop, NSSize, NSString, NSURL,
};
use std::cell::Cell;
use std::path::PathBuf;

struct DragSourceIvars {
    outcome: Cell<Option<DragOutcome>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; FileDragSource does not impl Drop.
    #[unsafe(super = NSObject)]
    #[name = "AudioLibFileDragSource"]
    #[thread_kind = MainThreadOnly]
    #[ivars = DragSourceIvars]
    struct FileDragSource;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for FileDragSource {}

    // SAFETY: NSDraggingSource methods match the protocol selectors; we only offer Copy.
    unsafe impl NSDraggingSource for FileDragSource {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn draggingSession_sourceOperationMaskForDraggingContext(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            // Match Windows: only offer copy so a drop target cannot move/delete the source file.
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn draggingSession_endedAtPoint_operation(
            &self,
            _session: &NSDraggingSession,
            _screen_point: NSPoint,
            operation: NSDragOperation,
        ) {
            let outcome = if operation.contains(NSDragOperation::Copy) {
                DragOutcome::Copied
            } else {
                DragOutcome::Cancelled
            };
            self.ivars().outcome.set(Some(outcome));
        }

        #[unsafe(method(ignoreModifierKeysForDraggingSession:))]
        fn ignoreModifierKeysForDraggingSession(&self, _session: &NSDraggingSession) -> bool {
            true
        }
    }
);

impl FileDragSource {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DragSourceIvars {
            outcome: Cell::new(None),
        });
        // SAFETY: initializer for an NSObject subclass with no extra init requirements.
        unsafe { msg_send![super(this), init] }
    }
}

pub(super) fn primary_button_down() -> bool {
    // Bit 0 = left mouse button (see NSEvent.pressedMouseButtons).
    NSEvent::pressedMouseButtons() & 1 != 0
}

fn path_to_file_url(path: &PathBuf) -> std::io::Result<Retained<NSURL>> {
    let text = path.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not valid UTF-8 for a file URL", path.display()),
        )
    })?;
    Ok(NSURL::fileURLWithPath_isDirectory(
        &NSString::from_str(text),
        false,
    ))
}

fn drag_event(app: &NSApplication, window: &NSWindow) -> std::io::Result<Retained<NSEvent>> {
    if let Some(event) = app.currentEvent() {
        return Ok(event);
    }
    // After a deferred prepare (server download) the button may still be held while
    // currentEvent is nil; synthesize a left-drag at the cursor for beginDraggingSession.
    let location = window.mouseLocationOutsideOfEventStream();
    NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        NSEventType::LeftMouseDragged,
        location,
        NSEventModifierFlags::empty(),
        0.0,
        window.windowNumber(),
        None,
        0,
        1,
        1.0,
    )
    .ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "could not synthesize a mouse-drag event for file drag-out",
        )
    })
}

pub(super) fn drag(paths: &[PathBuf]) -> std::io::Result<DragOutcome> {
    let mtm = MainThreadMarker::new().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "file drag-out must run on the AppKit main thread",
        )
    })?;
    let app = NSApplication::sharedApplication(mtm);
    let window = app
        .keyWindow()
        .or_else(|| app.mainWindow())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no key/main window available to start a file drag",
            )
        })?;
    let view = window.contentView().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "window has no content view for file drag",
        )
    })?;
    let event = drag_event(&app, &window)?;

    let mut urls = Vec::with_capacity(paths.len());
    for path in paths {
        urls.push(path_to_file_url(path)?);
    }

    let cursor = window.mouseLocationOutsideOfEventStream();
    let frame = NSRect::new(
        NSPoint::new(cursor.x - 16.0, cursor.y - 16.0),
        NSSize::new(32.0, 32.0),
    );
    // SAFETY: AppKit image-name constants are immutable process-lifetime extern statics.
    let preview = NSImage::imageNamed(unsafe { NSImageNameMultipleDocuments });

    let mut items = Vec::with_capacity(urls.len());
    for url in &urls {
        let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&**url);
        let item = NSDraggingItem::initWithPasteboardWriter(mtm.alloc::<NSDraggingItem>(), writer);
        // SAFETY: contents is an optional NSImage (or nil) for the drag badge.
        unsafe {
            item.setDraggingFrame_contents(
                frame,
                preview.as_deref().map(|image| image as &AnyObject),
            );
        }
        items.push(item);
    }
    let items = NSArray::from_retained_slice(&items);

    let source = FileDragSource::new(mtm);
    let _session = view.beginDraggingSessionWithItems_event_source(
        &items,
        &event,
        ProtocolObject::from_ref(&*source),
    );

    // beginDraggingSession returns immediately; nest the run loop until endedAt fires
    // so callers keep the same blocking contract as Windows DoDragDrop.
    let run_loop = NSRunLoop::currentRunLoop();
    loop {
        if let Some(outcome) = source.ivars().outcome.get() {
            return Ok(outcome);
        }
        let slice = NSDate::dateWithTimeIntervalSinceNow(0.05);
        // SAFETY: Foundation run-loop mode constants are immutable process-lifetime extern statics.
        run_loop.runMode_beforeDate(unsafe { NSEventTrackingRunLoopMode }, &slice);
        run_loop.runMode_beforeDate(unsafe { NSDefaultRunLoopMode }, &slice);
        // If the user released before AppKit delivered endedAt, avoid hanging forever.
        if !primary_button_down() {
            let grace = NSDate::dateWithTimeIntervalSinceNow(0.25);
            run_loop.runMode_beforeDate(unsafe { NSEventTrackingRunLoopMode }, &grace);
            if let Some(outcome) = source.ivars().outcome.get() {
                return Ok(outcome);
            }
            return Ok(DragOutcome::Cancelled);
        }
    }
}
