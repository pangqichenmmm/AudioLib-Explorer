// crates/audiolib-core/src/local_libraries.rs
//! Local source registrations; database identity validation remains a provider concern.
use super::{ActiveSource, AppConfig, DataSourceMode, DataSourceSettings};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The original working library. Its registration cannot be removed.
pub const DEFAULT_LOCAL_LIBRARY_ID: &str = "default";
const MAX_LOCAL_LIBRARIES: usize = 64;

fn enabled() -> bool {
    true
}

/// One local sound library, independently named and configured without moving its data.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalLibrary {
    /// Stable registration identity, independent of display name and database path.
    pub id: String,
    /// User-facing name; the original registration starts as `default`.
    pub name: String,
    /// Optional plain-text description, at most 2000 characters.
    #[serde(default)]
    pub description: String,
    /// Absolute path to the library database; never a source audio directory.
    pub database: PathBuf,
    /// Generate missing list Waveforms in the background; new registrations default on.
    #[serde(default = "enabled")]
    pub pregenerate_list_waveforms: bool,
}

impl LocalLibrary {
    /// Validate the stored registration without opening or changing the database.
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(Error::Validation("Invalid local library identity".into()));
        }
        validate_details(&self.name, &self.description)?;
        if !self.database.is_absolute()
            || self.database.as_os_str().len() > 32767
            || self.database.to_string_lossy().contains('\0')
            || self.database.file_name().is_none()
        {
            return Err(Error::Validation(
                "Local library requires an absolute database file path".into(),
            ));
        }
        Ok(())
    }
}

fn validate_details(name: &str, description: &str) -> Result<()> {
    if name.trim().is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(Error::Validation(
            "Local library name requires 1..=64 characters without control characters".into(),
        ));
    }
    if description.chars().count() > 2000
        || description
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Err(Error::Validation(
            "Local library description must be at most 2000 characters".into(),
        ));
    }
    Ok(())
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(path.canonicalize().unwrap_or(path))
}

fn same_database(left: &Path, right: &Path) -> bool {
    if same_lexical_database(left, right) {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => same_lexical_database(&left, &right),
        _ => false,
    }
}

/// Compare persisted spellings without touching files or unavailable network shares.
fn same_lexical_database(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        windows_database_spelling(left).eq_ignore_ascii_case(&windows_database_spelling(right))
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

#[cfg(windows)]
fn windows_database_spelling(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    // canonicalize() returns the extended namespace, while legacy active_database
    // values often retain ordinary drive/UNC syntax. Keep these identities stable
    // when the file is absent. Other device namespaces are not interchangeable.
    if path
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("//?/UNC/"))
    {
        format!("//{}", &path[8..])
    } else if path.starts_with("//?/")
        && path.as_bytes().get(4).is_some_and(u8::is_ascii_alphabetic)
        && path.as_bytes().get(5) == Some(&b':')
        && path.as_bytes().get(6) == Some(&b'/')
    {
        path[4..].to_owned()
    } else {
        path
    }
}

impl DataSourceSettings {
    /// Structurally validate a registry without filesystem I/O. Existing databases
    /// are checked when registered/opened; unavailable paths remain editable.
    pub fn validate_local_libraries(&self) -> Result<()> {
        if self.local_libraries.is_empty() {
            return Ok(());
        }
        if self.local_libraries.len() > MAX_LOCAL_LIBRARIES
            || !self
                .local_libraries
                .iter()
                .any(|entry| entry.id == DEFAULT_LOCAL_LIBRARY_ID)
        {
            return Err(Error::Validation(
                "Local libraries require the default library and at most 64 registrations".into(),
            ));
        }
        for (index, entry) in self.local_libraries.iter().enumerate() {
            entry.validate()?;
            if self.local_libraries[..index].iter().any(|other| {
                other.id == entry.id || same_lexical_database(&other.database, &entry.database)
            }) {
                return Err(Error::Validation(
                    "Duplicate local library registration".into(),
                ));
            }
        }
        if let Some(id) = &self.active_local_library
            && !self.local_libraries.iter().any(|entry| &entry.id == id)
        {
            return Err(Error::Validation(
                "Selected local library is not registered".into(),
            ));
        }
        Ok(())
    }

