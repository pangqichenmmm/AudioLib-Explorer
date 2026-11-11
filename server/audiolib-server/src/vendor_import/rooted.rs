// server/audiolib-server/src/vendor_import/rooted.rs
//! Root-confined table opens. Authorization belongs to opened objects, never a later path reopen.
use std::{
    ffi::OsStr,
    fs::File,
    io,
    path::{Component, Path},
};

#[cfg(windows)]
use std::path::PathBuf;

pub(super) enum Opened {
    Directory(Directory),
    File(File),
}

pub(super) struct Directory {
    file: File,
    #[cfg(windows)]
    parents: Vec<File>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn single_name(name: &OsStr) -> io::Result<()> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(invalid("Expected one table-directory entry name"));
    }
    Ok(())
}

impl Directory {
    /// Descend only through opened directories. Symlink components are never followed.
    pub(super) fn descend(&self, relative: &Path) -> io::Result<Self> {
        let mut directory = self.try_clone()?;
        for component in relative.components() {
            let name = match component {
                Component::CurDir => continue,
                Component::Normal(name) => name,
                _ => return Err(invalid("Table directory must be relative to its root")),
            };
            directory = match directory.open_entry(name, false)? {
                Some(Opened::Directory(child)) => child,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Table directory changed or contains a link",
                    ));
                }
            };
        }
        Ok(directory)
    }

    /// Return a root-authorized open regular file, never a source path for later reopening.
    pub(super) fn open_file(&self, relative: &Path) -> io::Result<Option<File>> {
        let parent = self.descend(relative.parent().unwrap_or_else(|| Path::new("")))?;
        let name = relative
            .file_name()
            .ok_or_else(|| invalid("Table file name is required"))?;
        Ok(match parent.open_entry(name, true)? {
            Some(Opened::File(file)) => Some(file),
            _ => None,
        })
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{
        ffi::{CStr, CString, OsString},
        os::{
            fd::{AsRawFd, FromRawFd, IntoRawFd},
            unix::ffi::{OsStrExt, OsStringExt},
        },
    };

    fn c_name(name: &OsStr) -> io::Result<CString> {
        single_name(name)?;
        CString::new(name.as_bytes()).map_err(|_| invalid("NUL in table entry"))
    }

    fn opened(parent: &File, name: &CStr, directory: bool) -> io::Result<File> {
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: parent owns a live directory FD; name is NUL-terminated. O_NOFOLLOW
        // rejects a replaced final component; each ancestor was opened the same way.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat returned a new owned FD; File closes it exactly once.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    impl Directory {
        /// Capture a canonical configured root through a no-follow directory chain.
        pub(in crate::vendor_import::server_path) fn open_root(path: &Path) -> io::Result<Self> {
            let canonical = path.canonicalize()?;
            let mut file = File::open("/")?;
            for component in canonical.components() {
                if let Component::Normal(name) = component {
                    file = opened(&file, &c_name(name)?, true)?;
                }
            }
            Ok(Self { file })
        }
        pub(super) fn try_clone(&self) -> io::Result<Self> {
            Ok(Self {
                file: self.file.try_clone()?,
            })
        }
        /// Open one actual child; skip links, special files and unrequested regular files.
        pub(in crate::vendor_import::server_path) fn open_entry(
            &self,
            name: &OsStr,
            include_file: bool,
        ) -> io::Result<Option<Opened>> {
            self.open_entry_checked(name, include_file, || {})
        }
        fn open_entry_checked(
            &self,
            name: &OsStr,
            include_file: bool,
            before_open: impl FnOnce(),
        ) -> io::Result<Option<Opened>> {
            let name = c_name(name)?;
            let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: status is writable; AT_SYMLINK_NOFOLLOW classifies the entry
            // relative to the held directory, without opening a device/FIFO/link.
            if unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    status.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                let error = io::Error::last_os_error();
                return if error.kind() == io::ErrorKind::NotFound {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            // SAFETY: successful fstatat initialized status.
            let mode = unsafe { status.assume_init() }.st_mode & libc::S_IFMT;
            if mode != libc::S_IFDIR && (mode != libc::S_IFREG || !include_file) {
                return Ok(None);
            }
            before_open();
            let file = match opened(&self.file, &name, mode == libc::S_IFDIR) {
                Ok(file) => file,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        || matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            let actual = file.metadata()?;
            Ok(if actual.is_dir() {
                Some(Opened::Directory(Self { file }))
            } else if actual.is_file() {
                Some(Opened::File(file))
            } else {
                None
            })
        }

        /// List the held directory with a hard count bound and an independent stream offset.
        pub(in crate::vendor_import::server_path) fn names(
            &self,
            limit: usize,
        ) -> io::Result<Vec<OsString>> {
            // A fresh open description gives this listing its own directory offset;
            // dup/try_clone would share an offset with other listings of this capability.
            let fd = opened(&self.file, c".", true)?.into_raw_fd();
            // SAFETY: fd is a new owned directory FD. fdopendir takes ownership on
            // success; on failure we explicitly close the still-owned descriptor.
            let raw = unsafe { libc::fdopendir(fd) };
            if raw.is_null() {
                let error = io::Error::last_os_error();
                // SAFETY: fdopendir failed and did not consume fd.
                unsafe { libc::close(fd) };
                return Err(error);
            }
            struct Stream(*mut libc::DIR);
            impl Drop for Stream {
                fn drop(&mut self) {
                    // SAFETY: this guard exclusively owns the fdopendir result.
                    unsafe { libc::closedir(self.0) };
                }
            }
            let stream = Stream(raw);
            let mut names = Vec::new();
            loop {
                // SAFETY: these platforms expose this thread's errno slot. Reset it
                // before readdir so NULL can distinguish EOF from an enumeration error.
                #[cfg(target_os = "linux")]
                unsafe {
                    *libc::__errno_location() = 0;
                }
                #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
                unsafe {
                    *libc::__error() = 0;
                }
                // SAFETY: stream remains live; the returned entry is copied before
                // the next call, so no borrowed dirent storage escapes.
                let entry = unsafe { libc::readdir(stream.0) };
                if entry.is_null() {
                    #[cfg(any(
                        target_os = "linux",
                        target_os = "macos",
                        target_os = "ios",
                        target_os = "freebsd"
                    ))]
                    if io::Error::last_os_error().raw_os_error() != Some(0) {
                        return Err(io::Error::last_os_error());
                    }
                    break;
                }
                // SAFETY: POSIX readdir supplies a NUL-terminated d_name in a valid entry.
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                if names.len() == limit {
                    return Err(invalid("Table discovery directory-entry limit exceeded"));
                }
                names.push(OsString::from_vec(name.to_vec()));
            }
            Ok(names)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn replacing_an_entry_with_an_outside_link_between_stat_and_open_is_refused() {
            let owned = tempfile::tempdir().unwrap();
            let root = owned.path().join("root");
            std::fs::create_dir(&root).unwrap();
            let table = root.join("table.csv");
            std::fs::write(&table, b"allowed").unwrap();
            let outside = owned.path().join("outside.csv");
            std::fs::write(&outside, b"secret").unwrap();
            let dir = Directory::open_root(&root).unwrap();
            let opened = dir
                .open_entry_checked(OsStr::new("table.csv"), true, || {
                    std::fs::remove_file(&table).unwrap();
                    std::os::unix::fs::symlink(&outside, &table).unwrap();
                })
                .unwrap();
            assert!(opened.is_none());
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        ffi::{OsString, c_void},
        fs::{self, OpenOptions},
        os::windows::{
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawHandle,
        },
    };
    const REPARSE: u32 = 0x400;
    const OPEN_REPARSE: u32 = 0x00200000;
    const BACKUP_SEMANTICS: u32 = 0x02000000;
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
        use std::os::windows::ffi::OsStringExt;
        let mut buffer = vec![0_u16; 512];
        loop {
            // SAFETY: File owns a live handle; buffer is writable for length UTF-16
            // code units. Flags 0 request the normalized DOS/UNC path of that object.
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
                return Err(invalid("Opened table path exceeds Windows path budget"));
            }
            buffer.resize(count as usize + 1, 0);
        }
    }
    fn open(path: &Path, directory: bool) -> io::Result<File> {
        // Omitting FILE_SHARE_DELETE fixes each opened directory component and file
        // against rename/replacement until its validated bytes have been staged.
        OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2)
            .custom_flags(OPEN_REPARSE | if directory { BACKUP_SEMANTICS } else { 0 })
            .open(path)
    }
    impl Directory {
        /// Hold the canonical root's directory chain without delete sharing or reparses.
        pub(in crate::vendor_import::server_path) fn open_root(path: &Path) -> io::Result<Self> {
            let canonical = path.canonicalize()?;
            let mut current = PathBuf::new();
            let mut parents = Vec::new();
            for component in canonical.components() {
                current.push(component.as_os_str());
                if matches!(component, Component::Prefix(_)) {
                    continue;
                }
                let file = open(&current, true)?;
                let metadata = file.metadata()?;
                if !metadata.is_dir() || metadata.file_attributes() & REPARSE != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Configured table root changed into a reparse point",
                    ));
                }
                parents.push(file);
            }
            let file = parents
                .pop()
                .ok_or_else(|| invalid("Configured table root must be absolute"))?;
            Ok(Self { file, parents })
        }
        pub(super) fn try_clone(&self) -> io::Result<Self> {
            Ok(Self {
                file: self.file.try_clone()?,
                parents: self
                    .parents
                    .iter()
                    .map(File::try_clone)
                    .collect::<io::Result<_>>()?,
            })
        }
        /// Verify the actual opened child's path/type, retaining directory-chain locks.
        pub(in crate::vendor_import::server_path) fn open_entry(
            &self,
            name: &OsStr,
            include_file: bool,
        ) -> io::Result<Option<Opened>> {
            single_name(name)?;
            let parent_path = final_path(&self.file)?;
            let path = parent_path.join(name);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
            if metadata.file_attributes() & REPARSE != 0
                || (!metadata.is_file() && !metadata.is_dir())
                || (metadata.is_file() && !include_file)
            {
                return Ok(None);
            }
            let file = match open(&path, metadata.is_dir()) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
            let actual = file.metadata()?;
            if actual.file_attributes() & REPARSE != 0 {
                return Ok(None);
            }
            let actual_path = final_path(&file)?;
            let held_parent = final_path(&self.file)?;
            if actual_path.parent() != Some(held_parent.as_path()) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Opened table escaped its held directory",
                ));
            }
            Ok(if actual.is_dir() {
                let mut parents = self
                    .parents
                    .iter()
                    .map(File::try_clone)
                    .collect::<io::Result<Vec<_>>>()?;
                parents.push(self.file.try_clone()?);
                Some(Opened::Directory(Self { file, parents }))
            } else if actual.is_file() {
                Some(Opened::File(file))
            } else {
                None
            })
        }
        /// List only while the checked directory and its ancestors remain pinned.
        pub(in crate::vendor_import::server_path) fn names(
            &self,
            limit: usize,
        ) -> io::Result<Vec<OsString>> {
            let mut names = Vec::new();
            // The directory and every ancestor remain held without delete sharing,
            // so read_dir cannot be redirected to a replaced directory path.
            for entry in fs::read_dir(final_path(&self.file)?)? {
                if names.len() == limit {
                    return Err(invalid("Table discovery directory-entry limit exceeded"));
                }
                names.push(entry?.file_name());
            }
            Ok(names)
        }
    }
}
