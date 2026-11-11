// crates/audiolib-update/src/package.rs
//! Archive readers enforce public package boundaries before writing any entry.
use crate::Product;
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashSet,
    io::Read,
    path::{Component, Path},
};

const MAX_EXPANDED: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;
const PORTABLE_FILES: &str = include_str!("../../../tools/portable-files.json");
const SERVER_FILES: &str = include_str!("../../../tools/server-files.json");

pub(crate) fn parse_checksum(bytes: &[u8], expected_name: &str) -> Result<String> {
    let text = std::str::from_utf8(bytes)?;
    let parts: Vec<_> = text.split_whitespace().collect();
    ensure!(
        parts.len() == 2
            && parts[0].len() == 64
            && parts[0].bytes().all(|byte| byte.is_ascii_hexdigit())
            && parts[1] == expected_name,
        "Invalid release SHA256 file"
    );
    Ok(parts[0].to_ascii_lowercase())
}

pub(crate) fn safe_relative(text: &str) -> Result<&Path> {
    ensure!(
        !text.is_empty()
            && !text.contains(['\\', ':', '\0', '?', '*', '<', '>', '|', '"'])
            && !text.chars().any(char::is_control),
        "Unsafe update path"
    );
    ensure!(
        text.split('/').all(|part| !part.is_empty()
            && part != "."
            && part != ".."
            && !part.ends_with(['.', ' '])
            && !reserved_component(part)),
        "Unsafe update path component"
    );
    let path = Path::new(text);
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "Unsafe update path"
    );
    Ok(path)
}

