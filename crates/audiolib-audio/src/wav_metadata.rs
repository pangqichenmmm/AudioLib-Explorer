// crates/audiolib-audio/src/wav_metadata.rs
//! Bounded, read-only extraction of public WAV Metadata containers.
use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, WINDOWS_1252};
use roxmltree::{Document, Node, ParsingOptions};
use serde_json::{Map, Value, json};

/// Increment when extraction changes so an ordinary scan refreshes old records.
pub const WAV_METADATA_VERSION: i64 = 4;
pub(crate) const MAX_CHUNK_BYTES: u64 = 2 * 1024 * 1024;
// Modern SMED includes UTF-16 base64 artwork (the supplied BOOM files use ~4 MiB).
pub(crate) const MAX_SMED_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_METADATA_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FIELDS: usize = 16_384;
const MAX_XML_DEPTH: usize = 32;
const MAX_FIELD_PATH_BYTES: usize = 4096;
/// Bounded decoded cover payload; larger pictures are ignored rather than retained.
pub(crate) const MAX_ARTWORK_BYTES: usize = 8 * 1024 * 1024;
// An ID3 tag can contain one bounded image plus text/frame overhead. Ordinary
// Metadata extraction still skips binary frames and caps copied text separately.
pub(crate) const MAX_ID3_BYTES: u64 = MAX_ARTWORK_BYTES as u64 + MAX_CHUNK_BYTES;

/// Cover artwork embedded in the WAV itself (ID3 or SMED), kept separate from Metadata JSON
/// so a multi-megabyte image never reaches SQLite or FTS5.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artwork {
    /// Original JPEG/PNG bytes, never re-encoded by the extractor.
    pub bytes: Vec<u8>,
    /// MIME type detected from the image magic, never trusted from a tag declaration.
    pub mime: String,
}

pub(crate) fn text(bytes: &[u8]) -> String {
    let bytes = bytes.split(|b| *b == 0).next().unwrap_or_default();
    let decoded = match std::str::from_utf8(bytes) {
        Ok(value) => value.into(),
        Err(_) => WINDOWS_1252.decode_without_bom_handling(bytes).0,
    };
    decoded.trim().to_string()
}

fn xml_text(bytes: &[u8]) -> String {
    let encoding = Encoding::for_bom(bytes)
        .map(|(encoding, _)| encoding)
        .or_else(|| {
            if bytes.starts_with(b"<\0") {
                Some(UTF_16LE)
            } else if bytes.starts_with(b"\0<") {
                Some(UTF_16BE)
            } else {
                // The declaration is ASCII even when the following XML text uses a
                // legacy encoding. Do not require that source text to be UTF-8.
                bytes.strip_prefix(b"<?xml")?;
                let end = bytes.windows(2).position(|pair| pair == b"?>")?;
                let declaration = std::str::from_utf8(&bytes[..end]).ok()?;
                let label = declaration
                    .split_once("encoding")?
                    .1
                    .trim_start()
                    .strip_prefix('=')?
                    .trim_start();
                let quote = label.chars().next()?;
                let label = label.strip_prefix(quote)?.split(quote).next()?;
                Encoding::for_label(label.as_bytes())
            }
        });
    let decoded = match encoding {
        Some(encoding) => encoding.decode(bytes).0,
        None => String::from_utf8_lossy(bytes),
    };
    decoded.trim_matches(['\0', '\u{feff}']).trim().to_string()
}

fn append(map: &mut Map<String, Value>, key: &str, value: &str) {
    let value = value.trim_matches(['\0', '\u{feff}']).trim();
    if value.is_empty() || map.len() >= MAX_FIELDS {
        return;
    }
    match map.get_mut(key) {
        None => {
            map.insert(key.into(), json!(value));
        }
        Some(Value::Array(values)) => {
            if values.len() < MAX_FIELDS && !values.iter().any(|v| v.as_str() == Some(value)) {
                values.push(json!(value));
            }
        }
        Some(previous) if previous.as_str() != Some(value) => {
            *previous = json!([previous.clone(), value]);
        }
        _ => {}
    }
}

#[derive(Default)]
pub(crate) struct EmbeddedMetadata {
    groups: Map<String, Value>,
    chunks: Vec<Value>,
    warnings: Vec<String>,
    artwork: Option<Artwork>,
    artwork_priority: Option<ArtworkPriority>,
}

