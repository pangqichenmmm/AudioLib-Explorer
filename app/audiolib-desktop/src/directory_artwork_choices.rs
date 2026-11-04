// app/audiolib-desktop/src/directory_artwork_choices.rs
//! Read explicit directory choices without falling back after damaged preferences.
use image::RgbaImage;
use std::{
    fs::{File, Metadata, OpenOptions},
    io::{self, Read},
    path::Path,
};

const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_HIDDEN_BYTES: u64 = 4096;
const MAX_RECEIPT_BYTES: u64 = 64;

/// A directory's explicit preference; Embedded suppresses inherited album choices.
#[derive(Debug)]
pub(super) enum Choice {
    Hidden,
    Pixels(RgbaImage),
    Embedded,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn valid_key(key: &str) -> io::Result<()> {
    if key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid artwork key",
        ))
    }
}

fn linked(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn directory_exists(directory: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) if linked(&metadata) || !metadata.is_dir() => Err(invalid(
            "Artwork choice storage is linked or is not a directory",
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn open_no_follow(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_FLAG_OPEN_REPARSE_POINT opens the leaf itself. FILE_SHARE_READ keeps
    // that identity stable while the existing receipt reader opens it read-only.
    OpenOptions::new()
        .read(true)
        .share_mode(1)
        .custom_flags(0x0020_0000)
        .open(path)
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
fn open_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NOFOLLOW | O_NONBLOCK: refuse leaf links, and do not wait on a FIFO if
    // the pathname changed after metadata inspection. No FFI or new dependency.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = 0x0002_0000 | 0x0000_0800;
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let flags = 0x0000_0100 | 0x0000_0004;
    OpenOptions::new().read(true).custom_flags(flags).open(path)
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
fn open_no_follow(_path: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "No-follow artwork reads are unavailable on this host",
    ))
}

fn regular_file(path: &Path, maximum: u64) -> io::Result<Option<File>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if linked(&metadata) || !metadata.is_file() || metadata.len() > maximum {
        return Err(invalid(
            "Artwork choice is linked, non-regular or oversized",
        ));
    }
    let file = open_no_follow(path)?;
    let metadata = file.metadata()?;
    if linked(&metadata) || !metadata.is_file() || metadata.len() > maximum {
        return Err(invalid(
            "Opened artwork choice is not a bounded regular file",
        ));
    }
    Ok(Some(file))
}

struct Files {
    image: Option<File>,
    hidden: Option<File>,
    // Retain the no-follow handle through receipt validation and choice reading.
    _receipt: Option<File>,
    completed: bool,
}

fn files(directory: &Path, key: &str) -> io::Result<Files> {
    valid_key(key)?;
    if !directory_exists(directory)? {
        return Ok(Files {
            image: None,
            hidden: None,
            _receipt: None,
            completed: false,
        });
    }
    let image = regular_file(&directory.join(format!("{key}.png")), MAX_IMAGE_BYTES)?;
    let hidden = regular_file(&directory.join(format!("{key}.hidden")), MAX_HIDDEN_BYTES)?;
    let receipt = regular_file(
        &directory.join(format!("{key}.migrated")),
        MAX_RECEIPT_BYTES,
    )?;
    // Reuse the existing exact, bounded format validator. A malformed receipt
    // must not silently make an inherited whole-album preference active again.
    let completed = if receipt.is_some() {
        crate::album_override_migration::completed(directory, key)?
    } else {
        false
    };
    Ok(Files {
        image,
        hidden,
        _receipt: receipt,
        completed,
    })
}

/// Whether this directory has an explicit image, hidden marker or valid receipt.
/// Invalid paths and receipts are errors; no inherited choice is admitted then.
#[cfg(test)]
pub(super) fn explicitly_configured(directory: &Path, key: &str) -> io::Result<bool> {
    let choices = files(directory, key)?;
    Ok(choices.image.is_some() || choices.hidden.is_some() || choices.completed)
}

/// Resolve hidden, image, then explicit embedded preference in that order.
/// Invalid images fail rather than falling back to another directory or album.
pub(super) fn read_choice(directory: &Path, key: &str, side: u32) -> io::Result<Option<Choice>> {
    if side == 0 || side > 512 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid artwork thumbnail side",
        ));
    }
    let choices = files(directory, key)?;
    if choices.hidden.is_some() {
        return Ok(Some(Choice::Hidden));
    }
    if let Some(file) = choices.image {
        let mut bytes = Vec::new();
        file.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(invalid("Artwork image grew beyond the encoded bound"));
        }
        let pixels = crate::artwork::decode_thumbnail(&bytes, side)
            .ok_or_else(|| invalid("Explicit artwork image is invalid or exceeds decode limits"))?;
        return Ok(Some(Choice::Pixels(pixels)));
    }
    Ok(choices.completed.then_some(Choice::Embedded))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef";

    fn image(directory: &Path) {
        crate::artwork::save_thumbnail(
            &directory.join(format!("{KEY}.png")),
            &RgbaImage::from_pixel(8, 4, image::Rgba([12, 34, 56, 255])),
        )
        .unwrap();
    }

    #[test]
    fn missing_choices_are_not_explicit_and_keys_cannot_escape_storage() {
        let root = tempfile::tempdir().unwrap();
        let absent = root.path().join("absent");
        assert!(!explicitly_configured(&absent, KEY).unwrap());
        assert!(read_choice(&absent, KEY, 40).unwrap().is_none());
        for key in [
            "../0123456789abc",
            "0123456789abcdeg",
            "",
            "0123456789abcdef0",
        ] {
            assert!(explicitly_configured(root.path(), key).is_err());
            assert!(read_choice(root.path(), key, 40).is_err());
        }
        assert!(read_choice(root.path(), KEY, 0).is_err());
        assert!(read_choice(root.path(), KEY, 513).is_err());
    }

    #[test]
    fn valid_receipt_means_embedded_and_does_not_rewrite_preferences() {
        let root = tempfile::tempdir().unwrap();
        crate::album_override_migration::mark_completed(root.path(), KEY).unwrap();
        let receipt = root.path().join(format!("{KEY}.migrated"));
        let original = std::fs::read(&receipt).unwrap();
        assert!(explicitly_configured(root.path(), KEY).unwrap());
        assert!(matches!(
            read_choice(root.path(), KEY, 40).unwrap(),
            Some(Choice::Embedded)
        ));
        assert_eq!(std::fs::read(receipt).unwrap(), original);
    }

    #[test]
    fn hidden_then_image_then_embedded_is_the_explicit_priority() {
        let root = tempfile::tempdir().unwrap();
        crate::album_override_migration::mark_completed(root.path(), KEY).unwrap();
        image(root.path());
        let Some(Choice::Pixels(pixels)) = read_choice(root.path(), KEY, 4).unwrap() else {
            panic!("Explicit image must override an embedded receipt");
        };
        assert_eq!(pixels.dimensions(), (4, 2));
        assert_eq!(pixels.get_pixel(0, 0).0, [12, 34, 56, 255]);
        let hidden = root.path().join(format!("{KEY}.hidden"));
        std::fs::write(&hidden, []).unwrap();
        std::fs::write(
            root.path().join(format!("{KEY}.png")),
            b"damaged lower-priority image",
        )
        .unwrap();
        assert!(matches!(
            read_choice(root.path(), KEY, 40).unwrap(),
            Some(Choice::Hidden)
        ));
        std::fs::remove_file(hidden).unwrap();
        assert!(explicitly_configured(root.path(), KEY).unwrap());
        assert!(read_choice(root.path(), KEY, 40).is_err());
    }

    #[test]
    fn damaged_or_oversized_receipts_cannot_restore_inheritance() {
        let root = tempfile::tempdir().unwrap();
        let receipt = root.path().join(format!("{KEY}.migrated"));
        for bytes in [
            b"unknown receipt".to_vec(),
            vec![0; MAX_RECEIPT_BYTES as usize + 1],
        ] {
            std::fs::write(&receipt, bytes).unwrap();
            assert!(explicitly_configured(root.path(), KEY).is_err());
            assert!(read_choice(root.path(), KEY, 40).is_err());
        }
    }

    #[test]
    fn non_regular_choice_paths_and_oversized_payloads_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        for (extension, maximum) in [
            ("png", MAX_IMAGE_BYTES),
            ("hidden", MAX_HIDDEN_BYTES),
            ("migrated", MAX_RECEIPT_BYTES),
        ] {
            let path = root.path().join(format!("{KEY}.{extension}"));
            std::fs::create_dir(&path).unwrap();
            assert!(explicitly_configured(root.path(), KEY).is_err());
            assert!(read_choice(root.path(), KEY, 40).is_err());
            std::fs::remove_dir(&path).unwrap();
            File::create(&path).unwrap().set_len(maximum + 1).unwrap();
            assert!(explicitly_configured(root.path(), KEY).is_err());
            assert!(read_choice(root.path(), KEY, 40).is_err());
            std::fs::remove_file(path).unwrap();
        }
        let non_directory = root.path().join("not-a-directory");
        std::fs::write(&non_directory, b"owned file").unwrap();
        assert!(read_choice(&non_directory, KEY, 40).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn linked_storage_and_choice_files_are_never_followed() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = root.path().join("linked-directory");
        symlink(&target, &link).unwrap();
        assert!(read_choice(&link, KEY, 40).is_err());
        let original = root.path().join("untouched");
        std::fs::write(&original, b"private fixture bytes").unwrap();
        for extension in ["png", "hidden", "migrated"] {
            let path = target.join(format!("{KEY}.{extension}"));
            symlink(&original, &path).unwrap();
            assert!(explicitly_configured(&target, KEY).is_err());
            assert!(read_choice(&target, KEY, 40).is_err());
            std::fs::remove_file(path).unwrap();
        }
        assert_eq!(std::fs::read(original).unwrap(), b"private fixture bytes");
    }
}
