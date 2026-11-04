// crates/audiolib-db/src/migration/recipe.rs
//! Compiled schema evolution recipes. User input never becomes executable DDL.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Reviewable structure operation; only an exact compiled recipe may be executed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SchemaChange {
    /// Create the compiled shared-album tables and persistent identity allocator.
    CreateAlbumEntities,
    /// Add a field with its explicit SQLite type/default/constraint declaration.
    AddColumn {
        table: String,
        column: String,
        definition: String,
    },
    /// Remove reconstructable data without losing authoritative business values.
    DropDerivedColumn { table: String, column: String },
    /// Explicitly authorized retirement; original values remain in rollback backup.
    RetireColumn { table: String, column: String },
    /// Explicitly authorized table retirement, including its table-owned indexes.
    RetireTable { table: String },
}

pub(crate) fn changes() -> Vec<SchemaChange> {
    let mut result: Vec<_> = crate::search_storage::DERIVED_COLUMNS
        .iter()
        .map(|column| SchemaChange::DropDerivedColumn {
            table: "audio_files".into(),
            column: (*column).into(),
        })
        .collect();
    result.insert(0, SchemaChange::CreateAlbumEntities);
    result.push(SchemaChange::DropDerivedColumn {
        table: "audio_files".into(),
        column: "album".into(),
    });
    result.push(SchemaChange::AddColumn {
        table: "audio_files".into(),
        column: "album_id".into(),
        definition: "INTEGER REFERENCES albums(id)".into(),
    });
    // Schema 8 replaces v7's duplicated derived album text with an entity binding.
    // Final v6 contract retires only the two playback fields and history table.
    // Client-local history import runs after backup and before this recipe.
    result.push(SchemaChange::RetireColumn {
        table: "audio_files".into(),
        column: "play_count".into(),
    });
    result.push(SchemaChange::RetireColumn {
        table: "audio_files".into(),
        column: "last_played_at".into(),
    });
    result.push(SchemaChange::RetireTable {
        table: "play_history".into(),
    });
    result
}

pub(super) fn identity() -> String {
    let mut hash = Sha256::new();
    hash.update(b"ale-schema8-albums1-text-metadata1-retire-plays2;");
    // Index batches and query compilation must keep one implementation identity,
    // including source Metadata projection and the exact locked dependency graph.
    hash.update(super::plan::tokenizer_identity().as_bytes());
    hash.update(include_bytes!("recipe.rs"));
    hash.update(include_bytes!("stage.rs"));
    hash.update(include_bytes!("checkpoint.rs"));
    hash.update(include_bytes!("job.rs"));
    hash.update(include_bytes!("../search_storage.rs"));
    hash.update(include_bytes!("../albums.rs"));
    hash.update(include_bytes!("../metadata_storage.rs"));
    hash.update(include_bytes!("../schema.rs"));
    hash.update(include_bytes!("../schema/current.rs"));
    hash.update(include_bytes!("../schema/legacy.rs"));
    hash.update(include_bytes!("../local_play_history.rs"));
    hash.update(include_bytes!("../import_lease.rs"));
    hash.update(include_bytes!("../migration.rs"));
    hash.update(include_bytes!("../../../audiolib-core/src/config.rs"));
    hash.update(include_bytes!("../schema/definition.rs"));
    hash.update(include_bytes!("../schema/search_upgrade.rs"));
    hash.update(include_bytes!("inspect.rs"));
    hash.update(include_bytes!("diagnose.rs"));
    hash.update(include_bytes!("plan.rs"));
    hash.update(include_bytes!("runtime.rs"));
    format!("{:x}", hash.finalize())
}