// A valid ID3 picture overrides SMED regardless of RIFF chunk order. Within ID3,
// prefer an explicitly tagged front cover, retaining the first equal-priority image.
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum ArtworkPriority {
    Smed,
    Id3,
    Id3Front,
}

impl EmbeddedMetadata {
    fn accepts_artwork(&self, priority: ArtworkPriority) -> bool {
        self.artwork_priority
            .is_none_or(|previous| previous < priority)
    }

    /// Retain a bounded JPEG/PNG only when it improves the source/type priority.
    fn set_artwork(&mut self, bytes: Vec<u8>, priority: ArtworkPriority) -> bool {
        if !self.accepts_artwork(priority) || bytes.is_empty() || bytes.len() > MAX_ARTWORK_BYTES {
            return false;
        }
        let Some(mime) = image_mime(&bytes) else {
            return false;
        };
        self.artwork = Some(Artwork {
            bytes,
            mime: mime.to_owned(),
        });
        self.artwork_priority = Some(priority);
        true
    }

    pub(crate) fn take_artwork(&mut self) -> Option<Artwork> {
        self.artwork.take()
    }
}

/// The image consumers support bounded JPEG/PNG; a MIME label alone is insufficient.
fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else {
        None
    }
}

impl EmbeddedMetadata {
    pub(crate) fn smed(
        &mut self,
        bytes: &[u8],
        attrs: &mut Map<String, Value>,
        include_artwork: bool,
    ) {
        use blowfish::cipher::{Block, BlockDecrypt, KeyInit};
        if bytes.len() < 12 || !(bytes.len() - 4).is_multiple_of(8) {
            self.warning("SMED: unsupported block layout");
            return;
        }
        let size = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        if size == 0 || size > bytes.len() - 4 || bytes.len() - 4 - size >= 8 {
            self.warning("SMED: invalid decoded length");
            return;
        }
        // Legacy Soundminer compatibility only, never used to protect application data.
        // Public format analysis: https://reverseengineering.stackexchange.com/q/26241
        let cipher: blowfish::Blowfish =
            blowfish::Blowfish::new_from_slice(b"u7w58he4746").unwrap();
        let mut xml = bytes[4..].to_vec();
        for bytes in xml.chunks_exact_mut(8) {
            let mut block = Block::<blowfish::Blowfish>::default();
            block.copy_from_slice(bytes);
            cipher.decrypt_block(&mut block);
            bytes.copy_from_slice(&block);
        }
        xml.truncate(size);
        self.xml_with_artwork("smed", &xml, attrs, include_artwork);
    }

    pub(crate) fn chunk(&mut self, id: &[u8], size: u64) {
        if self.chunks.len() < 256 {
            self.chunks
                .push(json!({"id": String::from_utf8_lossy(id), "bytes": size}));
        }
    }

    pub(crate) fn warning(&mut self, message: impl Into<String>) {
        let message = message.into();
        if self.warnings.len() < 32 && !self.warnings.contains(&message) {
            self.warnings.push(message);
        }
    }

