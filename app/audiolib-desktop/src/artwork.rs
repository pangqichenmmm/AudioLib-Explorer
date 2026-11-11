// app/audiolib-desktop/src/artwork.rs
//! Bounded cover decoding and source-versioned thumbnail cache keys.
use image::{ImageReader, RgbaImage};
use std::{
    io::Cursor,
    path::{Path, PathBuf},
};

const MAX_ENCODED_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIMENSION: u32 = 4096;
const MAX_DECODE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// Stable filename component; no source paths or server addresses appear in filenames.
pub(crate) fn key(parts: &[&str]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for part in parts {
        for byte in part.bytes().chain(std::iter::once(0)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

/// Read only bounded regular image files, including user-selected overrides.
pub(crate) fn read_thumbnail(path: &Path, side: u32) -> Option<RgbaImage> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ENCODED_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_ENCODED_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    decode_thumbnail(&bytes, side)
}

/// Publish complete PNG pixels atomically, retaining no link to the chosen source image.
pub(crate) fn save_thumbnail(path: &Path, pixels: &RgbaImage) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("missing artwork directory"))?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    pixels
        .write_to(file.as_file_mut(), image::ImageFormat::Png)
        .map_err(std::io::Error::other)?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

struct CacheFile {
    modified: std::time::SystemTime,
    bytes: u64,
    path: PathBuf,
}

/// Enumerate only owned flat thumbnail files, using metadata that never follows links.
fn owned_cache_files(cache: &Path) -> std::io::Result<Vec<CacheFile>> {
    let directory = cache.join("artwork");
    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "artwork cache is not an owned directory",
        ));
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some((stem, extension)) = name.rsplit_once('.') else {
            continue;
        };
        if stem.len() != 16
            || !stem.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !matches!(extension, "png" | "etag")
        {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        files.push(CacheFile {
            modified: metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
            bytes: metadata.len(),
            path,
        });
    }
    Ok(files)
}

/// Count owned thumbnail PNGs and ETag sidecars without reading or modifying their contents.
pub(crate) fn cache_stats(cache: &Path) -> std::io::Result<(usize, u64)> {
    let files = owned_cache_files(cache)?;
    let bytes = files
        .iter()
        .fold(0_u64, |total, file| total.saturating_add(file.bytes));
    Ok((files.len(), bytes))
}

/// Clear/prune only this application's flat thumbnail files. Never follow child links.
pub(crate) fn clean_cache(cache: &Path, clear: bool) -> std::io::Result<usize> {
    let mut files = owned_cache_files(cache)?;
    let mut total = files
        .iter()
        .fold(0_u64, |total, file| total.saturating_add(file.bytes));
    files.sort_by_key(|file| file.modified);
    let mut removed = 0;
    for file in files {
        if !clear && total <= MAX_CACHE_BYTES {
            break;
        }
        std::fs::remove_file(file.path)?;
        total = total.saturating_sub(file.bytes);
        removed += 1;
    }
    Ok(removed)
}

/// Decode with strict dimensions and an allocation budget, then retain only display pixels.
pub(crate) fn decode_thumbnail(bytes: &[u8], side: u32) -> Option<RgbaImage> {
    if bytes.len() > MAX_ENCODED_BYTES || side == 0 || side > 512 {
        return None;
    }
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    Some(if decoded.width() > side || decoded.height() > side {
        decoded.thumbnail(side, side).into_rgba8()
    } else {
        decoded.into_rgba8()
    })
}

