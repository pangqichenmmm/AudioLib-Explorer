// server/audiolib-server/src/stream.rs
//! HTTP byte-range and conditional-request rules for `GET /audio/{id}/stream`
//! (`doc/api/README.md` §8). Pure functions over header values so they are unit-testable.
//!
//! Supported: one range per request (`bytes=a-b`, `bytes=a-`, `bytes=-n`), strong `ETag`
//! validators, `If-None-Match` (weak comparison, `*`) and `If-Range` (strong comparison).
//! Multi-range requests are answered with the full representation (200); syntactically
//! invalid or non-`bytes` ranges are ignored as RFC 9110 §14.2 permits. An `If-Range`
//! HTTP-date never matches because no `Last-Modified` is sent, so it also yields a full 200.

/// How to answer a request after evaluating `Range` and `If-Range`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteRange {
    /// Send the whole file with 200.
    Full,
    /// Send bytes `start..=end` with 206; `end` is inclusive and below the file length.
    Partial { start: u64, end: u64 },
    /// Reply 416 with `Content-Range: bytes */<len>`.
    Unsatisfiable,
}

impl ByteRange {
    /// Number of bytes sent for `len`-byte file, `None` for 416.
    pub fn body_len(self, len: u64) -> Option<u64> {
        match self {
            Self::Full => Some(len),
            Self::Partial { start, end } => Some(end - start + 1),
            Self::Unsatisfiable => None,
        }
    }
}

/// Strong entity tag for an opened file: size + modification time, plus inode and
/// status-change time on Unix and creation time on Windows. Clients treat it as an
/// opaque quoted string. Waveform cache identities additionally include the source path.
pub fn file_etag(metadata: &std::fs::Metadata) -> String {
    let len = metadata.len();
    let modified = crate::revision::mtime_ns(metadata);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let changed =
            i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec());
        format!(
            "\"{len:x}-{modified:x}-{:x}-{:x}\"",
            metadata.ino(),
            changed.max(0)
        )
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Creation time is exposed safely by std and distinguishes file replacement
        // without requiring Windows handle FFI. Waveform keys additionally scope the
        // version to the canonical source path, record id and library identity.
        format!("\"{len:x}-{modified:x}-{:x}\"", metadata.creation_time())
    }
    #[cfg(not(any(unix, windows)))]
    {
        format!("\"{len:x}-{modified:x}\"")
    }
}

/// Decide the reply for `Range` / `If-Range` header values against a `len`-byte file.
pub fn select_range(
    range: Option<&str>,
    if_range: Option<&str>,
    etag: &str,
    len: u64,
) -> ByteRange {
    let Some(range) = range else {
        return ByteRange::Full;
    };
    if let Some(validator) = if_range
        && !if_range_matches(validator, etag)
    {
        return ByteRange::Full;
    }
    parse_range(range, len)
}

/// Parse a single-range `Range` header value for a `len`-byte file.
pub fn parse_range(value: &str, len: u64) -> ByteRange {
    let Some((unit, set)) = value.trim().split_once('=') else {
        return ByteRange::Full;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return ByteRange::Full;
    }
    let set = set.trim();
    // Multi-range (multipart/byteranges) is not implemented: send the full file.
    if set.contains(',') {
        return ByteRange::Full;
    }
    let Some((first, last)) = set.split_once('-') else {
        return ByteRange::Full;
    };
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        // Suffix range: the final `last` bytes.
        let Some(suffix) = digits(last) else {
            return ByteRange::Full;
        };
        if suffix == 0 || len == 0 {
            return ByteRange::Unsatisfiable;
        }
        return ByteRange::Partial {
            start: len.saturating_sub(suffix),
            end: len - 1,
        };
    }
    let Some(start) = digits(first) else {
        return ByteRange::Full;
    };
    let end = if last.is_empty() {
        None
    } else {
        match digits(last) {
            Some(end) if end >= start => Some(end),
            _ => return ByteRange::Full,
        }
    };
    if start >= len {
        return ByteRange::Unsatisfiable;
    }
    ByteRange::Partial {
        start,
        end: end.map_or(len - 1, |end| end.min(len - 1)),
    }
}

/// `If-None-Match` evaluation: `*` or any listed tag equal under weak comparison.
pub fn none_match(header: &str, etag: &str) -> bool {
    let ours = etag.strip_prefix("W/").unwrap_or(etag);
    header
        .split(',')
        .map(str::trim)
        .any(|tag| tag == "*" || (!tag.is_empty() && tag.strip_prefix("W/").unwrap_or(tag) == ours))
}

/// `If-Range` evaluation: only an identical strong entity tag matches.
pub fn if_range_matches(header: &str, etag: &str) -> bool {
    let header = header.trim();
    header.starts_with('"') && !etag.starts_with("W/") && header == etag
}