    fn merge(&mut self, name: &str, values: Map<String, Value>) {
        let group = self
            .groups
            .entry(name)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .unwrap();
        for (key, value) in values {
            match value {
                Value::String(value) => append(group, &key, &value),
                Value::Array(values) => {
                    for value in values {
                        if let Some(value) = value.as_str() {
                            append(group, &key, value);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn info(&mut self, bytes: &[u8]) {
        if !bytes.starts_with(b"INFO") {
            return;
        }
        let mut fields = Map::new();
        let mut rest = &bytes[4..];
        for _ in 0..MAX_FIELDS {
            if rest.is_empty() {
                break;
            }
            if rest.len() < 8 {
                self.warning("LIST/INFO: truncated field header");
                break;
            }
            let size = u32::from_le_bytes(rest[4..8].try_into().unwrap()) as usize;
            let Some(data) = rest.get(8..8 + size) else {
                self.warning("LIST/INFO: truncated field value");
                break;
            };
            append(
                &mut fields,
                &String::from_utf8_lossy(&rest[..4]),
                &text(data),
            );
            rest = rest.get(8 + size + (size & 1)..).unwrap_or_default();
        }
        self.merge("riff_info", fields);
    }

    pub(crate) fn xml(&mut self, group: &str, bytes: &[u8], attrs: &mut Map<String, Value>) {
        self.xml_with_artwork(group, bytes, attrs, false);
    }

    fn xml_with_artwork(
        &mut self,
        group: &str,
        bytes: &[u8],
        attrs: &mut Map<String, Value>,
        include_artwork: bool,
    ) {
        let xml = xml_text(bytes);
        if group == "ixml" {
            attrs.insert("ixml_raw".into(), json!(xml));
        }
        let document = match Document::parse_with_options(
            &xml,
            ParsingOptions {
                allow_dtd: false,
                nodes_limit: MAX_FIELDS as u32,
                ..Default::default()
            },
        ) {
            Ok(document) => document,
            Err(error) => {
                self.warning(format!("{group}: {error}"));
                return;
            }
        };
        // Reuse the decrypted text and bounded DOM. Artwork never enters fields,
        // ixml_raw, or the resulting Metadata JSON, and scans skip this work.
        if group == "smed" && include_artwork {
            self.read_smed_artwork(&document);
        }
        let mut fields = Map::new();
        for node in document.descendants().filter(Node::is_element) {
            // Apply the depth bound before any work that walks ancestor chains.
            let ancestors: Vec<_> = node
                .ancestors()
                .filter(Node::is_element)
                .take(MAX_XML_DEPTH + 1)
                .collect();
            if ancestors.len() > MAX_XML_DEPTH {
                self.warning(format!("{group}: nesting exceeds 32 levels"));
                continue;
            }
            // Namespace URIs can be much longer than tag names. Check each part
            // before copying it so a shared large URI cannot inflate every path.
            let path = ancestors
                .iter()
                .rev()
                .try_fold(String::new(), |mut path, n| {
                    if !path.is_empty() {
                        path.push('/');
                    }
                    append_xml_name(&mut path, n.tag_name().namespace(), n.tag_name().name())
                        .then_some(path)
                });
            let Some(path) = path else {
                self.warning(format!("{group}: field path exceeds 4096 bytes"));
                continue;
            };
            // SMED embeds base64 artwork/caches alongside searchable library fields.
            // Do not duplicate those binary blobs into SQLite or FTS5.
            if group == "smed"
                && ancestors.iter().any(|parent| {
                    parent
                        .attribute("type")
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("NSData"))
                        || [
                            "picture",
                            "artwork",
                            "waveform",
                            "waveformdata",
                            "waveformcache",
                        ]
                        .iter()
                        .any(|name| parent.tag_name().name().eq_ignore_ascii_case(name))
                })
            {
                continue;
            }
            let value: String = node
                .children()
                .filter(Node::is_text)
                .filter_map(|n| n.text())
                .collect();
            append(&mut fields, &path, &value);
            for attribute in node.attributes() {
                let mut key = path.clone();
                key.push_str("/@");
                if append_xml_name(&mut key, attribute.namespace(), attribute.name()) {
                    append(&mut fields, &key, attribute.value());
                } else {
                    self.warning(format!("{group}: field path exceeds 4096 bytes"));
                }
            }
        }
        if group == "ixml" {
            // Read only direct BWFXML children; track/user tags with the same names are unrelated.
            for node in document.root_element().children().filter(Node::is_element) {
                let value: String = node
                    .children()
                    .filter(Node::is_text)
                    .filter_map(|n| n.text())
                    .collect();
                let value = value.trim();
                let key = match node.tag_name().name() {
                    "SCENE" => "ixml_scene",
                    "TAKE" => "ixml_take",
                    "TAPE" => "ixml_tape",
                    "CIRCLED" => "ixml_circled",
                    "WILD_TRACK" => "ixml_wild_track",
                    "NO_GOOD" => "ixml_no_good",
                    _ => continue,
                };
                if ["ixml_scene", "ixml_take", "ixml_tape"].contains(&key) {
                    attrs.insert(key.into(), json!(value));
                } else if !value.is_empty() {
                    attrs.insert(
                        key.into(),
                        json!(matches!(
                            value.to_ascii_uppercase().as_str(),
                            "TRUE" | "1" | "YES"
                        )),
                    );
                }
            }
        }
        self.merge(group, fields);
    }

    pub(crate) fn finish(mut self, attrs: &mut Map<String, Value>) {
        let mut fields = Map::new();
        // Rich description sources precede the fixed 256-byte BWF description.
        // Keep every original value under its source path even when summaries overlap.
        for group in ["smed", "ixml", "xmp", "axml", "riff_info", "id3"] {
            if let Some(values) = self.groups.get(group).and_then(Value::as_object) {
                let mut values: Vec<_> = values.iter().collect();
                // Copyright comments/notes must not hide the actual description, and
                // SMED's full Description must precede its truncated BWDescription copy.
                values.sort_by_key(|(path, _)| match field_name(path).as_str() {
                    "bwdescription" => 1,
                    "note" | "comment" | "comments" => 2,
                    _ => 0,
                });
                for (path, value) in values {
                    if let Some(key) = canonical_field(group, path) {
                        match value {
                            Value::String(value) => append(&mut fields, key, value),
                            Value::Array(values) => {
                                for value in values {
                                    if let Some(value) = value.as_str() {
                                        append(&mut fields, key, value);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        if !fields.contains_key("description")
            && let Some(value) = attrs.get("bwf_description").and_then(Value::as_str)
        {
            append(&mut fields, "description", value);
        }
        self.groups.insert("fields".into(), Value::Object(fields));
        self.groups
            .insert("version".into(), json!(WAV_METADATA_VERSION));
        self.groups.insert("chunks".into(), json!(self.chunks));
        if !self.warnings.is_empty() {
            self.groups.insert("warnings".into(), json!(self.warnings));
        }
        attrs.insert("wav_metadata".into(), Value::Object(self.groups));
    }
}

fn append_xml_name(path: &mut String, namespace: Option<&str>, name: &str) -> bool {
    // Expanded names preserve unknown vendor fields and prevent prefix collisions.
    let namespace_bytes = namespace.map_or(0, |namespace| namespace.len() + 2);
    if path.len() + namespace_bytes + name.len() > MAX_FIELD_PATH_BYTES {
        return false;
    }
    if let Some(namespace) = namespace {
        path.push('{');
        path.push_str(namespace);
        path.push('}');
    }
    path.push_str(name);
    true
}

fn field_name(path: &str) -> String {
    // Namespace URIs may contain '/'. Strip expanded namespaces before splitting paths.
    let mut plain = String::new();
    let mut namespace = false;
    for ch in path.chars() {
        match ch {
            '{' => namespace = true,
            '}' => namespace = false,
            _ if !namespace => plain.push(ch),
            _ => {}
        }
    }
    let name = plain
        .split('/')
        .rev()
        .find(|name| !["li", "Bag", "Seq", "Alt"].contains(name))
        .unwrap_or_default()
        .trim_start_matches('@');
    name.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn canonical_field(group: &str, path: &str) -> Option<&'static str> {
    if group == "id3" && (path.starts_with("COMM/") || path.starts_with("COM/")) {
        return Some("description");
    }
    let normalized = field_name(path);
    Some(match (group, normalized.as_str()) {
        ("riff_info", "icmt" | "isbj") | ("id3", "comm" | "com") => "description",
        ("riff_info", "inam") | ("id3", "tit2" | "tt2") => "title",
        ("riff_info", "ikey") => "keywords",
        ("riff_info", "iart") | ("id3", "tpe1" | "tp1") => "artist",
        ("riff_info", "iprd") | ("id3", "talb" | "tal") => "library",
        ("riff_info", "ignr") | ("id3", "tcon" | "tco") => "category",
        ("riff_info", "icop") | ("id3", "tcop" | "tcr") => "copyright",
        (_, "description" | "bwdescription" | "note" | "comment" | "comments") => "description",
        (_, "title" | "tracktitle" | "fxname") => "title",
        (_, "category" | "genre") => "category",
        (_, "subcategory") => "subcategory",
        (_, "vendorcategory") => "vendor_category",
        (_, "catid" | "categoryid") => "cat_id",
        (_, "keywords" | "subject" | "tags") => "keywords",
        (_, "library" | "cdtitle" | "album") => "library",
        (_, "designer" | "sounddesigner") => "designer",
        (_, "artist" | "creator") => "artist",
        (_, "manufacturer") => "manufacturer",
        (_, "publisher" | "musicpublisher") => "publisher",
        (_, "copyright" | "rights") => "copyright",
        (_, "microphone" | "micmodel") => "microphone",
        (_, "location") => "location",
        _ => return None,
    })
}

#[path = "wav_id3.rs"]
mod id3;

#[path = "wav_artwork.rs"]
mod artwork;

#[cfg(test)]
#[path = "wav_metadata_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "wav_artwork_tests.rs"]
mod artwork_tests;
