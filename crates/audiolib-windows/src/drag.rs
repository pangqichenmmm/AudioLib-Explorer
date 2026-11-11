// crates/audiolib-windows/src/drag.rs
//! Native file drag source for dragging real, complete local files out of the desktop app.
//! Windows uses OLE `DoDragDrop` with `CF_HDROP`; macOS uses AppKit `beginDraggingSession`
//! with `NSURL` / `NSPasteboardWriting` (`public.file-url`). Only the copy effect is offered,
//! so a drop target can never move (and thereby remove) the dragged file. No virtual or
//! delayed-rendered files.
use std::path::PathBuf;

/// How a drag ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragOutcome {
    /// Dropped on a target that accepted a copy.
    Copied,
    /// Released over no target, a target that refused, or cancelled with Escape.
    Cancelled,
}

#[cfg(windows)]
mod native {
    use super::DragOutcome;
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use windows::Win32::Foundation::{
        DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC,
        DV_E_TYMED, E_NOTIMPL, E_OUTOFMEMORY, OLE_E_ADVISENOTSUPPORTED, POINT, S_OK,
    };
    use windows::Win32::System::Com::{
        DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl,
        IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
    };
    use windows::Win32::System::Memory::{
        GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock,
    };
    use windows::Win32::System::Ole::{
        CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource,
        IDropSource_Impl, OleInitialize, OleUninitialize,
    };
    use windows::Win32::System::SystemServices::{MK_LBUTTON, MK_RBUTTON, MODIFIERKEYS_FLAGS};
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
    use windows::Win32::UI::Shell::{DROPFILES, SHCreateStdEnumFmtEtc};
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SWAPBUTTON};
    use windows::core::{BOOL, HRESULT, Ref, implement};

    fn hdrop_format() -> FORMATETC {
        FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        }
    }

    /// `DROPFILES` header followed by the NUL-separated, double-NUL-terminated UTF-16 list.
    pub(super) fn dropfiles_bytes(paths: &[PathBuf]) -> Vec<u8> {
        let header = std::mem::size_of::<DROPFILES>();
        let mut names: Vec<u16> = Vec::new();
        for path in paths {
            names.extend(path.as_os_str().encode_wide().filter(|&unit| unit != 0));
            names.push(0);
        }
        names.push(0);
        let dropfiles = DROPFILES {
            pFiles: header as u32,
            pt: POINT { x: 0, y: 0 },
            fNC: BOOL(0),
            fWide: BOOL(1),
        };
        let mut bytes = Vec::with_capacity(header + names.len() * 2);
        // SAFETY: DROPFILES is a plain repr(C) struct; its bytes are copied as-is.
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts((&raw const dropfiles).cast::<u8>(), header)
        });
        for unit in names {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    #[implement(IDataObject)]
    pub(super) struct FileDrop {
        bytes: Vec<u8>,
    }

    impl FileDrop {
        pub(super) fn new(paths: &[PathBuf]) -> Self {
            Self {
                bytes: dropfiles_bytes(paths),
            }
        }
    }

    fn matches_hdrop(format: *const FORMATETC) -> HRESULT {
        // SAFETY: OLE passes a valid FORMATETC pointer (null is rejected).
        let Some(format) = (unsafe { format.as_ref() }) else {
            return DV_E_FORMATETC;
        };
        if format.cfFormat != CF_HDROP.0 || format.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_FORMATETC;
        }
        if format.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
            return DV_E_TYMED;
        }
        S_OK
    }

    #[allow(non_snake_case)]
    impl IDataObject_Impl for FileDrop_Impl {
        fn GetData(&self, format: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
            matches_hdrop(format).ok()?;
            // SAFETY: a fresh movable global block of the exact size is filled while locked;
            // ownership passes to the caller through the STGMEDIUM (no pUnkForRelease).
            unsafe {
                let global = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, self.bytes.len())?;
                let target = GlobalLock(global);
                if target.is_null() {
                    return Err(E_OUTOFMEMORY.into());
                }
                std::ptr::copy_nonoverlapping(
                    self.bytes.as_ptr(),
                    target.cast::<u8>(),
                    self.bytes.len(),
                );
                let _ = GlobalUnlock(global);
                Ok(STGMEDIUM {
                    tymed: TYMED_HGLOBAL.0 as u32,
                    u: STGMEDIUM_0 { hGlobal: global },
                    pUnkForRelease: std::mem::ManuallyDrop::new(None),
                })
            }
        }
        fn GetDataHere(
            &self,
            _format: *const FORMATETC,
            _medium: *mut STGMEDIUM,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }
        fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
            matches_hdrop(format)
        }
        fn GetCanonicalFormatEtc(&self, _format: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
            // SAFETY: OLE passes a writable FORMATETC; ptd must be NULL on return.
            if let Some(out) = unsafe { out.as_mut() } {
                out.ptd = std::ptr::null_mut();
            }
            E_NOTIMPL
        }
        fn SetData(
            &self,
            _format: *const FORMATETC,
            _medium: *const STGMEDIUM,
            _release: BOOL,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }
        fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
            if direction != DATADIR_GET.0 as u32 {
                return Err(E_NOTIMPL.into());
            }
            // SAFETY: the shell copies the one-element array before returning.
            unsafe { SHCreateStdEnumFmtEtc(&[hdrop_format()]) }
        }
        fn DAdvise(
            &self,
            _format: *const FORMATETC,
            _flags: u32,
            _sink: Ref<'_, IAdviseSink>,
        ) -> windows::core::Result<u32> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }
        fn DUnadvise(&self, _connection: u32) -> windows::core::Result<()> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }
        fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }
    }

    #[implement(IDropSource)]
    struct DropSource;

    #[allow(non_snake_case)]
    impl IDropSource_Impl for DropSource_Impl {
        fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
            if escape.as_bool() {
                DRAGDROP_S_CANCEL
            } else if keys.0 & (MK_LBUTTON.0 | MK_RBUTTON.0) == 0 {
                DRAGDROP_S_DROP
            } else {
                S_OK
            }
        }
        fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    pub(super) fn primary_button_down() -> bool {
        // SAFETY: plain Win32 queries without pointers.
        unsafe {
            let key = if GetSystemMetrics(SM_SWAPBUTTON) != 0 {
                VK_RBUTTON
            } else {
                VK_LBUTTON
            };
            (GetAsyncKeyState(i32::from(key.0)) as u16 & 0x8000) != 0
        }
    }

    pub(super) fn drag(paths: &[PathBuf]) -> std::io::Result<DragOutcome> {
        // SAFETY: OLE is initialized (or already was) on this thread for the modal drag
        // loop and uninitialized again only when this call initialized it.
        unsafe {
            OleInitialize(None).map_err(std::io::Error::other)?;
            let data: IDataObject = FileDrop::new(paths).into();
            let source: IDropSource = DropSource.into();
            let mut effect = DROPEFFECT_NONE;
            let result = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
            OleUninitialize();
            if result == DRAGDROP_S_DROP && effect.0 & DROPEFFECT_COPY.0 != 0 {
                Ok(DragOutcome::Copied)
            } else if result == DRAGDROP_S_DROP || result == DRAGDROP_S_CANCEL {
                Ok(DragOutcome::Cancelled)
            } else {
                Err(std::io::Error::other(windows::core::Error::from(result)))
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[path = "drag_macos.rs"]
mod macos;

/// True while the primary mouse button is physically held (respects swapped buttons).
/// A drag that had to wait for downloads only starts if the user is still dragging.
pub fn primary_button_down() -> bool {
    #[cfg(windows)]
    {
        native::primary_button_down()
    }
    #[cfg(target_os = "macos")]
    {
        macos::primary_button_down()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

/// Run a modal native drag of `paths` (existing local files) with the copy effect only.
/// Call on the UI thread while the mouse button is held.
pub fn drag_files(paths: &[PathBuf]) -> std::io::Result<DragOutcome> {
    if paths.is_empty() {
        return Ok(DragOutcome::Cancelled);
    }
    if let Some(missing) = paths.iter().find(|path| !path.is_file()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} is not a local file", missing.display()),
        ));
    }
    #[cfg(windows)]
    {
        native::drag(paths)
    }
    #[cfg(target_os = "macos")]
    {
        macos::drag(paths)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "file drag and drop is only available on Windows and macOS",
        ))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::native::FileDrop;
    use std::path::PathBuf;
    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::{CF_HDROP, ReleaseStgMedium};
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    #[test]
    fn data_object_offers_real_paths_as_cf_hdrop() {
        let paths = vec![
            PathBuf::from(r"C:\cache\lib\7-0123456789abcdef\雷声 近.wav"),
            PathBuf::from(r"D:\SFX\door.wav"),
        ];
        let data: IDataObject = FileDrop::new(&paths).into();
        let format = FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        // SAFETY: the medium returned by GetData is read while locked and then released.
        unsafe {
            assert!(data.QueryGetData(&format).is_ok());
            let mut medium = data.GetData(&format).unwrap();
            let global = medium.u.hGlobal;
            assert!(GlobalSize(global) > 0);
            let locked = GlobalLock(global);
            let drop = HDROP(locked);
            assert_eq!(DragQueryFileW(drop, u32::MAX, None), 2);
            for (index, expected) in paths.iter().enumerate() {
                let mut buffer = [0u16; 260];
                let length = DragQueryFileW(drop, index as u32, Some(&mut buffer)) as usize;
                assert_eq!(
                    String::from_utf16(&buffer[..length]).unwrap(),
                    expected.display().to_string()
                );
            }
            let _ = GlobalUnlock(global);
            ReleaseStgMedium(&mut medium);
            let mut other = format;
            other.cfFormat = 1;
            assert!(data.QueryGetData(&other).is_err());
            assert!(data.GetData(&other).is_err());
        }
    }
}

#[cfg(test)]
mod validation_tests {
    use std::path::PathBuf;

    #[test]
    fn missing_files_are_never_offered() {
        let missing = if cfg!(windows) {
            PathBuf::from(r"Z:\definitely\missing.wav")
        } else {
            PathBuf::from("/definitely/missing-audiolib-drag.wav")
        };
        let error = super::drag_files(&[missing]).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            super::drag_files(&[]).unwrap(),
            super::DragOutcome::Cancelled
        );
    }
}
