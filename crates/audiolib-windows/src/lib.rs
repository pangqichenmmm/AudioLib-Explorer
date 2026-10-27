// crates/audiolib-windows/src/lib.rs
//! Windows driver discovery and native diagnostics; no drivers are installed here.
use audiolib_core::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod drag;
pub use drag::{DragOutcome, drag_files, primary_button_down};

/// Explicit diagnostic capture of this process's own GUI completion marker.
/// Normal application use does not start capture; codec helpers are cross-platform.
pub mod gui_capture;

/// Opt-in own-window geometry history on the HWND's existing Windows UI thread.
#[cfg(windows)]
pub mod gui_geometry;

/// Preserve own-window visibility during native menu redraw suppression.
#[cfg(windows)]
pub mod window_redraw;

/// Display a fatal startup diagnostic for a desktop application without a console.
/// Headless callers should write their diagnostics directly and skip this dialog.
pub fn show_error_dialog(message: &str) {
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn MessageBoxW(
                window: *mut std::ffi::c_void,
                text: *const u16,
                caption: *const u16,
                flags: u32,
            ) -> i32;
        }
        let text = message
            .replace('\0', " ")
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let caption = "AudioLib Explorer\0".encode_utf16().collect::<Vec<_>>();
        // SAFETY: Both UTF-16 buffers are NUL-terminated and remain alive during the
        // synchronous call. A null owner is valid for a fatal startup diagnostic.
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), 0x10) };
    }
    #[cfg(not(windows))]
    eprintln!("AudioLib Explorer: {message}");
}

