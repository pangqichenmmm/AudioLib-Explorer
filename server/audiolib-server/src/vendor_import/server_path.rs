// server/audiolib-server/src/vendor_import/server_path.rs
//! Read-only, bounded table discovery inside an explicitly configured server root.
use super::{MAX_FILES, UPLOAD_LIMIT, upload_error};
use crate::{ApiError, AppState};
use audiolib_core::{
    DataProvider,
    vendor_import::{VendorImportOptions, VendorImportPreview, VendorLimits},
};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

#[path = "rooted.rs"]
mod rooted;
use rooted::{Directory, Opened};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ServerPathPreview {
    root: String,
    root_relative_dir: String,
    #[serde(default)]
    options: VendorImportOptions,
}

fn relative(raw: &str) -> Result<PathBuf, ApiError> {
    if raw.contains('\0') || raw.contains(':') || raw.starts_with(['/', '\\']) {
        return Err(upload_error(
            "root_relative_dir must remain inside the named root",
        ));
    }
    let normalized = raw.replace('\\', "/");
    let path = Path::new(&normalized);
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(upload_error(
            "root_relative_dir must not contain parent or absolute components",
        ));
    }
    Ok(path.to_owned())
}

fn discover(root: &Directory, directory: &Path) -> Result<Vec<PathBuf>, ApiError> {
    let mut pending = vec![directory.to_owned()];
    let mut visited = BTreeSet::new();
    let mut files = Vec::new();
    let mut examined = 0_usize;
    while let Some(directory) = pending.pop() {
        if !visited.insert(directory.clone()) {
            continue;
        }
        let opened = root
            .descend(&directory)
            .map_err(audiolib_core::Error::from)?;
        let names=opened.names(100_000-examined).map_err(|error| {
            if error.kind()==std::io::ErrorKind::InvalidInput {
                upload_error("Server table discovery exceeds 100,000 directory entries; select a smaller directory")
            } else { ApiError::from(audiolib_core::Error::from(error)) }
        })?;
        for name in names {
            examined += 1;
            if examined > 100_000 {
                return Err(upload_error(
                    "Server table discovery exceeds 100,000 directory entries; select a smaller directory",
                ));
            }
            let path = directory.join(&name);
            let supported = path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|extension| {
                    ["csv", "tsv", "txt", "xls", "xlsx"]
                        .iter()
                        .any(|known| extension.eq_ignore_ascii_case(known))
                });
            match opened
                .open_entry(&name, supported)
                .map_err(audiolib_core::Error::from)?
            {
                Some(Opened::Directory(_)) => {
                    pending.push(path);
                    continue;
                }
                Some(Opened::File(_)) => {}
                None => continue,
            }
            if !supported {
                continue;
            }
            files.push(path);
            if files.len() > MAX_FILES {
                return Err(upload_error(
                    "One preview accepts at most 512 tables; select a smaller directory",
                ));
            }
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(upload_error(
            "No supported vendor tables in the selected directory",
        ));
    }
    Ok(files)
}

struct Staged {
    _directory: tempfile::TempDir,
    files: Vec<PathBuf>,
}

fn copy_bounded(
    input: &mut fs::File,
    output: &mut fs::File,
    total: &mut u64,
) -> Result<(), ApiError> {
    let maximum = VendorLimits::default().max_file_bytes + 1;
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    while copied < maximum {
        let remaining = (UPLOAD_LIMIT as u64).saturating_sub(*total);
        if remaining == 0 {
            let mut probe = [0];
            if input.read(&mut probe).map_err(audiolib_core::Error::from)? != 0 {
                return Err(upload_error(
                    "Server table batch exceeds the 512 MiB input budget",
                ));
            }
            break;
        }
        let want = (maximum - copied).min(remaining).min(buffer.len() as u64) as usize;
        let count = input
            .read(&mut buffer[..want])
            .map_err(audiolib_core::Error::from)?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(audiolib_core::Error::from)?;
        copied += count as u64;
        *total += count as u64;
    }
    Ok(())
}

fn stage_files(
    root: &Directory,
    files: &[PathBuf],
    mut after_open: impl FnMut(&Path),
) -> Result<Staged, ApiError> {
    let directory = tempfile::Builder::new()
        .prefix("audiolib-vendor-root-")
        .tempdir()
        .map_err(audiolib_core::Error::from)?;
    let mut staged = Vec::new();
    let mut total = 0;
    for relative in files {
        let Some(mut input) = root
            .open_file(relative)
            .map_err(audiolib_core::Error::from)?
        else {
            continue;
        };
        // The hook is a no-op in production and permits deterministic replacement
        // regressions after authorization but before the first source byte is read.
        after_open(relative);
        let parent = directory.path().join(staged.len().to_string());
        fs::create_dir(&parent).map_err(audiolib_core::Error::from)?;
        let path = parent.join(
            relative
                .file_name()
                .ok_or_else(|| upload_error("Missing table file name"))?,
        );
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(audiolib_core::Error::from)?;
        // Retain max+1 bytes for an oversized file. The shared importer reports its
        // normal per-file refusal while still parsing other valid files in this batch.
        copy_bounded(&mut input, &mut output, &mut total)?;
        output.flush().map_err(audiolib_core::Error::from)?;
        drop(output);
        staged.push(path);
    }
    if staged.is_empty() {
        return Err(upload_error(
            "No supported regular tables remain in the selected directory",
        ));
    }
    Ok(Staged {
        _directory: directory,
        files: staged,
    })
}

