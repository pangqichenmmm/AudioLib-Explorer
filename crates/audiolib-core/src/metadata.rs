// crates/audiolib-core/src/metadata.rs
//! Presentation and derived search text for owned source Metadata.
use serde_json::Value;
use std::collections::BTreeSet;

/// Preferred vendor description, then embedded description; BWF stays unaltered.
pub fn source_description<'a>(bwf: &'a str, attributes: &'a Value) -> &'a str {
    if let Some(value) = vendor_text(attributes, "description") {
        return value;
    }
    let value = &attributes["wav_metadata"]["fields"]["description"];
    value
        .as_str()
        .or_else(|| value.as_array()?.first()?.as_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(bwf)
}

/// Library/album name carried by the file's own Metadata (`fields.library`, filled from
/// Soundminer `<Library>`, ID3 `TALB` or RIFF `IPRD`). Empty means the file declares no
/// Metadata album. A shared folder fallback is represented separately by `Album`;
/// it must not overwrite this declared Metadata value.
pub fn album(attributes: &Value) -> String {
    if let Some(value) = vendor_text(attributes, "library") {
        return value.trim().to_owned();
    }
    let value = &attributes["wav_metadata"]["fields"]["library"];
    let text = match value {
        Value::String(text) => text.as_str(),
        Value::Array(items) => items.iter().filter_map(Value::as_str).next().unwrap_or(""),
        _ => "",
    };
    text.trim().to_owned()
}

fn vendor_text<'a>(attributes: &'a Value, key: &str) -> Option<&'a str> {
    let value = &attributes["vendor_metadata"]["fields"][key];
    value
        .as_str()
        .or_else(|| {
            value
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .find(|v| !v.trim().is_empty())
        })
        .filter(|value| !value.trim().is_empty())
}

/// `wav_metadata.fields` keys that feed `source_tags` and the derived search text.
pub const SOURCE_TAG_KEYS: [&str; 5] = [
    "keywords",
    "category",
    "subcategory",
    "vendor_category",
    "cat_id",
];

/// Read-only tags from controlled vendor fields, then canonical embedded fields.
/// Keywords precede category labels; empty and case-insensitive duplicate values are
/// omitted while retaining the first spelling. These values never replace library tags.
pub fn source_tags(attributes: &Value) -> Vec<String> {
    let mut tags = Vec::new();
    let mut seen = BTreeSet::new();
    for fields in [
        &attributes["vendor_metadata"]["fields"],
        &attributes["wav_metadata"]["fields"],
    ] {
        for key in SOURCE_TAG_KEYS {
            let values = match &fields[key] {
                Value::String(value) => vec![value.as_str()],
                Value::Array(values) => values.iter().filter_map(Value::as_str).collect(),
                _ => continue,
            };
            for value in values {
                // Preserve multiword tags and category names; only keyword lists have separators.
                let parts: Vec<_> = if key == "keywords" {
                    value
                        .split([',', ';', '\r', '\n', '|', '，', '；', '、'])
                        .collect()
                } else {
                    vec![value]
                };
                for tag in parts {
                    let tag = tag.trim();
                    if !tag.is_empty() && seen.insert(tag.to_lowercase()) {
                        tags.push(tag.to_owned());
                    }
                }
            }
        }
    }
    tags
}

/// Read-only list-column text: library tags followed by `source_tags` that are not already
/// present (case-insensitive). Never persisted, so `AudioFile.tags` keeps its meaning.
pub fn display_tags(user_tags: &str, attributes: &Value) -> String {
    let mut seen: BTreeSet<String> = user_tags
        .split(',')
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect();
    let mut output = user_tags.trim().to_owned();
    for tag in source_tags(attributes) {
        if seen.insert(tag.to_lowercase()) {
            if !output.is_empty() {
                output.push_str(", ");
            }
            output.push_str(&tag);
        }
    }
    output
}