fn reserved_component(part: &str) -> bool {
    let stem = part
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|number| {
            matches!(
                number,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

pub(crate) fn allowed(product: Product, text: &str) -> bool {
    if safe_relative(text).is_err() {
        return false;
    }
    if matches!(text, "update-channel.txt" | "update-version.txt") {
        return true;
    }
    let allowed_files: Vec<String> = serde_json::from_str(match product {
        Product::Desktop => PORTABLE_FILES,
        Product::Server => SERVER_FILES,
    })
    .unwrap_or_default();
    if allowed_files.iter().any(|file| file == text) {
        return true;
    }
    if text.starts_with("licenses/") {
        let lower = text.to_ascii_lowercase();
        return ![
            ".exe",
            ".dll",
            ".cmd",
            ".bat",
            ".ps1",
            ".sh",
            ".db",
            ".sqlite",
            ".wav",
            ".log",
            ".sqlite3",
            ".db-wal",
            ".db-shm",
            ".sqlite-wal",
            ".sqlite-shm",
            ".sqlite3-wal",
            ".sqlite3-shm",
        ]
        .iter()
        .any(|extension| lower.ends_with(extension))
            && !lower.ends_with("settings.json")
            && !matches!(lower.rsplit('/').next(), Some("tokens.json" | ".env"))
            && !lower.ends_with(".server-state.json");
    }
    match product {
        Product::Desktop => matches!(
            text,
            "AudioLibExplorer.exe" | "AudioLibCli.exe" | "Start-AudioLibExplorer.cmd"
        ),
        Product::Server => {
            text == if cfg!(windows) {
                "audiolib-server.exe"
            } else {
                "audiolib-server"
            }
        }
    }
}

/// Required files for a current automatic-install payload. Server layout 2
/// always installs the matching platform migrator alongside the server. Older
/// asset names remain discoverable, but historical layout 1 archives require
/// manual installation; missing tools never silently select an older layout.
pub(crate) fn required(product: Product) -> &'static [&'static str] {
    match product {
        Product::Desktop => &[
            "AudioLibExplorer.exe",
            "AudioLibCli.exe",
            "Start-AudioLibExplorer.cmd",
            "LICENSE",
            "THIRD_PARTY.md",
        ],
        Product::Server if cfg!(windows) => &[
            "audiolib-server.exe",
            "audiolib-migrate.exe",
            "LICENSE",
            "THIRD_PARTY.md",
        ],
        Product::Server => &[
            "audiolib-server",
            "audiolib-migrate",
            "LICENSE",
            "THIRD_PARTY.md",
        ],
    }
}

fn copy_entry(
    reader: impl Read,
    payload: &Path,
    name: &str,
    size: u64,
    executable: bool,
) -> Result<()> {
    let path = payload.join(safe_relative(name)?);
    std::fs::create_dir_all(path.parent().context("Entry requires parent")?)?;
    let mut output = std::fs::File::create_new(&path)?;
    let copied = std::io::copy(&mut reader.take(size + 1), &mut output)?;
    ensure!(copied == size, "Archive entry size mismatch");
    output.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
        )?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

pub(crate) fn extract(archive: &Path, payload: &Path, product: Product, name: &str) -> Result<()> {
    let mut seen = HashSet::new();
    let mut total: u64 = 0;
    if name.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?)?;
        ensure!(zip.len() <= MAX_ENTRIES, "Too many archive entries");
        for index in 0..zip.len() {
            let entry = zip.by_index(index)?;
            let name = entry.name().to_owned();
            ensure!(
                entry
                    .unix_mode()
                    .is_none_or(|mode| matches!(mode & 0o170000, 0 | 0o100000 | 0o040000)),
                "Linked or special ZIP entry"
            );
            if entry.is_dir() {
                safe_relative(name.trim_end_matches('/'))?;
                continue;
            }
            ensure!(
                entry
                    .unix_mode()
                    .is_none_or(|mode| mode & 0o170000 == 0 || mode & 0o170000 == 0o100000),
                "Linked or special ZIP entry"
            );
            ensure!(allowed(product, &name), "Non-public update file: {name}");
            ensure!(
                seen.insert(name.to_ascii_lowercase()),
                "Duplicate update file"
            );
            let size = entry.size();
            total = total.checked_add(size).context("Archive size overflow")?;
            ensure!(
                total <= MAX_EXPANDED,
                "Expanded update exceeds its size limit"
            );
            copy_entry(entry, payload, &name, size, false)?;
        }
    } else {
        let decoder = flate2::read::GzDecoder::new(std::fs::File::open(archive)?);
        let mut tar = tar::Archive::new(decoder);
        let mut extended_path = None;
        // Raw mode lets us cap metadata headers before the tar crate allocates
        // GNU/PAX long-path buffers. Release packages use ordinary PAX metadata.
        for (index, entry) in tar.entries()?.raw(true).enumerate() {
            let mut entry = entry?;
            ensure!(index < MAX_ENTRIES, "Too many archive entries");
            let kind = entry.header().entry_type();
            let size = entry.size();
            total = total.checked_add(size).context("Archive size overflow")?;
            ensure!(
                total <= MAX_EXPANDED,
                "Expanded update exceeds its size limit"
            );
            if kind.is_pax_local_extensions() || kind.is_pax_global_extensions() {
                ensure!(size <= 64 * 1024, "Oversized TAR metadata");
                if let Some(extensions) = entry.pax_extensions()? {
                    for extension in extensions {
                        let extension = extension?;
                        match extension.key()? {
                            "path" => {
                                ensure!(
                                    kind.is_pax_local_extensions() && extended_path.is_none(),
                                    "Ambiguous TAR extended path"
                                );
                                extended_path = Some(extension.value()?.to_owned());
                            }
                            "linkpath"
                            | "size"
                            | "GNU.sparse.map"
                            | "GNU.sparse.size"
                            | "GNU.sparse.realsize" => anyhow::bail!("Unsupported TAR metadata"),
                            _ => {}
                        }
                    }
                }
                continue;
            }
            if kind.is_gnu_longname() {
                ensure!(
                    size <= 4096 && extended_path.is_none(),
                    "Oversized or ambiguous TAR long path"
                );
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                if bytes.last() == Some(&0) {
                    bytes.pop();
                }
                extended_path = Some(String::from_utf8(bytes)?);
                continue;
            }
            let name = if let Some(name) = extended_path.take() {
                name
            } else {
                std::str::from_utf8(&entry.path_bytes())
                    .context("Non-UTF8 archive path")?
                    .to_owned()
            };
            let name = name.strip_prefix("./").unwrap_or(&name).to_owned();
            if entry.header().entry_type().is_dir() {
                ensure!(size == 0, "TAR directory has a data payload");
                if !name.is_empty() {
                    safe_relative(name.trim_end_matches('/'))?;
                }
                continue;
            }
            ensure!(
                entry.header().entry_type().is_file(),
                "Linked or special TAR entry"
            );
            ensure!(allowed(product, &name), "Non-public update file: {name}");
            ensure!(
                seen.insert(name.to_ascii_lowercase()),
                "Duplicate update file"
            );
            let executable = matches!(
                name.as_str(),
                "audiolib-server" | "audiolib-migrate" | "server/deploy/redeploy-server.sh"
            );
            copy_entry(entry, payload, &name, size, executable)?;
        }
        ensure!(extended_path.is_none(), "Dangling TAR extended path");
    }
    for name in required(product) {
        ensure!(
            payload.join(name).is_file(),
            "Required update file is missing: {name}"
        );
    }
    Ok(())
}