    /// Mirror the legacy selected path and checkbox, preserving older-client changes.
    pub(super) fn reconcile_local_libraries(
        &mut self,
        database: &Path,
        pregenerate: bool,
    ) -> Result<()> {
        // Saved selections already carry their stable path identity. In server
        // mode the remembered local database may be on a disconnected share;
        // ordinary preference edits must not probe it just to mirror a checkbox.
        if let Some(index) = self
            .local_libraries
            .iter()
            .position(|entry| same_lexical_database(&entry.database, database))
        {
            self.local_libraries[index].pregenerate_list_waveforms = pregenerate;
            self.active_local_library = Some(self.local_libraries[index].id.clone());
            return self.validate_local_libraries();
        }
        let database = absolute(database)?;
        if self.local_libraries.is_empty() {
            self.local_libraries.push(LocalLibrary {
                id: DEFAULT_LOCAL_LIBRARY_ID.into(),
                name: "default".into(),
                description: String::new(),
                database: database.clone(),
                pregenerate_list_waveforms: pregenerate,
            });
        }
        let index = match self
            .local_libraries
            .iter()
            .position(|entry| same_database(&entry.database, &database))
        {
            Some(index) => index,
            None => {
                let id = self.next_local_id();
                self.local_libraries.push(LocalLibrary {
                    id,
                    name: database
                        .file_stem()
                        .and_then(|name| name.to_str())
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or("Local library")
                        .chars()
                        .take(64)
                        .collect(),
                    description: String::new(),
                    database,
                    pregenerate_list_waveforms: pregenerate,
                });
                self.local_libraries.len() - 1
            }
        };
        self.local_libraries[index].pregenerate_list_waveforms = pregenerate;
        self.active_local_library = Some(self.local_libraries[index].id.clone());
        self.validate_local_libraries()
    }

    fn next_local_id(&self) -> String {
        loop {
            let id = format!("local-{}", self.new_profile_id());
            if !self.local_libraries.iter().any(|entry| entry.id == id) {
                return id;
            }
        }
    }
}

impl AppConfig {
    /// Find one saved local source without accessing its database.
    pub fn local_library(&self, id: &str) -> Option<&LocalLibrary> {
        self.data_source
            .local_libraries
            .iter()
            .find(|entry| entry.id == id)
    }

    /// The local source retained when switching to or from a server.
    pub fn selected_local_library(&self) -> Option<&LocalLibrary> {
        self.data_source
            .active_local_library
            .as_deref()
            .and_then(|id| self.local_library(id))
    }

    /// Migrate legacy selections in memory; this never writes or creates a database.
    pub fn reconcile_local_libraries(&mut self) -> Result<()> {
        self.data_source
            .reconcile_local_libraries(&self.paths.database, self.pregenerate_list_waveforms)
    }

    /// Register an existing provider-validated library; the selection stays unchanged.
    pub fn register_local_library(
        &mut self,
        name: &str,
        description: &str,
        database: &Path,
        pregenerate: bool,
    ) -> Result<String> {
        validate_details(name, description)?;
        if !database.is_file() {
            return Err(Error::NotFound(format!(
                "Library database {}",
                database.display()
            )));
        }
        let database = database.canonicalize()?;
        let mut source = self.data_source.clone();
        source.reconcile_local_libraries(&self.paths.database, self.pregenerate_list_waveforms)?;
        if source
            .local_libraries
            .iter()
            .any(|entry| same_database(&entry.database, &database))
        {
            return Err(Error::Validation(
                "This local library is already registered".into(),
            ));
        }
        let id = source.next_local_id();
        source.local_libraries.push(LocalLibrary {
            id: id.clone(),
            name: name.trim().into(),
            description: description.trim().into(),
            database,
            pregenerate_list_waveforms: pregenerate,
        });
        source.validate_local_libraries()?;
        self.data_source = source;
        Ok(id)
    }

