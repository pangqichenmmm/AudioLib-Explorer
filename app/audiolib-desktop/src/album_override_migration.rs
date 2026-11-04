// app/audiolib-desktop/src/album_override_migration.rs
//! Non-destructive migration of old cover choices into one shared album identity.
use std::{
    collections::HashSet,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const RECEIPT: &[u8] = b"shared-album-choice-v1\n";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    NotNeeded,
    AlreadyConfigured,
    Migrated,
    Conflict(Vec<PathBuf>),
}

fn valid_key(key: &str) -> bool {
    key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn regular(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(info) if info.file_type().is_symlink() || !info.is_file() => {
            Err(std::io::Error::other("linked or non-file cover choice"))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Enumerate only the application's flat legacy choice names, never source WAVs.
pub(crate) fn choice_keys(directory: &Path) -> std::io::Result<HashSet<String>> {
    match std::fs::symlink_metadata(directory) {
        Ok(info) if info.file_type().is_symlink() || !info.is_dir() => {
            return Err(std::io::Error::other(
                "linked or non-directory cover storage",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => return Err(error),
    }
    let mut keys = HashSet::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some((key, extension)) = name.to_str().and_then(|name| name.rsplit_once('.')) else {
            continue;
        };
        if valid_key(key) && matches!(extension, "png" | "hidden" | "migrated") {
            if entry.file_type()?.is_symlink() || !entry.file_type()?.is_file() {
                return Err(std::io::Error::other("linked or non-file legacy cover"));
            }
            keys.insert(key.to_owned());
        }
    }
    Ok(keys)
}

#[derive(PartialEq, Eq)]
enum Choice {
    Hidden,
    Image(Vec<u8>),
}

/// Read the bounded, exact existing receipt format. Callers validate the key
/// before constructing paths; malformed receipts fail instead of being ignored.
pub(crate) fn completed(directory: &Path, key: &str) -> std::io::Result<bool> {
    let path = directory.join(format!("{key}.migrated"));
    if !regular(&path)? {
        return Ok(false);
    }
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(RECEIPT.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes != RECEIPT {
        return Err(std::io::Error::other("unknown cover migration receipt"));
    }
    Ok(true)
}

/// Record an explicit shared choice, including "use embedded". This prevents a
/// future startup from resurrecting retained pre-migration custom cover files.
pub(crate) fn mark_completed(directory: &Path, key: &str) -> std::io::Result<()> {
    if !valid_key(key) {
        return Err(std::io::Error::other("invalid cover identity"));
    }
    let _ = choice_keys(directory)?;
    if completed(directory, key)? {
        return Ok(());
    }
    std::fs::create_dir_all(directory)?;
    let mut staged = tempfile::NamedTempFile::new_in(directory)?;
    staged.write_all(RECEIPT)?;
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(directory.join(format!("{key}.migrated"))) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if completed(directory, key)? {
                Ok(())
            } else {
                Err(std::io::Error::other("cover migration receipt changed"))
            }
        }
        Err(error) => Err(error.error),
    }
}

/// The caller supplies every matching old member key and serializes persistent
/// actions. Different old choices remain untouched for explicit user resolution.
pub(crate) fn migrate(
    directory: &Path,
    new_key: &str,
    old_keys: &[String],
) -> std::io::Result<Outcome> {
    if !valid_key(new_key) || old_keys.iter().any(|key| !valid_key(key)) {
        return Err(std::io::Error::other("invalid cover identity"));
    }
    // Validate the directory even if the caller has already collected its names.
    let _ = choice_keys(directory)?;
    if completed(directory, new_key)? {
        return Ok(Outcome::NotNeeded);
    }
    let new_image = directory.join(format!("{new_key}.png"));
    let new_hidden = directory.join(format!("{new_key}.hidden"));
    if regular(&new_image)? || regular(&new_hidden)? {
        mark_completed(directory, new_key)?;
        return Ok(Outcome::AlreadyConfigured);
    }
    // A previously migrated shared alias is newer than the original per-WAV
    // choices. Its receipt also carries an explicit restoration to embedded art.
    let preferred = old_keys
        .iter()
        .find_map(|key| match completed(directory, key) {
            Ok(true) => Some(Ok(key.clone())),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .transpose()?;
    let keys = preferred
        .as_ref()
        .map(std::slice::from_ref)
        .unwrap_or(old_keys);
    let mut chosen = None;
    let mut paths = Vec::new();
    let mut conflict = false;
    for key in keys {
        let hidden = directory.join(format!("{key}.hidden"));
        let image = directory.join(format!("{key}.png"));
        let has_hidden = regular(&hidden)?;
        let has_image = regular(&image)?;
        let choice = if has_hidden {
            paths.push(hidden);
            Some(Choice::Hidden)
        } else if has_image {
            let file = std::fs::File::open(&image)?;
            if file.metadata()?.len() > MAX_IMAGE_BYTES {
                return Err(std::io::Error::other("legacy cover exceeds image budget"));
            }
            let mut bytes = Vec::new();
            file.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_IMAGE_BYTES
                || crate::artwork::decode_thumbnail(&bytes, 512).is_none()
            {
                return Err(std::io::Error::other("invalid legacy cover image"));
            }
            paths.push(image);
            Some(Choice::Image(bytes))
        } else {
            None
        };
        if let Some(choice) = choice {
            if let Some(previous) = &chosen {
                conflict |= previous != &choice;
            } else {
                chosen = Some(choice);
            }
        }
    }
    if conflict {
        return Ok(Outcome::Conflict(paths));
    }
    let Some(choice) = chosen else {
        if preferred.is_some() {
            mark_completed(directory, new_key)?;
            return Ok(Outcome::Migrated);
        }
        return Ok(Outcome::NotNeeded);
    };
    std::fs::create_dir_all(directory)?;
    let mut staged = tempfile::NamedTempFile::new_in(directory)?;
    let destination = match choice {
        Choice::Hidden => new_hidden,
        Choice::Image(bytes) => {
            staged.write_all(&bytes)?;
            new_image
        }
    };
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(destination) {
        Ok(_) => {
            mark_completed(directory, new_key)?;
            Ok(Outcome::Migrated)
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            mark_completed(directory, new_key)?;
            Ok(Outcome::AlreadyConfigured)
        }
        Err(error) => Err(error.error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const OLD_A: &str = "0000000000000001";
    const OLD_B: &str = "0000000000000002";
    const NEW: &str = "0000000000000003";
    fn image(directory: &Path, key: &str, value: u8) -> Vec<u8> {
        let pixels = image::RgbaImage::from_pixel(2, 2, image::Rgba([value, 0, 0, 255]));
        let path = directory.join(format!("{key}.png"));
        crate::artwork::save_thumbnail(&path, &pixels).unwrap();
        std::fs::read(path).unwrap()
    }
    #[test]
    fn identical_choices_merge_without_removing_legacy_files() {
        let directory = tempfile::tempdir().unwrap();
        let original = image(directory.path(), OLD_A, 4);
        image(directory.path(), OLD_B, 4);
        assert_eq!(
            migrate(directory.path(), NEW, &[OLD_A.into(), OLD_B.into()]).unwrap(),
            Outcome::Migrated
        );
        for key in [OLD_A, OLD_B, NEW] {
            assert_eq!(
                std::fs::read(directory.path().join(format!("{key}.png"))).unwrap(),
                original
            );
        }
    }
    #[test]
    fn conflicting_images_and_hidden_choices_are_preserved_without_picking_a_winner() {
        let directory = tempfile::tempdir().unwrap();
        image(directory.path(), OLD_A, 4);
        image(directory.path(), OLD_B, 9);
        assert!(matches!(
            migrate(directory.path(), NEW, &[OLD_A.into(), OLD_B.into()]).unwrap(),
            Outcome::Conflict(_)
        ));
        assert!(!directory.path().join(format!("{NEW}.png")).exists());
        std::fs::write(directory.path().join(format!("{OLD_B}.hidden")), []).unwrap();
        assert!(matches!(
            migrate(directory.path(), NEW, &[OLD_A.into(), OLD_B.into()]).unwrap(),
            Outcome::Conflict(_)
        ));
        assert!(directory.path().join(format!("{OLD_A}.png")).is_file());
        assert!(directory.path().join(format!("{OLD_B}.png")).is_file());
    }
    #[test]
    fn new_choice_wins_and_failures_never_delete_old_configuration() {
        let directory = tempfile::tempdir().unwrap();
        image(directory.path(), OLD_A, 4);
        let current = image(directory.path(), NEW, 9);
        assert_eq!(
            migrate(directory.path(), NEW, &[OLD_A.into()]).unwrap(),
            Outcome::AlreadyConfigured
        );
        assert_eq!(
            std::fs::read(directory.path().join(format!("{NEW}.png"))).unwrap(),
            current
        );
        assert!(migrate(directory.path(), "../outside", &[OLD_A.into()]).is_err());
        assert!(directory.path().join(format!("{OLD_A}.png")).is_file());
    }
    #[test]
    fn explicit_restore_and_newer_shared_alias_never_resurrect_old_member_choices() {
        let directory = tempfile::tempdir().unwrap();
        image(directory.path(), OLD_A, 4);
        mark_completed(directory.path(), OLD_B).unwrap();
        assert_eq!(
            migrate(directory.path(), NEW, &[OLD_B.into(), OLD_A.into()]).unwrap(),
            Outcome::Migrated
        );
        assert!(!directory.path().join(format!("{NEW}.png")).exists());
        assert!(directory.path().join(format!("{OLD_A}.png")).is_file());
        assert_eq!(
            migrate(directory.path(), NEW, &[OLD_A.into()]).unwrap(),
            Outcome::NotNeeded
        );
    }
    #[cfg(unix)]
    #[test]
    fn linked_legacy_images_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        image(outside.path(), OLD_A, 4);
        std::os::unix::fs::symlink(
            outside.path().join(format!("{OLD_A}.png")),
            directory.path().join(format!("{OLD_A}.png")),
        )
        .unwrap();
        assert!(migrate(directory.path(), NEW, &[OLD_A.into()]).is_err());
        assert!(!directory.path().join(format!("{NEW}.png")).exists());
    }
}