/// Search public embedded text without indexing XML markup, diagnostics or user annotations.
/// The existing BWF token column carries this derived text, keeping schema v3 compatible.
pub fn source_metadata_search_text(bwf: &str, attributes: &Value) -> String {
    let mut values = BTreeSet::new();
    if !bwf.is_empty() {
        values.insert(bwf);
    }
    for group in [
        "riff_info",
        "ixml",
        "axml",
        "xmp",
        "id3",
        "id3v1",
        "smed",
        "vorbis",
        "aiff",
    ] {
        if let Some(fields) = attributes["wav_metadata"][group].as_object() {
            for (key, value) in fields {
                if group == "vorbis"
                    && matches!(
                        key.to_ascii_uppercase().as_str(),
                        "METADATA_BLOCK_PICTURE" | "COVERART" | "COVERARTMIME"
                    )
                {
                    continue;
                }
                if let Some(value) = value.as_str() {
                    values.insert(value);
                } else if let Some(values_array) = value.as_array() {
                    for value in values_array.iter().filter_map(Value::as_str) {
                        values.insert(value);
                    }
                }
            }
        }
    }
    // A separate, controlled vendor projection never rewrites embedded originals.
    if let Some(fields) = attributes["vendor_metadata"]["fields"].as_object() {
        for value in fields.values() {
            match value {
                Value::String(value) => {
                    values.insert(value.as_str());
                }
                Value::Array(items) => {
                    for value in items.iter().filter_map(Value::as_str) {
                        values.insert(value);
                    }
                }
                _ => {}
            }
        }
    }
    // Canonical extractor fields that source_tags() displays are searchable too (raw values;
    // the tokenizer splits keyword lists), so a visible extracted tag is always findable.
    let fields = &attributes["wav_metadata"]["fields"];
    for key in SOURCE_TAG_KEYS {
        match &fields[key] {
            Value::String(value) => {
                values.insert(value.as_str());
            }
            Value::Array(items) => {
                for value in items.iter().filter_map(Value::as_str) {
                    values.insert(value);
                }
            }
            _ => {}
        }
    }
    let mut output = String::new();
    // Imported extension data is untrusted too; bound tokenization work to 256 KiB.
    for value in values {
        if output.len() + value.len() + 1 > 256 * 1024 {
            continue;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(value);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{display_tags, source_tags};
    use serde_json::json;

    #[test]
    fn independent_vendor_fields_take_display_priority_without_hiding_search_sources() {
        let attributes = json!({
            "vendor_metadata":{"fields":{"description":"Vendor description","library":"Vendor album","keywords":["wind","vendor"]}},
            "wav_metadata":{"fields":{"description":"Embedded description","library":"Embedded album","keywords":"wind,embedded"},
                "vorbis":{"DESCRIPTION":"Embedded description","ARTIST":["Artist one","艺术家"],"COVERART":"binary-secret"}}
        });
        assert_eq!(
            super::source_description("BWF original", &attributes),
            "Vendor description"
        );
        assert_eq!(super::album(&attributes), "Vendor album");
        assert_eq!(
            display_tags("user, WIND", &attributes),
            "user, WIND, vendor, embedded"
        );
        let text = super::source_metadata_search_text("BWF original", &attributes);
        for value in [
            "BWF original",
            "Vendor description",
            "Vendor album",
            "Embedded description",
            "Artist one",
            "艺术家",
            "wind,embedded",
        ] {
            assert!(text.contains(value), "{value}");
        }
        assert!(!text.contains("binary-secret"));
    }

    #[test]
    fn wav_without_vendor_or_vorbis_retains_its_original_search_text_bytes() {
        let attributes = json!({"wav_metadata":{"riff_info":{"INAM":"Alpha"},"smed":{"Description":"Zulu"},
            "fields":{"keywords":"Beta","category":"Gamma","description":"not independently indexed"}}});
        assert_eq!(
            super::source_metadata_search_text("Delta", &attributes),
            "Alpha\nBeta\nDelta\nGamma\nZulu"
        );
    }

    #[test]
    fn source_tags_preserve_order_and_deduplicate_canonical_fields() {
        let attributes = json!({"wav_metadata": {"fields": {
            "keywords": [" Engine, pass by; ENGINE ", "飞机，引擎、飞机\nLow RPM|idle"],
            "category": "Vehicles",
            "subcategory": ["Aircraft", " engine "],
            "vendor_category": "Vehicles",
            "cat_id": " AERO "
        }}});
        let original = attributes.clone();
        assert_eq!(
            source_tags(&attributes),
            [
                "Engine", "pass by", "飞机", "引擎", "Low RPM", "idle", "Vehicles", "Aircraft",
                "AERO"
            ]
        );
        assert_eq!(attributes, original);
    }

    #[test]
    fn source_tags_ignore_absent_malformed_and_unrelated_fields() {
        assert!(source_tags(&json!(null)).is_empty());
        assert!(source_tags(&json!({})).is_empty());
        assert!(source_tags(&json!({"wav_metadata": {"fields": []}})).is_empty());
        let attributes = json!({
            "tags": "user annotation",
            "wav_metadata": {
                "ixml": {"unknown_vendor_tag": "raw vendor text"},
                "fields": {
                    "keywords": [null, 42, " ; ", "metal door", "METAL DOOR"],
                    "category": false,
                    "subcategory": {"nested": "ignored"},
                    "description": "Not a tag",
                    "library": "Not a category"
                }
            }
        });
        assert_eq!(source_tags(&attributes), ["metal door"]);
    }

    #[test]
    fn display_tags_append_source_tags_without_duplicates() {
        let attributes = json!({"wav_metadata": {"fields": {"keywords": "Engine; door", "category": "Aircraft"}}});
        assert_eq!(display_tags("", &attributes), "Engine, door, Aircraft");
        assert_eq!(
            display_tags("ENGINE, mine", &attributes),
            "ENGINE, mine, door, Aircraft"
        );
        assert_eq!(display_tags("mine", &json!(null)), "mine");
    }

    #[test]
    fn search_text_includes_extracted_tag_fields() {
        let attributes = json!({"wav_metadata": {"fields": {
            "keywords": "alpha; beta", "category": "Weapons", "subcategory": ["Guns"],
            "vendor_category": "VC", "cat_id": "WPNGun", "description": "ignored here"}}});
        let text = super::source_metadata_search_text("", &attributes);
        for needle in ["alpha; beta", "Weapons", "Guns", "VC", "WPNGun"] {
            assert!(text.contains(needle), "{needle} missing from {text:?}");
        }
        assert!(!text.contains("ignored here"));
    }

    #[test]
    fn aiff_and_id3v1_text_is_searchable_without_indexing_layout_or_diagnostics() {
        let attributes = json!({
            "wav_metadata": {
                "aiff": {"NAME":"aiff-title-token","ANNO":["aiff-description-token"]},
                "id3v1": {"artist":"legacy-id3-artist-token"},
                "warnings":["diagnostic-token"]
            },
            "source_audio_layout":{"untrusted":"layout-token"}
        });
        let text = super::source_metadata_search_text("", &attributes);
        for value in [
            "aiff-title-token",
            "aiff-description-token",
            "legacy-id3-artist-token",
        ] {
            assert!(text.contains(value));
        }
        assert!(!text.contains("diagnostic-token"));
        assert!(!text.contains("layout-token"));
    }
}