/// Decimal digits only, saturating at `u64::MAX` so huge values stay well-defined.
fn digits(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(text.bytes().fold(0_u64, |value, b| {
        value.saturating_mul(10).saturating_add(u64::from(b - b'0'))
    }))
}

/// `Content-Type` for a served audio file, chosen by extension (WAV libraries by default).
pub fn content_type(path: &std::path::Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("wav" | "wave" | "bwf" | "rf64") => "audio/wav",
        Some("aif" | "aiff" | "aifc") => "audio/aiff",
        Some("flac") => "audio/flac",
        Some("mp3") => "audio/mpeg",
        Some("ogg" | "oga") => "audio/ogg",
        Some("m4a" | "mp4") => "audio/mp4",
        _ => "application/octet-stream",
    }
}

/// Prefer the inspected container for original bytes, retaining the extension
/// fallback for historical records without source-container Metadata.
pub fn content_type_for_record(
    path: &std::path::Path,
    record: &audiolib_core::AudioFile,
) -> &'static str {
    match record.attributes["container"].as_str() {
        Some("RIFF" | "RF64" | "BW64") => "audio/wav",
        Some("AIFF" | "AIFC") => "audio/aiff",
        Some("MP3") => "audio/mpeg",
        Some("FLAC") => "audio/flac",
        Some("Ogg") => "audio/ogg",
        _ => content_type(path),
    }
}

/// Longest ASCII fallback file name in a download `Content-Disposition`.
const MAX_FALLBACK_NAME: usize = 150;
/// Longest suffix (dot included) kept as an extension in the ASCII fallback name.
const MAX_EXTENSION: usize = 16;

/// `Content-Disposition` for `GET /audio/{id}/stream?download=1`:
/// `attachment; filename="<ASCII fallback>"; filename*=UTF-8''<RFC 5987 encoded name>`.
/// The fallback replaces everything outside printable ASCII, plus `"` `\` and the
/// characters Windows forbids in file names, with `_`; when nothing alphanumeric is left
/// of the stem it becomes `audio-<id>` (keeping an ASCII extension). Always a valid
/// header value.
pub fn attachment_disposition(filename: &str, id: i64) -> String {
    let name = if filename.is_empty() {
        format!("audio-{id}")
    } else {
        filename.to_string()
    };
    format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        ascii_fallback(&name, id),
        rfc5987_encode(&name)
    )
}

fn ascii_fallback(name: &str, id: i64) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic()
                && !matches!(c, '"' | '\\' | '/' | ':' | '*' | '?' | '<' | '>' | '|')
                || c == ' '
            {
                c
            } else {
                '_'
            }
        })
        .collect();
    let (stem, extension) = match sanitized.rfind('.') {
        Some(dot)
            if dot > 0
                && sanitized.len() - dot <= MAX_EXTENSION
                && sanitized[dot + 1..]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric()) =>
        {
            sanitized.split_at(dot)
        }
        _ => (sanitized.as_str(), ""),
    };
    let stem = if stem.chars().any(|c| c.is_ascii_alphanumeric()) {
        stem.trim().to_string()
    } else {
        format!("audio-{id}")
    };
    let keep = MAX_FALLBACK_NAME.saturating_sub(extension.len()).max(1);
    let stem: String = stem.chars().take(keep).collect();
    format!("{stem}{extension}")
}

