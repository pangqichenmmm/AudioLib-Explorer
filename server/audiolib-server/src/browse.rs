// server/audiolib-server/src/browse.rs
//! Read-only directory browser behind `GET /api/v1/fs` and `GET /admin/api/fs`: lists
//! the sub-directories of a server directory so an administrator can pick a library
//! root. It never writes, deletes or recurses; every listing is one `read_dir` of the
//! requested directory (cheap on SMB mounts) with a cap on the entries returned.
use crate::ApiError;
use serde::Serialize;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Most sub-directories returned for one directory (`truncated` is set beyond it).
pub const MAX_ENTRIES: usize = 2000;
/// Most directory entries examined for the file counts of one directory.
pub const MAX_SCANNED_ENTRIES: usize = 50_000;

/// One sub-directory of a listing.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct FsEntry {
    pub name: String,
    /// Absolute path (a symbolic link keeps its own path; it is resolved when entered).
    pub path: String,
    pub is_symlink: bool,
}

/// `GET /admin/api/fs?path=<absolute directory>`.
#[derive(Debug, Serialize)]
pub struct FsListing {
    /// Canonical path of the listed directory.
    pub path: String,
    /// Canonical parent directory (`null` at the file system root).
    pub parent: Option<String>,
    /// Whether the directory is on a network file system (SMB, NFS, AFP, WebDAV), when
    /// the platform tells cheaply.
    pub network: Option<bool>,
    /// Sub-directories, case-insensitively sorted (hidden ones only with `hidden=1`).
    pub entries: Vec<FsEntry>,
    /// More than [`MAX_ENTRIES`] sub-directories exist.
    pub truncated: bool,
    /// `.wav` files directly in this directory (not in sub-directories).
    pub wav_files: u64,
    /// Supported WAV, FLAC and Ogg source files directly in this directory.
    pub audio_files: u64,
    /// All other files directly in this directory.
    pub other_files: u64,
    /// The listing stopped after [`MAX_SCANNED_ENTRIES`] entries: the file counts and
    /// possibly `entries` are incomplete, even when `truncated` is false.
    pub counts_truncated: bool,
}

/// A starting point of the browser.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Place {
    /// `root` (file system root), `volume`, `home` or `library_root`.
    pub kind: &'static str,
    pub name: String,
    pub path: String,
    pub network: Option<bool>,
}

/// `GET /admin/api/fs` without a path: where to start.
#[derive(Debug, Serialize)]
pub struct FsPlaces {
    pub places: Vec<Place>,
}

/// Convert an I/O error of the browsed directory into an API error (Chinese messages
/// for the admin page, with the macOS privacy hint for permission errors).
fn io_error(path: &str, error: io::Error) -> ApiError {
    match error.kind() {
        io::ErrorKind::NotFound => {
            ApiError::not_found(format!("目录不存在：{path}（磁盘可能未挂载）"))
        }
        io::ErrorKind::PermissionDenied => ApiError::fs_forbidden(format!(
            "没有权限读取 {path}：请检查目录权限；在 macOS 上还可能是系统隐私保护（TCC）拦截，\
             请在服务器上打开“系统设置 → 隐私与安全性 → 完全磁盘访问权限”并允许 AudioLib 服务端，\
             或在服务器屏幕上点击允许的弹窗后重试"
        )),
        io::ErrorKind::NotADirectory => ApiError::validation(format!("不是目录：{path}")),
        _ => ApiError::from(audiolib_core::Error::Io(error)),
    }
}

/// Canonical, verbatim-free absolute directory for `raw`: 400 for an empty or relative
/// path or one that is not a directory, 404 when missing, 403 on permission errors.
pub fn resolve_directory(raw: &str) -> Result<PathBuf, ApiError> {
    if raw.is_empty() || raw.contains('\0') {
        return Err(ApiError::validation(
            "path must be a non-empty absolute path",
        ));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(ApiError::validation(format!(
            "路径必须是绝对路径（path must be absolute）：{raw}"
        )));
    }
    let canonical = fs::canonicalize(path).map_err(|error| io_error(raw, error))?;
    let canonical = PathBuf::from(audiolib_db::display_path(&canonical));
    let metadata = fs::metadata(&canonical).map_err(|error| io_error(raw, error))?;
    if !metadata.is_dir() {
        return Err(ApiError::validation(format!("不是目录：{raw}")));
    }
    Ok(canonical)
}

