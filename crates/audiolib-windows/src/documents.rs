// crates/audiolib-windows/src/documents.rs
//! Resolve the user's actual Documents directory, including Windows redirection.
use audiolib_core::{Error, Result};
use std::path::PathBuf;

/// Platform Documents directory used by durable user exports.
/// Failure is explicit so the desktop can ask the user to choose a destination.
pub fn documents_directory() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        use windows::Win32::{
            System::Com::CoTaskMemFree,
            UI::Shell::{FOLDERID_Documents, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
        };
        // SAFETY: A known-folder GUID and no token ask for the current user's
        // folder. On success Windows returns a NUL-terminated CoTaskMem allocation.
        let folder = unsafe { SHGetKnownFolderPath(&FOLDERID_Documents, KF_FLAG_DEFAULT, None) }
            .map_err(|error| Error::Validation(format!("Resolve Documents: {error}")))?;
        // SAFETY: The successful call owns this valid NUL-terminated UTF-16 buffer;
        // copy its contents before freeing the allocation exactly once.
        let path = unsafe { PathBuf::from(OsString::from_wide(folder.as_wide())) };
        // SAFETY: SHGetKnownFolderPath allocated this pointer with CoTaskMem.
        unsafe { CoTaskMemFree(Some(folder.0.cast())) };
        Ok(path)
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/bin/osascript")
            .args(["-e", "POSIX path of (path to documents folder)"])
            .output()?;
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
        Err(Error::Validation(
            "Resolve Documents failed; choose an export directory".into(),
        ))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let output = std::process::Command::new("xdg-user-dir")
            .arg("DOCUMENTS")
            .output()?;
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if output.status.success() && !path.is_empty() {
            Ok(PathBuf::from(path))
        } else {
            Err(Error::Validation(
                "Resolve Documents failed; choose an export directory".into(),
            ))
        }
    }
}