    /// Update display information and the per-library checkbox, preserving path/id.
    pub fn update_local_library(
        &mut self,
        id: &str,
        name: &str,
        description: &str,
        pregenerate: bool,
    ) -> Result<()> {
        validate_details(name, description)?;
        let selected = self.data_source.active_local_library.as_deref() == Some(id);
        let entry = self
            .data_source
            .local_libraries
            .iter_mut()
            .find(|entry| entry.id == id)
            .ok_or_else(|| Error::NotFound(format!("Local library {id}")))?;
        entry.name = name.trim().into();
        entry.description = description.trim().into();
        entry.pregenerate_list_waveforms = pregenerate;
        if selected {
            self.pregenerate_list_waveforms = pregenerate;
        }
        Ok(())
    }

    /// Forget an inactive registration, retaining every database, annotation and WAV.
    /// Removing the remembered local source in server mode remembers `default` instead.
    pub fn remove_local_library(&mut self, id: &str) -> Result<()> {
        if id == DEFAULT_LOCAL_LIBRARY_ID {
            return Err(Error::Validation(
                "The default library cannot be removed".into(),
            ));
        }
        let mut updated = self.clone();
        if updated.data_source.active_local_library.as_deref() == Some(id) {
            if updated.data_source.mode != DataSourceMode::Server
                || updated.data_source.active_source() == ActiveSource::Local
            {
                return Err(Error::Busy(
                    "Select another local library before removing this one".into(),
                ));
            }
            updated.select_local_library(DEFAULT_LOCAL_LIBRARY_ID)?;
        }
        let index = updated
            .data_source
            .local_libraries
            .iter()
            .position(|entry| entry.id == id)
            .ok_or_else(|| Error::NotFound(format!("Local library {id}")))?;
        updated.data_source.local_libraries.remove(index);
        *self = updated;
        Ok(())
    }

    /// Select a provider-validated local database. Source-mode changes and saves remain explicit.
    pub fn select_local_library(&mut self, id: &str) -> Result<()> {
        let mut updated = self.clone();
        updated.reconcile_local_libraries()?;
        let entry = updated
            .local_library(id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("Local library {id}")))?;
        if !entry.database.is_file() && id != DEFAULT_LOCAL_LIBRARY_ID {
            return Err(Error::NotFound(format!(
                "Library database {}",
                entry.database.display()
            )));
        }
        updated.paths.database = entry.database;
        updated.paths.legacy_database = None;
        updated.pregenerate_list_waveforms = entry.pregenerate_list_waveforms;
        updated.data_source.active_local_library = Some(entry.id);
        *self = updated;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(root: &Path) -> AppConfig {
        AppConfig::load_with_roots(root.to_owned(), &[]).unwrap()
    }