pub(super) async fn preview(
    state: AppState,
    mut input: ServerPathPreview,
) -> Result<VendorImportPreview, ApiError> {
    let relative = relative(&input.root_relative_dir)?;
    let monitor = state.root_monitor();
    let configured = monitor
        .roots()
        .iter()
        .find(|root| root.name == input.root)
        .ok_or_else(|| upload_error("Unknown configured vendor root"))?
        .server_path
        .clone();
    tokio::task::spawn_blocking(move || {
        let root = configured
            .canonicalize()
            .map_err(audiolib_core::Error::from)?;
        let opened = Directory::open_root(&root).map_err(audiolib_core::Error::from)?;
        let files = discover(&opened, &relative)?;
        let staged = stage_files(&opened, &files, |_| {})?;
        // No external path will be reopened by the provider. Release the source
        // directory pins before potentially long parsing of the private copies.
        drop(opened);
        if input.options.vendor_root.is_none() {
            input.options.vendor_root = Some(audiolib_db::display_path(&root));
        }
        state
            .provider()
            .vendor_import_preview_files(&staged.files, &input.options)
            .map_err(ApiError::from)
    })
    .await
    .map_err(|error| ApiError::internal(format!("Vendor table discovery failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_paths_reject_cross_platform_escape_before_filesystem_access() {
        for path in [
            "../private",
            "nested/../../private",
            "/private",
            "\\private",
            "C:\\private",
            "C:private",
            "a\0b",
        ] {
            assert!(relative(path).is_err(), "{path:?}");
        }
        assert_eq!(
            relative("tables\\sub").unwrap(),
            PathBuf::from("tables/sub")
        );
        assert!(relative("").is_ok());
    }

    #[test]
    fn table_discovery_is_bounded_and_reads_only_selected_regular_tables() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let selected = root.join("tables");
        fs::create_dir_all(selected.join("nested")).unwrap();
        fs::write(
            selected.join("first.csv.xls"),
            "filename,description\na.wav,wind\n",
        )
        .unwrap();
        fs::write(selected.join("nested/second.XLSX"), "fixture").unwrap();
        fs::write(selected.join("source.wav"), "untouched").unwrap();
        let opened = Directory::open_root(&root).unwrap();
        assert_eq!(discover(&opened, Path::new("tables")).unwrap().len(), 2);
        for index in 0..MAX_FILES {
            fs::write(selected.join(format!("extra-{index}.csv")), "x").unwrap();
        }
        assert!(discover(&opened, Path::new("tables")).is_err());
        assert_eq!(
            fs::read_to_string(selected.join("source.wav")).unwrap(),
            "untouched"
        );
    }

    #[test]
    fn staging_copies_the_authorized_handle_even_when_its_source_path_is_replaced() {
        let owned = tempfile::tempdir().unwrap();
        let root = owned.path().join("root");
        fs::create_dir(&root).unwrap();
        let table = root.join("table.csv");
        fs::write(&table, b"filename,description\none.wav,allowed\n").unwrap();
        let outside = owned.path().join("outside.csv");
        fs::write(&outside, b"filename,description\none.wav,secret\n").unwrap();
        let opened = Directory::open_root(&root).unwrap();
        let files = discover(&opened, Path::new("")).unwrap();
        let staged = stage_files(&opened, &files, |_| {
            match fs::rename(&table, owned.path().join("previous.csv")) {
                Ok(()) => {
                    fs::copy(&outside, &table).unwrap();
                }
                Err(error) => {
                    // Windows deliberately denies rename while the authorized file
                    // handle lacks FILE_SHARE_DELETE. Unix retains the opened inode.
                    #[cfg(windows)]
                    assert!(matches!(error.raw_os_error(), Some(5 | 32 | 33)), "{error}");
                    #[cfg(not(windows))]
                    panic!("Owned Unix fixture rename unexpectedly failed: {error}");
                }
            }
        })
        .unwrap();
        assert_eq!(staged.files[0].file_name().unwrap(), "table.csv");
        assert_eq!(
            fs::read(&staged.files[0]).unwrap(),
            b"filename,description\none.wav,allowed\n"
        );
        assert_eq!(
            fs::read(&outside).unwrap(),
            b"filename,description\none.wav,secret\n"
        );
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn a_link_substituted_after_discovery_is_not_read_or_sent_to_the_parser() {
        let owned = tempfile::tempdir().unwrap();
        let root = owned.path().join("root");
        fs::create_dir(&root).unwrap();
        let table = root.join("table.csv");
        fs::write(&table, b"allowed").unwrap();
        let other = root.join("other.csv");
        fs::write(&other, b"other").unwrap();
        let outside = owned.path().join("outside.csv");
        fs::write(&outside, b"secret").unwrap();
        let opened = Directory::open_root(&root).unwrap();
        let files = discover(&opened, Path::new("")).unwrap();
        fs::rename(&table, owned.path().join("previous.csv")).unwrap();
        #[cfg(unix)]
        let link = std::os::unix::fs::symlink(&outside, &table);
        #[cfg(windows)]
        let link = std::os::windows::fs::symlink_file(&outside, &table);
        #[cfg(windows)]
        if link
            .as_ref()
            .is_err_and(|error| error.raw_os_error() == Some(1314))
        {
            eprintln!(
                "Windows source-link replacement requires Developer Mode or symlink privilege"
            );
            return;
        }
        link.unwrap();
        let staged = stage_files(&opened, &files, |_| {}).unwrap();
        assert_eq!(staged.files.len(), 1);
        assert_eq!(staged.files[0].file_name().unwrap(), "other.csv");
        assert_eq!(fs::read(&staged.files[0]).unwrap(), b"other");
    }

    #[test]
    fn overlarge_files_remain_per_file_refusals_and_other_tables_still_preview() {
        let owned = tempfile::tempdir().unwrap();
        let root = owned.path().join("tables");
        fs::create_dir(&root).unwrap();
        let large = fs::File::create(root.join("large.csv")).unwrap();
        large
            .set_len(VendorLimits::default().max_file_bytes + 20)
            .unwrap();
        drop(large);
        fs::write(
            root.join("valid.csv"),
            b"filename,description\none.wav,wind\n",
        )
        .unwrap();
        let opened = Directory::open_root(&root).unwrap();
        let staged =
            stage_files(&opened, &discover(&opened, Path::new("")).unwrap(), |_| {}).unwrap();
        assert_eq!(
            fs::metadata(&staged.files[0]).unwrap().len(),
            VendorLimits::default().max_file_bytes + 1
        );
        let database = owned.path().join("library.db");
        let provider = audiolib_db::SqliteProvider::open(&database).unwrap();
        let conn = rusqlite::Connection::open(&database).unwrap();
        audiolib_db::register_library_functions(&conn).unwrap();
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename) VALUES(1,'owned/one.wav','one.wav')",
            [],
        )
        .unwrap();
        drop(conn);
        let preview = provider
            .vendor_import_preview_files(&staged.files, &Default::default())
            .unwrap();
        assert!(
            preview
                .files
                .iter()
                .find(|file| file.name == "large.csv")
                .unwrap()
                .error
                .is_some()
        );
        assert!(
            preview
                .files
                .iter()
                .find(|file| file.name == "valid.csv")
                .unwrap()
                .error
                .is_none()
        );
    }

    #[test]
    fn staging_never_writes_past_the_shared_batch_budget() {
        let owned = tempfile::tempdir().unwrap();
        let source = owned.path().join("source.csv");
        fs::write(&source, b"abc").unwrap();
        let destination = owned.path().join("copy.csv");
        let mut input = fs::File::open(source).unwrap();
        let mut output = fs::File::create(&destination).unwrap();
        let mut total = UPLOAD_LIMIT as u64 - 2;
        assert!(copy_bounded(&mut input, &mut output, &mut total).is_err());
        assert_eq!(total, UPLOAD_LIMIT as u64);
        assert_eq!(fs::read(&destination).unwrap(), b"ab");
    }

    #[test]
    #[cfg(windows)]
    fn held_directory_chain_blocks_parent_replacement_during_enumeration() {
        let owned = tempfile::tempdir().unwrap();
        let root = owned.path().join("root");
        fs::create_dir_all(root.join("tables")).unwrap();
        fs::write(root.join("tables/valid.csv"), b"allowed").unwrap();
        let opened = Directory::open_root(&root).unwrap();
        let selected = opened.descend(Path::new("tables")).unwrap();
        assert!(fs::rename(root.join("tables"), owned.path().join("moved")).is_err());
        assert!(fs::rename(&root, owned.path().join("moved-root")).is_err());
        assert_eq!(
            selected.names(10).unwrap(),
            [std::ffi::OsString::from("valid.csv")]
        );
    }
}
