// crates/audiolib-core/src/audio_file.rs
//! Owned presentation snapshots with copy-on-write mutation of the whole record.
//!
//! AudioFileData is the existing AudioFile field declaration, renamed without
//! changing any field, type, serde attribute, or Metadata authority. This wrapper
//! is a public Rust constructor/field-move change; JSON remains the bare record.
use crate::AudioFileData;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    fmt,
    ops::{Deref, DerefMut},
    sync::Arc,
};

/// One owned audio record. Clone shares the immutable snapshot; the first mutable
/// field access detaches all fields together, preserving other owned snapshots.
#[derive(Clone)]
pub struct AudioFile(Arc<AudioFileData>);

impl AudioFile {
    /// Consume the handle for callers which move owned fields out of the record.
    /// Unique data is moved out; shared data is cloned before ownership transfers.
    pub fn into_data(self) -> AudioFileData {
        Arc::try_unwrap(self.0).unwrap_or_else(|shared| shared.as_ref().clone())
    }
}

impl Default for AudioFile {
    fn default() -> Self {
        AudioFileData::default().into()
    }
}

impl From<AudioFileData> for AudioFile {
    fn from(data: AudioFileData) -> Self {
        Self(Arc::new(data))
    }
}

impl AsRef<AudioFileData> for AudioFile {
    fn as_ref(&self) -> &AudioFileData {
        self.0.as_ref()
    }
}

impl Deref for AudioFile {
    type Target = AudioFileData;

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl DerefMut for AudioFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.0)
    }
}

impl Serialize for AudioFile {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_ref().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AudioFile {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        AudioFileData::deserialize(deserializer).map(Self::from)
    }
}