    #[test]
    fn legacy_selection_becomes_default_without_moving_or_creating_databases() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("working.db");
        std::fs::write(&original, b"existing library bytes").unwrap();
        let root = temp.path().join("preferences");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("settings.json"),
            serde_json::to_vec(&serde_json::json!({
                "active_database": original,
                "pregenerate_list_waveforms": false,
                "language": "cn"
            }))
            .unwrap(),
        )
        .unwrap();
        let config = config(&root);
        let library = config.selected_local_library().unwrap();
        assert_eq!(library.id, DEFAULT_LOCAL_LIBRARY_ID);
        assert_eq!(library.name, "default");
        assert_eq!(library.database, original.canonicalize().unwrap());
        assert!(!library.pregenerate_list_waveforms);
        assert!(!root.join("library.db").exists());
        config.save().unwrap();
        assert_eq!(std::fs::read(&original).unwrap(), b"existing library bytes");
    }

    #[test]
    fn local_sources_round_trip_selection_descriptions_and_independent_waveform_preferences() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("preferences");
        let additional = temp.path().join("effects.db");
        std::fs::write(&additional, []).unwrap();
        let mut config = config(&root);
        assert!(
            config
                .selected_local_library()
                .unwrap()
                .pregenerate_list_waveforms
        );
        let id = config
            .register_local_library("Effects", "Nature and Foley", &additional, false)
            .unwrap();
        assert_eq!(
            config.selected_local_library().unwrap().id,
            DEFAULT_LOCAL_LIBRARY_ID
        );
        config.select_local_library(&id).unwrap();
        assert!(!config.pregenerate_list_waveforms);
        config.save().unwrap();
        let mut loaded = self::config(&root);
        assert_eq!(loaded.paths.database, additional.canonicalize().unwrap());
        assert_eq!(
            loaded.selected_local_library().unwrap().description,
            "Nature and Foley"
        );
        assert!(!loaded.pregenerate_list_waveforms);
        loaded
            .select_local_library(DEFAULT_LOCAL_LIBRARY_ID)
            .unwrap();
        assert!(loaded.pregenerate_list_waveforms);
        loaded
            .update_local_library(&id, "Renamed", "Updated", true)
            .unwrap();
        loaded.select_local_library(&id).unwrap();
        assert!(loaded.pregenerate_list_waveforms);
        loaded.pregenerate_list_waveforms = false;
        loaded.save().unwrap();
        assert!(
            !self::config(&root)
                .selected_local_library()
                .unwrap()
                .pregenerate_list_waveforms
        );
    }

    #[test]
    fn removal_is_registration_only_and_default_or_active_sources_are_protected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("preferences");
        let database = temp.path().join("other.db");
        let sidecar = temp.path().join("other.db-wal");
        let audio = temp.path().join("source.wav");
        for path in [&database, &sidecar, &audio] {
            std::fs::write(path, b"preserve").unwrap();
        }
        let mut config = config(&root);
        let id = config
            .register_local_library("Other", "", &database, true)
            .unwrap();
        assert!(
            config
                .remove_local_library(DEFAULT_LOCAL_LIBRARY_ID)
                .is_err()
        );
        config.select_local_library(&id).unwrap();
        assert!(config.remove_local_library(&id).is_err());
        config
            .select_local_library(DEFAULT_LOCAL_LIBRARY_ID)
            .unwrap();
        config.remove_local_library(&id).unwrap();
        config.save().unwrap();
        assert!(self::config(&root).local_library(&id).is_none());
        for path in [&database, &sidecar, &audio] {
            assert_eq!(std::fs::read(path).unwrap(), b"preserve");
        }
    }

    #[test]
    fn server_mode_can_forget_remembered_local_source_while_retaining_the_server() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("preferences");
        let database = temp.path().join("remembered.db");
        std::fs::write(&database, b"library annotations").unwrap();
        let mut config = config(&root);
        let default = config
            .local_library(DEFAULT_LOCAL_LIBRARY_ID)
            .unwrap()
            .clone();
        let id = config
            .register_local_library("Remembered", "", &database, false)
            .unwrap();
        config.select_local_library(&id).unwrap();
        config.data_source.mode = DataSourceMode::Server;
        config.data_source.server_url = "http://server.example.lan:8790".into();
        config.data_source.migrate_profiles();
        let server = config.data_source.active_source();
        assert_ne!(server, ActiveSource::Local);
        config.remove_local_library(&id).unwrap();
        assert_eq!(config.data_source.active_source(), server);
        assert_eq!(config.data_source.mode, DataSourceMode::Server);
        assert_eq!(
            config.selected_local_library().unwrap().id,
            DEFAULT_LOCAL_LIBRARY_ID
        );
        assert_eq!(config.paths.database, default.database);
        assert_eq!(
            config.pregenerate_list_waveforms,
            default.pregenerate_list_waveforms
        );
        config.save().unwrap();
        let loaded = self::config(&root);
        assert_eq!(loaded.data_source.active_source(), server);
        assert!(loaded.local_library(&id).is_none());
        assert_eq!(std::fs::read(&database).unwrap(), b"library annotations");
    }

    #[test]
    fn duplicate_or_invalid_sources_fail_before_mutation_and_missing_sources_are_not_created() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("preferences");
        let database = temp.path().join("another.db");
        std::fs::write(&database, []).unwrap();
        let mut config = config(&root);
        let id = config
            .register_local_library("Other", "", &database, true)
            .unwrap();
        let before = config.data_source.clone();
        assert!(
            config
                .register_local_library("Duplicate", "", &database, true)
                .is_err()
        );
        assert!(
            config
                .register_local_library(" \n ", "", &database, true)
                .is_err()
        );
        assert!(config.update_local_library(&id, "", "", false).is_err());
        assert_eq!(config.data_source, before);
        std::fs::remove_file(&database).unwrap();
        assert!(config.select_local_library(&id).is_err());
        assert!(!database.exists());
        assert_eq!(config.data_source, before);
    }

    #[test]
    fn older_client_database_changes_are_registered_without_replacing_original_default() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("preferences");
        let config = config(&root);
        let original = config.selected_local_library().unwrap().database.clone();
        config.save().unwrap();
        let different = temp.path().join("older-client.db");
        std::fs::write(&different, []).unwrap();
        let settings = root.join("settings.json");
        let mut stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
        stored["active_database"] = serde_json::to_value(&different).unwrap();
        stored["pregenerate_list_waveforms"] = false.into();
        std::fs::write(&settings, serde_json::to_vec(&stored).unwrap()).unwrap();
        let loaded = self::config(&root);
        assert_eq!(
            loaded
                .local_library(DEFAULT_LOCAL_LIBRARY_ID)
                .unwrap()
                .database,
            original
        );
        assert_eq!(loaded.data_source.local_libraries.len(), 2);
        assert_eq!(
            loaded.selected_local_library().unwrap().database,
            different.canonicalize().unwrap()
        );
        assert!(!loaded.pregenerate_list_waveforms);
    }

    #[cfg(windows)]
    #[test]
    fn unavailable_migrated_default_keeps_its_identity_when_the_file_returns() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("preferences");
        let database = temp.path().join("existing-working.db");
        std::fs::write(&database, b"existing library annotations").unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("settings.json"),
            serde_json::to_vec(&serde_json::json!({ "active_database": database })).unwrap(),
        )
        .unwrap();
        let migrated = config(&root);
        assert_eq!(
            migrated.selected_local_library().unwrap().database,
            database.canonicalize().unwrap()
        );
        migrated.save().unwrap();
        std::fs::remove_file(&database).unwrap();

        let unavailable = config(&root);
        assert_eq!(unavailable.data_source.local_libraries.len(), 1);
        assert_eq!(
            unavailable.selected_local_library().unwrap().id,
            DEFAULT_LOCAL_LIBRARY_ID
        );
        unavailable.save().unwrap();
        assert!(
            !database.exists(),
            "settings must not recreate the database"
        );

        std::fs::write(&database, b"existing library annotations").unwrap();
        let available = config(&root);
        assert_eq!(available.data_source.local_libraries.len(), 1);
        assert_eq!(
            available.selected_local_library().unwrap().id,
            DEFAULT_LOCAL_LIBRARY_ID
        );
        assert_eq!(
            std::fs::read(&database).unwrap(),
            b"existing library annotations"
        );
    }

    #[cfg(windows)]
    #[test]
    fn offline_drive_and_unc_spellings_match_only_their_standard_namespace_alias() {
        assert!(same_lexical_database(
            Path::new(r"\\?\C:\Sounds\Library.db"),
            Path::new(r"c:/sounds/library.db")
        ));
        assert!(same_lexical_database(
            Path::new(r"\\?\UNC\nas\Audio\Library.db"),
            Path::new(r"\\NAS\audio\library.db")
        ));
        assert!(!same_lexical_database(
            Path::new(r"\\?\UNC\nas\Audio\Library.db"),
            Path::new(r"\\nas\Other\Library.db")
        ));
        assert!(!same_lexical_database(
            Path::new(r"\\?\C:\Sounds\one.db"),
            Path::new(r"C:\Sounds\two.db")
        ));
        assert!(!same_lexical_database(
            Path::new(r"\\?\GLOBALROOT\Device\Library.db"),
            Path::new(r"GLOBALROOT\Device\Library.db")
        ));
    }

    #[test]
    fn structural_validation_leaves_filesystem_type_checks_to_registration() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = config(&temp.path().join("preferences"));
        // A path's current type/availability cannot gate editing unrelated server
        // preferences. Registration and provider opening still require a real file.
        let directory = temp.path().join("not-a-database");
        std::fs::create_dir_all(&directory).unwrap();
        config.data_source.local_libraries[0].database = directory.clone();
        config.data_source.validate_local_libraries().unwrap();
        config.data_source.local_libraries[0].database = config.paths.database.clone();
        assert!(
            config
                .register_local_library("Invalid", "", &directory, true)
                .is_err()
        );
    }
}