/// Put the calling thread into background processing mode (lowest CPU scheduling plus
/// very low I/O and memory priority) for the rest of its life. Intended for optional
/// background analysis such as list Waveforms, so it never competes with audition
/// decoding or the audio device. A no-op elsewhere or if Windows refuses.
pub fn enter_background_thread_mode() {
    #[cfg(windows)]
    {
        const THREAD_MODE_BACKGROUND_BEGIN: i32 = 0x0001_0000;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn SetThreadPriority(thread: *mut std::ffi::c_void, priority: i32) -> i32;
        }
        // SAFETY: GetCurrentThread returns a pseudo handle for the calling thread that
        // needs no closing; SetThreadPriority only changes this thread's own scheduling
        // and accepts THREAD_MODE_BACKGROUND_BEGIN for the current thread. Failure is
        // harmless and ignored.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AsioDriver {
    pub name: String,
    pub aliases: Vec<String>,
    pub clsid: String,
    pub dll_path: Option<PathBuf>,
    pub available: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AsioBufferRange {
    pub min: u32,
    pub max: u32,
    pub preferred: u32,
    pub granularity: i32,
}
impl AsioBufferRange {
    /// SDK granularity -1 means powers of two, 0 means continuous, >0 means steps from min.
    pub fn supports(&self, frames: u32) -> bool {
        frames >= self.min
            && frames <= self.max
            && match self.granularity {
                -1 => frames.is_power_of_two(),
                0 => true,
                step if step > 0 => (frames - self.min).is_multiple_of(step as u32),
                _ => false,
            }
    }
}

#[cfg(all(windows, feature = "asio"))]
unsafe extern "C" {
    fn audiolib_asio_buffer_range(
        min: *mut i32,
        max: *mut i32,
        preferred: *mut i32,
        granularity: *mut i32,
    ) -> i32;
    fn audiolib_asio_channel_name(channel: i32, name: *mut std::ffi::c_char) -> i32;
    fn audiolib_asio_control_panel() -> i32;
}

/// Query an already initialized CPAL ASIO driver from its serialized control thread.
/// The caller must retain the CPAL device/stream; this helper never loads a second driver.
/// # Safety
/// Hold an initialized ASIO driver through a live CPAL iterator/stream and serialize
/// driver lifecycle/control operations against all other calls into this global SDK.
pub unsafe fn loaded_asio_buffer_range() -> Result<AsioBufferRange> {
    #[cfg(all(windows, feature = "asio"))]
    {
        let (mut min, mut max, mut preferred, mut granularity) = (0, 0, 0, 0);
        // SAFETY: Four initialized i32 slots match Windows SDK long; they remain valid for
        // the synchronous call. The caller guarantees a retained driver and serialization.
        let error = unsafe {
            audiolib_asio_buffer_range(&mut min, &mut max, &mut preferred, &mut granularity)
        };
        if error != 0 || min <= 0 || max < min || preferred <= 0 {
            return Err(audiolib_core::Error::Audio(format!(
                "ASIO buffer query failed ({error})"
            )));
        }
        Ok(AsioBufferRange {
            min: min as u32,
            max: max as u32,
            preferred: preferred as u32,
            granularity,
        })
    }
    #[cfg(not(all(windows, feature = "asio")))]
    {
        Err(audiolib_core::Error::Audio(
            "ASIO is not enabled in this build".into(),
        ))
    }
}
/// Get a loaded driver's output channel labels, without creating any audio buffers.
/// # Safety
/// Retain the initialized driver and serialize SDK lifecycle/control calls. `channels`
/// must be the output count reported by that same driver.
pub unsafe fn loaded_asio_channel_names(channels: u16) -> Result<Vec<String>> {
    #[cfg(all(windows, feature = "asio"))]
    {
        (0..channels)
            .map(|channel| {
                let mut name = [0_i8; 32];
                // SAFETY: The initialized 32-char buffer is writable for the synchronous
                // shim call. The caller retains the driver and serializes SDK access.
                let error =
                    unsafe { audiolib_asio_channel_name(i32::from(channel), name.as_mut_ptr()) };
                if error != 0 {
                    return Err(audiolib_core::Error::Audio(format!(
                        "ASIO channel query failed ({error})"
                    )));
                }
                // SAFETY: The shim always sets byte 31 to NUL on success, so CStr cannot
                // read beyond this initialized 32-byte array; it outlives the conversion.
                Ok(unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
                    .to_string_lossy()
                    .into_owned())
            })
            .collect()
    }
    #[cfg(not(all(windows, feature = "asio")))]
    {
        let _ = channels;
        Err(audiolib_core::Error::Audio(
            "ASIO is not enabled in this build".into(),
        ))
    }
}
/// Ask the currently loaded ASIO driver to open its own control panel.
/// # Safety
/// Retain the initialized driver and serialize SDK lifecycle/control operations; never
/// call from a real-time callback. The driver owns its window and any reset notification.
pub unsafe fn loaded_asio_control_panel() -> Result<()> {
    #[cfg(all(windows, feature = "asio"))]
    {
        // SAFETY: There are no Rust pointers in this synchronous FFI call. The caller
        // guarantees a retained global driver and serialized control-thread execution.
        let error = unsafe { audiolib_asio_control_panel() };
        if error != 0 {
            return Err(audiolib_core::Error::Audio(format!(
                "ASIO control panel failed ({error})"
            )));
        }
        Ok(())
    }
    #[cfg(not(all(windows, feature = "asio")))]
    {
        Err(audiolib_core::Error::Audio(
            "ASIO is not enabled in this build".into(),
        ))
    }
}

/// Enumerate x64 ASIO registrations, deduplicating aliases by COM CLSID.
pub fn asio_drivers() -> Result<Vec<AsioDriver>> {
    #[cfg(windows)]
    {
        use std::collections::BTreeMap;
        use winreg::{
            RegKey,
            enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY},
        };
        let machine = RegKey::predef(HKEY_LOCAL_MACHINE);
        let root =
            match machine.open_subkey_with_flags("SOFTWARE\\ASIO", KEY_READ | KEY_WOW64_64KEY) {
                Ok(key) => key,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(error.into()),
            };
        let mut drivers = BTreeMap::<String, AsioDriver>::new();
        for name in root.enum_keys().filter_map(std::result::Result::ok) {
            let key = root.open_subkey_with_flags(&name, KEY_READ | KEY_WOW64_64KEY)?;
            let clsid: String = match key.get_value("CLSID") {
                Ok(value) => value,
                Err(_) => continue,
            };
            let identity = clsid.to_ascii_uppercase();
            if let Some(driver) = drivers.get_mut(&identity) {
                driver.aliases.push(name);
                continue;
            }
            let server = format!("SOFTWARE\\Classes\\CLSID\\{}\\InprocServer32", clsid);
            let dll_path = machine
                .open_subkey_with_flags(server, KEY_READ | KEY_WOW64_64KEY)
                .ok()
                .and_then(|key| key.get_value::<String, _>("").ok())
                .map(PathBuf::from);
            let available = dll_path.as_ref().is_some_and(|path| path.is_file());
            drivers.insert(
                identity,
                AsioDriver {
                    name,
                    aliases: Vec::new(),
                    clsid,
                    dll_path,
                    available,
                },
            );
        }
        Ok(drivers.into_values().collect())
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}

/// Encrypt a small secret for the current Windows user with DPAPI.
/// Other platforms return the bytes unchanged; callers must still restrict file access.
pub fn protect_user_secret(plain: &[u8]) -> Result<Vec<u8>> {
    #[cfg(windows)]
    {
        dpapi::transform(plain, true)
    }
    #[cfg(not(windows))]
    {
        Ok(plain.to_vec())
    }
}

/// Decrypt bytes produced by [`protect_user_secret`] for the same Windows user.
pub fn unprotect_user_secret(protected: &[u8]) -> Result<Vec<u8>> {
    #[cfg(windows)]
    {
        dpapi::transform(protected, false)
    }
    #[cfg(not(windows))]
    {
        Ok(protected.to_vec())
    }
}

/// True when [`protect_user_secret`] really encrypts (Windows DPAPI).
pub const fn user_secret_protection_available() -> bool {
    cfg!(windows)
}

#[cfg(windows)]
mod dpapi {
    use audiolib_core::{Error, Result};
    use std::ffi::c_void;

    #[repr(C)]
    struct DataBlob {
        size: u32,
        data: *mut u8,
    }
    #[link(name = "crypt32")]
    unsafe extern "system" {
        fn CryptProtectData(
            input: *const DataBlob,
            description: *const u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            output: *mut DataBlob,
        ) -> i32;
        fn CryptUnprotectData(
            input: *const DataBlob,
            description: *mut *mut u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            output: *mut DataBlob,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    /// Run one DPAPI call. CRYPTPROTECT_UI_FORBIDDEN keeps it non-interactive.
    pub(super) fn transform(input: &[u8], protect: bool) -> Result<Vec<u8>> {
        let size = u32::try_from(input.len())
            .map_err(|_| Error::Validation("Secret is too large".into()))?;
        let blob = DataBlob {
            size,
            data: input.as_ptr().cast_mut(),
        };
        let mut output = DataBlob {
            size: 0,
            data: std::ptr::null_mut(),
        };
        // SAFETY: DataBlob matches the Windows SDK DATA_BLOB layout. The input buffer
        // outlives the synchronous call and DPAPI never writes through pDataIn. Null
        // description/entropy/prompt pointers are documented as optional. On success the
        // output allocation is owned by us and released exactly once with LocalFree.
        unsafe {
            let ok = if protect {
                CryptProtectData(
                    &blob,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0x1,
                    &mut output,
                )
            } else {
                CryptUnprotectData(
                    &blob,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0x1,
                    &mut output,
                )
            };
            if ok == 0 {
                return Err(Error::Io(std::io::Error::last_os_error()));
            }
            let result = if output.data.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(output.data, output.size as usize).to_vec()
            };
            if !output.data.is_null() {
                LocalFree(output.data.cast());
            }
            Ok(result)
        }
    }
}

/// Open an existing path in Windows Explorer without interpolating a shell command.
pub fn reveal_path(path: &std::path::Path) -> Result<()> {
    if !path.exists() {
        return Err(audiolib_core::Error::NotFound(path.display().to_string()));
    }
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg("/select,")
            .arg(path)
            .spawn()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn background_mode_applies_only_to_its_own_thread() {
        std::thread::spawn(enter_background_thread_mode)
            .join()
            .unwrap();
    }
    #[test]
    fn user_secret_round_trips_and_is_not_plaintext_on_windows() {
        let secret = b"token-\xe4\xb8\xad-123";
        let protected = protect_user_secret(secret).unwrap();
        if user_secret_protection_available() {
            assert_ne!(protected.as_slice(), secret.as_slice());
        }
        assert_eq!(unprotect_user_secret(&protected).unwrap(), secret);
        assert!(unprotect_user_secret(b"not a dpapi blob").is_err() || !cfg!(windows));
    }

    #[test]
    fn sdk_buffer_granularity() {
        let powers = AsioBufferRange {
            min: 32,
            max: 1024,
            preferred: 256,
            granularity: -1,
        };
        assert!(powers.supports(64));
        assert!(!powers.supports(48));
        let stepped = AsioBufferRange {
            min: 48,
            max: 1024,
            preferred: 240,
            granularity: 48,
        };
        assert!(stepped.supports(240));
        assert!(!stepped.supports(256));
        let continuous = AsioBufferRange {
            granularity: 0,
            ..powers
        };
        assert!(continuous.supports(257));
        assert!(!continuous.supports(2048));
    }
}
