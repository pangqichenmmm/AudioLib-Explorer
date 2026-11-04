// crates/audiolib-audio/src/wav_artwork.rs
//! Opt-in, bounded Soundminer pictures kept outside the searchable Metadata tree.
use super::{ArtworkPriority, EmbeddedMetadata, MAX_ARTWORK_BYTES, MAX_XML_DEPTH};
use base64::{Engine, engine::general_purpose::STANDARD};
use roxmltree::{Document, Node};

const MAX_ENCODED_ARTWORK_BYTES: usize = MAX_ARTWORK_BYTES.div_ceil(3) * 4;

impl EmbeddedMetadata {
    pub(super) fn read_smed_artwork(&mut self, document: &Document<'_>) {
        if !self.accepts_artwork(ArtworkPriority::Smed) {
            return;
        }
        for node in document.descendants().filter(Node::is_element) {
            if !["Picture", "Artwork"]
                .iter()
                .any(|name| node.tag_name().name().eq_ignore_ascii_case(name))
            {
                continue;
            }
            match picture_bytes(node) {
                Ok(Some(bytes)) => {
                    if self.set_artwork(bytes, ArtworkPriority::Smed) {
                        return;
                    }
                    self.warning("SMED picture: unsupported image magic");
                }
                Ok(None) => {}
                Err(message) => self.warning(format!("SMED picture: {message}")),
            }
        }
    }
}

fn picture_bytes(node: Node<'_, '_>) -> Result<Option<Vec<u8>>, &'static str> {
    let mut encoded = Vec::new();
    // Do not collect an unbounded concatenated String from XML/CDATA descendants.
    // XML parsing already applies the shared node/DTD limits; enforce depth for
    // each text leaf too, so a shallow Picture cannot hide a deeply nested value.
    for text in node.descendants().filter(Node::is_text) {
        if text
            .ancestors()
            .filter(Node::is_element)
            .take(MAX_XML_DEPTH + 1)
            .count()
            > MAX_XML_DEPTH
        {
            return Err("nesting exceeds 32 levels");
        }
        for byte in text.text().unwrap_or_default().bytes() {
            if byte.is_ascii_whitespace() {
                continue;
            }
            if encoded.len() == MAX_ENCODED_ARTWORK_BYTES {
                return Err("encoded size limit exceeded");
            }
            encoded.push(byte);
        }
    }
    if encoded.is_empty() {
        return Ok(None);
    }
    // Check the padded decoded length before asking the standard decoder to
    // allocate. Its conservative temporary estimate exceeds this by at most two
    // bytes; no decoded image larger than 8 MiB is accepted or retained.
    if !encoded.len().is_multiple_of(4) {
        return Err("invalid base64 length");
    }
    let padding = encoded
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    if padding > 2 {
        return Err("invalid base64 padding");
    }
    let decoded_len = (encoded.len() / 4)
        .checked_mul(3)
        .and_then(|length| length.checked_sub(padding))
        .filter(|length| *length <= MAX_ARTWORK_BYTES)
        .ok_or("decoded size limit exceeded")?;
    let bytes = STANDARD.decode(&encoded).map_err(|_| "invalid base64")?;
    if bytes.len() != decoded_len || bytes.len() > MAX_ARTWORK_BYTES {
        return Err("decoded size limit exceeded");
    }
    Ok(Some(bytes))
}
