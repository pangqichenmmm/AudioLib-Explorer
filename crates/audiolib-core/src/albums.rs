// crates/audiolib-core/src/albums.rs
//! Shared sound-library identities; a WAV is a member, never the owner of a cover.
use serde::{Deserialize, Serialize};

/// How a shared album obtains its label and membership.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlbumKind {
    Metadata,
    Folder,
}

/// Owned shared album identity within one library. Source paths are descriptive,
/// while the numeric identity survives a supported root relocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Album {
    pub id: i64,
    pub name: String,
    pub kind: AlbumKind,
    pub metadata_key: Option<String>,
    pub folder_id: Option<i64>,
    pub folder_path: Option<String>,
    pub representative_audio_id: Option<i64>,
}

/// Search-scoped member count paired with its complete, owned album identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AlbumFacet {
    pub album: Album,
    pub matching_count: u64,
}

/// Maximum bounded candidates for resolving one shared embedded cover.
pub const MAX_ALBUM_MEMBERS: usize = 64;