/// Historical cache-key fixture. Native covers now use shared album identities;
/// these old disposable paths remain distinct in compatibility regressions.
#[cfg(test)]
pub(crate) fn thumbnail_path(
    cache: &Path,
    library: &Path,
    source: &audiolib_core::AudioFile,
    album: &str,
) -> PathBuf {
    let key = key(&[
        "local-v2",
        &library.to_string_lossy(),
        &source.file_path,
        &source.id.to_string(),
        &source.file_size.to_string(),
        source.attributes["modified_at"]
            .as_str()
            .unwrap_or_default(),
        album,
    ]);
    cache.join("artwork").join(format!("{key}.png"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_stats_and_cleanup_share_flat_owned_file_boundaries() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(cache_stats(root.path()).unwrap(), (0, 0));
        let directory = root.path().join("artwork");
        std::fs::create_dir_all(directory.join("nested")).unwrap();
        std::fs::write(directory.join("0123456789abcdef.png"), b"image").unwrap();
        std::fs::write(directory.join("0123456789abcdef.etag"), b"tag").unwrap();
        std::fs::write(directory.join("manual.png"), b"unowned image").unwrap();
        std::fs::write(directory.join("0123456789abcdef.hidden"), b"choice").unwrap();
        std::fs::write(
            directory.join("nested").join("0123456789abcdef.png"),
            b"nested image",
        )
        .unwrap();
        std::fs::create_dir(directory.join("fedcba9876543210.png")).unwrap();

        assert_eq!(cache_stats(root.path()).unwrap(), (2, 8));
        assert_eq!(cache_stats(root.path()).unwrap(), (2, 8));
        assert_eq!(
            std::fs::read(directory.join("0123456789abcdef.png")).unwrap(),
            b"image"
        );
        assert_eq!(clean_cache(root.path(), true).unwrap(), 2);
        assert_eq!(cache_stats(root.path()).unwrap(), (0, 0));
        assert!(directory.join("manual.png").is_file());
        assert!(directory.join("0123456789abcdef.hidden").is_file());
        assert!(
            directory
                .join("nested")
                .join("0123456789abcdef.png")
                .is_file()
        );
        assert!(directory.join("fedcba9876543210.png").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn cache_stats_and_cleanup_never_follow_cache_or_child_links() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("untouched.png");
        std::fs::write(&target, b"outside bytes").unwrap();
        let cache = root.path().join("cache");
        let directory = cache.join("artwork");
        std::fs::create_dir_all(&directory).unwrap();
        symlink(&target, directory.join("0123456789abcdef.png")).unwrap();
        symlink(root.path(), directory.join("fedcba9876543210.etag")).unwrap();
        assert_eq!(cache_stats(&cache).unwrap(), (0, 0));
        assert_eq!(clean_cache(&cache, true).unwrap(), 0);
        assert!(directory.join("0123456789abcdef.png").is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"outside bytes");

        let linked_cache = root.path().join("linked-cache");
        std::fs::create_dir(&linked_cache).unwrap();
        symlink(&directory, linked_cache.join("artwork")).unwrap();
        assert!(cache_stats(&linked_cache).is_err());
        assert!(clean_cache(&linked_cache, true).is_err());
    }

    fn encoded(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::new(width, height);
        let mut output = Cursor::new(Vec::new());
        image
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    #[test]
    fn thumbnails_bound_decoded_size_and_reject_oversized_images() {
        let decoded = decode_thumbnail(&encoded(800, 400), 40).unwrap();
        assert_eq!(decoded.dimensions(), (40, 20));
        assert!(decode_thumbnail(&encoded(MAX_DIMENSION + 1, 1), 40).is_none());
        assert!(decode_thumbnail(&vec![0; MAX_ENCODED_BYTES + 1], 40).is_none());
        assert!(decode_thumbnail(b"broken image", 40).is_none());
    }

    #[test]
    fn thumbnail_cache_distinguishes_libraries_sources_and_source_versions() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.wav");
        let second = root.path().join("second.wav");
        std::fs::write(&first, b"one").unwrap();
        std::fs::write(&second, b"two").unwrap();
        let key = |library: &str, source: &Path| {
            let metadata = source.metadata().unwrap();
            let file = audiolib_core::AudioFile::from(audiolib_core::AudioFileData {
                file_path: source.to_string_lossy().into(),
                file_size: metadata.len(),
                ..Default::default()
            });
            thumbnail_path(root.path(), Path::new(library), &file, "Same album")
        };
        let original = key("first.db", &first);
        assert_ne!(original, key("second.db", &first));
        assert_ne!(original, key("first.db", &second));
        std::fs::write(&first, b"replacement cover").unwrap();
        assert_ne!(original, key("first.db", &first));
    }
}