/// RFC 5987 `value-chars`: `attr-char`s stay, every other UTF-8 byte becomes `%XX`.
fn rfc5987_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ByteRange::{Full, Partial, Unsatisfiable};

    #[test]
    fn single_ranges_clamp_and_report_unsatisfiable() {
        assert_eq!(parse_range("bytes=0-9", 100), Partial { start: 0, end: 9 });
        assert_eq!(
            parse_range("bytes=90-", 100),
            Partial { start: 90, end: 99 }
        );
        assert_eq!(
            parse_range("bytes=90-500", 100),
            Partial { start: 90, end: 99 }
        );
        assert_eq!(
            parse_range("bytes=-10", 100),
            Partial { start: 90, end: 99 }
        );
        assert_eq!(
            parse_range("bytes=-1000", 100),
            Partial { start: 0, end: 99 }
        );
        assert_eq!(
            parse_range("BYTES = 5-5 ", 100),
            Partial { start: 5, end: 5 }
        );
        assert_eq!(
            parse_range("bytes=1-99999999999999999999999", 100),
            Partial { start: 1, end: 99 }
        );
        assert_eq!(parse_range("bytes=100-", 100), Unsatisfiable);
        assert_eq!(parse_range("bytes=100-200", 100), Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 100), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=-5", 0), Unsatisfiable);
        assert_eq!(Partial { start: 90, end: 99 }.body_len(100), Some(10));
        assert_eq!(Unsatisfiable.body_len(100), None);
    }

    #[test]
    fn invalid_foreign_and_multi_ranges_fall_back_to_full() {
        for value in [
            "bytes=9-0",
            "bytes=a-b",
            "bytes=-",
            "bytes=5",
            "bytes",
            "items=0-1",
            "bytes=0-1,5-6",
            "bytes=+1-2",
        ] {
            assert_eq!(parse_range(value, 100), Full, "{value}");
        }
    }

    #[test]
    fn validators_follow_strong_and_weak_comparison() {
        let etag = "\"ff-10\"".to_string();
        assert!(none_match(&etag, &etag));
        assert!(none_match("\"x\", W/\"ff-10\"", &etag));
        assert!(none_match("*", &etag));
        assert!(!none_match("\"ff-11\"", &etag));
        assert!(if_range_matches(" \"ff-10\" ", &etag));
        assert!(!if_range_matches("W/\"ff-10\"", &etag));
        assert!(!if_range_matches("Wed, 21 Oct 2015 07:28:00 GMT", &etag));
        assert_eq!(select_range(None, Some("\"zz\""), &etag, 100), Full);
        assert_eq!(
            select_range(Some("bytes=0-1"), Some("\"zz\""), &etag, 100),
            Full
        );
        assert_eq!(
            select_range(Some("bytes=0-1"), Some(&etag), &etag, 100),
            Partial { start: 0, end: 1 }
        );
    }

    #[test]
    fn file_etags_are_strong_quoted_and_change_with_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        std::fs::write(&path, b"abc").unwrap();
        let first = file_etag(&std::fs::metadata(&path).unwrap());
        assert!(first.starts_with("\"3-") && first.ends_with('"'), "{first}");
        assert!(!first.starts_with("W/"));
        assert_eq!(first, file_etag(&std::fs::metadata(&path).unwrap()));
        std::fs::write(&path, b"abcd").unwrap();
        assert_ne!(first, file_etag(&std::fs::metadata(&path).unwrap()));
        #[cfg(unix)]
        {
            // Same size, replaced by a different inode: the tag differs.
            let other = dir.path().join("b.wav");
            std::fs::write(&other, b"abcd").unwrap();
            let before = file_etag(&std::fs::metadata(&path).unwrap());
            std::fs::rename(&other, &path).unwrap();
            assert_ne!(before, file_etag(&std::fs::metadata(&path).unwrap()));
        }
    }

    #[test]
    fn content_type_follows_extension() {
        use std::path::Path;
        assert_eq!(content_type(Path::new("/a/b.WAV")), "audio/wav");
        assert_eq!(content_type(Path::new("/a/b.flac")), "audio/flac");
        assert_eq!(content_type(Path::new("/a/b")), "application/octet-stream");
    }

    #[test]
    fn attachment_dispositions_carry_ascii_and_utf8_names() {
        assert_eq!(
            attachment_disposition("door.wav", 3),
            "attachment; filename=\"door.wav\"; filename*=UTF-8''door.wav"
        );
        assert_eq!(
            attachment_disposition("雷声 近.wav", 7),
            "attachment; filename=\"audio-7.wav\"; filename*=UTF-8''%E9%9B%B7%E5%A3%B0%20%E8%BF%91.wav"
        );
        assert_eq!(
            attachment_disposition("Boom 爆炸 01.WAV", 9),
            "attachment; filename=\"Boom __ 01.WAV\"; filename*=UTF-8''Boom%20%E7%88%86%E7%82%B8%2001.WAV"
        );
        assert_eq!(
            attachment_disposition("a\"b\\c:d;e\r\n.wav", 1),
            "attachment; filename=\"a_b_c_d;e__.wav\"; filename*=UTF-8''a%22b%5Cc%3Ad%3Be%0D%0A.wav"
        );
        assert_eq!(
            attachment_disposition("", 5),
            "attachment; filename=\"audio-5\"; filename*=UTF-8''audio-5"
        );
        let long = format!("{}.wav", "x".repeat(400));
        let header = attachment_disposition(&long, 2);
        assert!(header.contains(&format!("filename=\"{}.wav\"", "x".repeat(146))));
        // An absurdly long "extension" is not kept whole: the name stays within bounds.
        let header = attachment_disposition(&format!("a.{}", "b".repeat(300)), 4);
        let fallback = header.split('"').nth(1).unwrap();
        assert_eq!(fallback.len(), 150, "{fallback}");
        assert!(fallback.starts_with("a.bbb"));
        for name in ["\u{7f}\0\"\n", "é/ü\\.wav", "😀.flac"] {
            assert!(
                axum::http::HeaderValue::from_str(&attachment_disposition(name, 1)).is_ok(),
                "{name:?}"
            );
        }
    }
}