/// List the directory `raw` (see [`resolve_directory`] for the errors).
pub fn list_directory(raw: &str, hidden: bool) -> Result<FsListing, ApiError> {
    let path = resolve_directory(raw)?;
    let shown = path.display().to_string();
    let reader = fs::read_dir(&path).map_err(|error| io_error(&shown, error))?;
    let mut entries = Vec::new();
    let mut truncated = false;
    let (mut wav_files, mut audio_files, mut other_files, mut scanned) =
        (0_u64, 0_u64, 0_u64, 0_usize);
    let mut counts_truncated = false;
    for entry in reader {
        let Ok(entry) = entry else { continue };
        if scanned >= MAX_SCANNED_ENTRIES {
            counts_truncated = true;
            break;
        }
        scanned += 1;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let (is_dir, is_symlink) = if kind.is_symlink() {
            // A link counts as a directory only when it points at one.
            (fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()), true)
        } else {
            (kind.is_dir(), false)
        };
        if is_dir {
            if !hidden && name.starts_with('.') {
                continue;
            }
            if entries.len() >= MAX_ENTRIES {
                truncated = true;
                continue;
            }
            entries.push(FsEntry {
                path: entry.path().display().to_string(),
                name,
                is_symlink,
            });
        } else if kind.is_file() {
            let audio = audiolib_core::is_supported_audio_extension(Path::new(&name));
            let wav = Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"));
            if audio {
                audio_files += 1;
            }
            if wav {
                wav_files += 1;
            } else {
                other_files += 1;
            }
        }
    }
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(FsListing {
        parent: path.parent().map(|parent| parent.display().to_string()),
        network: is_network_path(&path),
        path: shown,
        entries,
        truncated,
        wav_files,
        audio_files,
        other_files,
        counts_truncated,
    })
}

/// File-system type name of the volume holding `path` (macOS/BSD `statfs`).
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn fs_type_name(path: &Path) -> Option<String> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statfs` writes into the zeroed struct for a valid NUL-terminated path.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let bytes: Vec<u8> = stat
        .f_fstypename
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8(bytes).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
fn fs_type_name(_path: &Path) -> Option<String> {
    None
}

/// Whether a file-system type name denotes a network file system.
pub fn is_network_fs(type_name: &str) -> bool {
    matches!(
        type_name.to_ascii_lowercase().as_str(),
        "smbfs" | "nfs" | "afpfs" | "webdav" | "cifs" | "smb3" | "ftp" | "fuse.sshfs"
    )
}

/// `Some(true)` for a network mount, `Some(false)` for a local volume, `None` when the
/// platform does not say cheaply (one `statfs`, no directory access).
pub fn is_network_path(path: &Path) -> Option<bool> {
    fs_type_name(path).map(|name| is_network_fs(&name))
}

/// Starting points: the file system root, mounted volumes, the home directory and the
/// directories of `library_roots` (name, path).
pub fn places(library_roots: &[(String, PathBuf)]) -> FsPlaces {
    let mut places = Vec::new();
    #[cfg(unix)]
    {
        places.push(Place {
            kind: "root",
            name: "/".into(),
            path: "/".into(),
            network: None,
        });
        if let Ok(reader) = fs::read_dir("/Volumes") {
            let mut volumes: Vec<(String, PathBuf)> = reader
                .take(MAX_ENTRIES)
                .flatten()
                .map(|entry| {
                    (
                        entry.file_name().to_string_lossy().into_owned(),
                        entry.path(),
                    )
                })
                .filter(|(name, _)| !name.starts_with('.'))
                .collect();
            volumes.sort_by_key(|(name, _)| name.to_lowercase());
            for (name, path) in volumes {
                places.push(Place {
                    kind: "volume",
                    network: is_network_path(&path),
                    path: path.display().to_string(),
                    name,
                });
            }
        }
    }
    #[cfg(windows)]
    for letter in b'A'..=b'Z' {
        let path = format!("{}:\\", letter as char);
        if Path::new(&path).exists() {
            places.push(Place {
                kind: "root",
                name: path.clone(),
                path,
                network: None,
            });
        }
    }
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
    {
        places.push(Place {
            kind: "home",
            name: "~".into(),
            network: is_network_path(&home),
            path: home.display().to_string(),
        });
    }
    for (name, path) in library_roots {
        places.push(Place {
            kind: "library_root",
            name: name.clone(),
            network: is_network_path(path),
            path: path.display().to_string(),
        });
    }
    FsPlaces { places }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_file_systems_are_recognised_by_type_name() {
        for name in ["smbfs", "SMBFS", "nfs", "afpfs", "webdav", "cifs"] {
            assert!(is_network_fs(name), "{name}");
        }
        for name in ["apfs", "hfs", "msdos", "exfat", "ntfs", "ext4", ""] {
            assert!(!is_network_fs(name), "{name}");
        }
    }

    #[test]
    fn relative_and_empty_paths_are_rejected() {
        for bad in ["", "relative/dir", "..", "./x", "a\0b"] {
            let error = resolve_directory(bad).unwrap_err();
            assert_eq!(error.status().as_u16(), 400, "{bad:?}");
        }
    }
}