impl fmt::Debug for AudioFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_ref(), formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Album, AlbumKind, SearchPage};
    use serde_json::{Value, json};

    fn all_fields() -> AudioFileData {
        AudioFileData {
            id: 17,
            file_path: "C:/owned/音效/Vehicle 17.flac".into(),
            filename: "Vehicle 17.flac".into(),
            folder_id: Some(3),
            file_size: u64::MAX,
            sample_rate: 96_000,
            bit_depth: 24,
            channels: 6,
            duration: 1.25,
            tags: "用户标签,vehicle".into(),
            color: Some("#123ABC".into()),
            notes: "retained notes".into(),
            favorite: true,
            bext_description: "原始描述".into(),
            attributes: json!({
                "wav_metadata": {"vorbis": {"COMMENT": ["原文", "duplicate"]}},
                "vendor_metadata": {"fields": {"description": ["supplier"]}},
                "unknown": {"array": [null, {"label": "retained", "flags": [true]}]}
            }),
            album: "共享专辑".into(),
            album_id: Some(9),
            album_entity: Some(Album {
                id: 9,
                name: "共享专辑".into(),
                kind: AlbumKind::Metadata,
                metadata_key: Some("共享专辑".into()),
                folder_id: None,
                folder_path: None,
                representative_audio_id: Some(17),
            }),
            missing: true,
        }
    }

    #[test]
    fn every_field_and_nested_metadata_have_the_same_bare_record_wire_shape() {
        let data = all_fields();
        let expected = serde_json::to_value(&data).unwrap();
        let expected_bytes = serde_json::to_vec(&data).unwrap();
        let file = AudioFile::from(data);
        assert_eq!(serde_json::to_value(&file).unwrap(), expected);
        assert_eq!(serde_json::to_vec(&file).unwrap(), expected_bytes);
        let decoded: AudioFile = serde_json::from_slice(&expected_bytes).unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), expected);
        let page = SearchPage {
            results: vec![file],
            total_count: 1,
            returned_count: 1,
            ..Default::default()
        };
        let page_wire = serde_json::to_value(&page).unwrap();
        assert_eq!(page_wire["results"][0], expected);
        let decoded_page: SearchPage = serde_json::from_value(page_wire).unwrap();
        assert_eq!(
            serde_json::to_value(&decoded_page.results[0]).unwrap(),
            expected
        );
    }

    #[test]
    fn file_and_page_clones_isolate_plain_nested_path_and_album_mutations() {
        let file = AudioFile::from(all_fields());
        let expected = serde_json::to_value(&file).unwrap();
        let mut draft = file.clone();
        draft.filename = "edited.wav".into();
        draft.file_path = "C:/owned/edited.wav".into();
        draft.tags.push_str(",newtag");
        draft.notes = "new note".into();
        assert_eq!(serde_json::to_value(&file).unwrap(), expected);
        assert_eq!(draft.filename, "edited.wav");
        let cached = SearchPage {
            results: vec![file.clone()],
            ..Default::default()
        };
        let mut response = cached.clone();
        response.results[0].attributes["unknown"]["array"][1]["label"] = json!("response edit");
        response.results[0].attributes["unknown"]["array"][1]["flags"]
            .as_array_mut()
            .unwrap()
            .push(json!(false));
        response.results[0].attributes["wav_metadata"]["vorbis"]["COMMENT"][0] = json!("changed");
        response.results[0].album_entity.as_mut().unwrap().name = "new album label".into();
        response.results[0].file_path = "C:/owned/response.flac".into();
        response.results[0].notes.push_str(" response edit");
        response.results[0].favorite = false;
        response.results[0].missing = false;
        assert_eq!(serde_json::to_value(&cached.results[0]).unwrap(), expected);
        assert_eq!(serde_json::to_value(&file).unwrap(), expected);
        assert_eq!(
            response.results[0].attributes["unknown"]["array"][1]["flags"],
            json!([true, false])
        );
        assert_eq!(
            response.results[0].attributes["vendor_metadata"],
            expected["attributes"]["vendor_metadata"]
        );
        assert_eq!(draft.attributes, expected["attributes"]);
    }

    #[test]
    fn generic_metadata_json_shapes_are_preserved_without_an_attributes_wrapper() {
        for attributes in [
            Value::Null,
            json!(true),
            json!(-123),
            json!(u64::MAX),
            json!(1.25),
            json!("中文 \u{0} unknown"),
            json!([1, null, {"unknown": false}]),
            json!({"unknown": {"empty": {}, "nested": [null, true]}}),
        ] {
            let mut data = all_fields();
            data.attributes = attributes.clone();
            let expected_bytes = serde_json::to_vec(&data).unwrap();
            let file = AudioFile::from(data);
            assert_eq!(file.attributes, attributes);
            assert_eq!(serde_json::to_vec(&file).unwrap(), expected_bytes);
            let decoded: AudioFile = serde_json::from_slice(&expected_bytes).unwrap();
            assert_eq!(decoded.attributes, attributes);
        }
    }

    #[test]
    fn legacy_optional_fields_and_missing_required_metadata_match_bare_deserialization() {
        let mut legacy = serde_json::to_value(all_fields()).unwrap();
        for optional in ["album", "album_id", "album_entity", "missing"] {
            legacy.as_object_mut().unwrap().remove(optional);
        }
        let direct: AudioFileData = serde_json::from_value(legacy.clone()).unwrap();
        let file: AudioFile = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&file).unwrap(),
            serde_json::to_value(direct).unwrap()
        );
        assert!(file.album.is_empty());
        assert!(file.album_id.is_none());
        assert!(file.album_entity.is_none());
        assert!(!file.missing);
        legacy.as_object_mut().unwrap().remove("attributes");
        assert!(serde_json::from_value::<AudioFileData>(legacy.clone()).is_err());
        assert!(serde_json::from_value::<AudioFile>(legacy).is_err());
        // Required fields, implicit Option defaults and explicit serde defaults
        // must behave exactly like the unchanged bare data declaration.
        let full = serde_json::to_value(all_fields()).unwrap();
        for field in full.as_object().unwrap().keys() {
            let mut missing = full.clone();
            missing.as_object_mut().unwrap().remove(field);
            let direct = serde_json::from_value::<AudioFileData>(missing.clone());
            let wrapped = serde_json::from_value::<AudioFile>(missing);
            match (direct, wrapped) {
                (Ok(direct), Ok(wrapped)) => assert_eq!(
                    serde_json::to_value(direct).unwrap(),
                    serde_json::to_value(wrapped).unwrap(),
                    "Missing field {field} changed default values"
                ),
                (Err(_), Err(_)) => {}
                _ => panic!("Missing field {field} changed deserialization acceptance"),
            }
        }
    }

    #[test]
    fn consuming_unique_or_shared_records_preserves_independent_owned_field_moves() {
        let data = all_fields();
        let expected = serde_json::to_value(&data).unwrap();
        let unique = AudioFile::from(data).into_data();
        assert_eq!(serde_json::to_value(&unique).unwrap(), expected);
        let file = AudioFile::from(all_fields());
        let remaining = file.clone();
        let mut detached = file.into_data();
        detached.filename = "consumed.wav".into();
        detached.attributes["unknown"]["array"][1]["label"] = json!("consumed edit");
        assert_eq!(serde_json::to_value(&remaining).unwrap(), expected);
        let moved_path = detached.file_path;
        let moved_metadata = detached.attributes;
        assert_eq!(moved_path, "C:/owned/音效/Vehicle 17.flac");
        assert_eq!(
            moved_metadata["unknown"]["array"][1]["label"],
            "consumed edit"
        );
    }

    #[test]
    fn defaults_keep_null_metadata_and_detach_on_first_mutable_access() {
        let original = AudioFile::default();
        let expected = serde_json::to_value(AudioFileData::default()).unwrap();
        let mut changed = original.clone();
        changed.filename = "new.wav".into();
        changed.attributes["new_unknown"] = json!([null, 1]);
        assert_eq!(serde_json::to_value(&original).unwrap(), expected);
        assert!(original.attributes.is_null());
        assert_eq!(changed.attributes["new_unknown"], json!([null, 1]));
    }
}
