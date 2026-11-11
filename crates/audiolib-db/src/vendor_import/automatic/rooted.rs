// crates/audiolib-db/src/vendor_import/automatic/rooted.rs
//! A source capability pinned through no-follow directory handles before copying.
use std::{
    fs::File,
    io,
    path::{Component, Path, PathBuf},
};

pub(super) struct Source {
    pub(super) file: File,
    pub(super) path: PathBuf,
    _parents: Vec<File>,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub(super) fn open(root: &Path, path: &Path) -> io::Result<Source> {
    if !root.is_absolute() {
        return Err(invalid("Scan root must already be approved and absolute"));
    }
    // Database spellings omit Windows' verbatim prefix. This is a lexical
    // spelling conversion, not a filesystem re-resolution of the authority.
    let approved = PathBuf::from(crate::display_path(root));
    let requested = PathBuf::from(crate::display_path(path));
    let relative = requested
        .strip_prefix(&approved)
        .map_err(|_| invalid("Table is outside its configured scan root"))?;
    let names = relative
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| {
            if let Component::Normal(name) = c {
                Ok(name.to_owned())
            } else {
                Err(invalid("Table path must descend within the scan root"))
            }
        })
        .collect::<io::Result<Vec<_>>>()?;
    if names.is_empty() {
        return Err(invalid("Table path has no filename"));
    }
    platform::open(root, &names)
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{
        ffi::{CString, OsString},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };
    fn child(parent: &File, name: &std::ffi::OsStr, directory: bool) -> io::Result<File> {
        let name = CString::new(name.as_bytes()).map_err(|_| invalid("NUL in table path"))?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: parent is a held directory FD; name is NUL terminated. Every
        // component is opened relative to that capability with O_NOFOLLOW.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new owned FD; File closes it exactly once.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
            return Err(invalid("Table path is not a regular file/directory"));
        }
        Ok(file)
    }
    pub(super) fn open(root: &Path, names: &[OsString]) -> io::Result<Source> {
        // ScanService already approved this canonical spelling. Re-resolving it
        // after a rename could silently authorize an outside replacement root.
        let canonical = root.to_path_buf();
        let mut held = File::open("/")?;
        for component in canonical.components() {
            if let Component::Normal(name) = component {
                held = child(&held, name, true)?;
            }
        }
        for name in &names[..names.len() - 1] {
            held = child(&held, name, true)?;
        }
        let file = child(&held, &names[names.len() - 1], false)?;
        let mut path = canonical;
        for name in names {
            path.push(name);
        }
        // openat authorized every component while held. Unix does not need
        // Windows' deny-delete directory handles after the file is opened;
        // retaining depth*128 descriptors would exceed common macOS limits.
        Ok(Source {
            file,
            path,
            _parents: Vec::new(),
        })
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        ffi::{OsString, c_void},
        fs::OpenOptions,
        os::windows::{
            ffi::OsStringExt,
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawHandle,
        },
    };
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(
            file: *mut c_void,
            name: *mut u16,
            length: u32,
            flags: u32,
        ) -> u32;
    }
    fn final_path(file: &File) -> io::Result<PathBuf> {
        let mut buffer = vec![0_u16; 512];
        loop {
            // SAFETY: file owns a live handle and buffer is writable for its
            // declared UTF-16 capacity. Flags zero return this object's path.
            let count = unsafe {
                GetFinalPathNameByHandleW(
                    file.as_raw_handle(),
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    0,
                )
            };
            if count == 0 {
                return Err(io::Error::last_os_error());
            }
            if (count as usize) < buffer.len() {
                return Ok(PathBuf::from(OsString::from_wide(
                    &buffer[..count as usize],
                )));
            }
            if count > 65536 {
                return Err(invalid("Opened table path exceeds path budget"));
            }
            buffer.resize(count as usize + 1, 0);
        }
    }
    fn checked(path: &Path, directory: bool) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            // Directories deny delete/rename; source files also deny writers for
            // the short copy, so staged bytes come from this immutable handle.
            .share_mode(if directory { 0x1 | 0x2 } else { 0x1 })
            .custom_flags(0x00200000 | if directory { 0x02000000 } else { 0 })
            .open(path)?;
        let metadata = file.metadata()?;
        if metadata.file_attributes() & 0x400 != 0
            || (directory && !metadata.is_dir())
            || (!directory && !metadata.is_file())
        {
            return Err(invalid(
                "Table path is a reparse point or non-regular object",
            ));
        }
        Ok(file)
    }
    pub(super) fn open(root: &Path, names: &[OsString]) -> io::Result<Source> {
        // Open the approved spelling component by component, never follow a
        // replacement junction/symlink by re-canonicalizing the authority.
        let canonical = root.to_path_buf();
        let mut path = PathBuf::new();
        let mut parents = Vec::new();
        for component in canonical.components() {
            path.push(component.as_os_str());
            if !matches!(component, Component::Prefix(_)) {
                parents.push(checked(&path, true)?);
            }
        }
        for (index, name) in names.iter().enumerate() {
            let parent = parents
                .last()
                .ok_or_else(|| invalid("Missing held table root"))?;
            let parent_path = final_path(parent)?;
            let directory = index + 1 < names.len();
            let file = checked(&parent_path.join(name), directory)?;
            let actual = final_path(&file)?;
            if actual.parent() != Some(final_path(parent)?.as_path()) {
                return Err(invalid("Opened table escaped its held parent"));
            }
            if directory {
                parents.push(file);
            } else {
                return Ok(Source {
                    file,
                    path: actual,
                    _parents: parents,
                });
            }
        }
        Err(invalid("Missing table file"))
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;
    pub(super) fn open(_: &Path, _: &[std::ffi::OsString]) -> io::Result<Source> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Root-confined automatic tables are unsupported on this platform",
        ))
    }
}
