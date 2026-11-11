// crates/audiolib-remote/src/roots.rs
//! Authenticated browsing and management of sound-library directories on the server.
//! These paths belong to the server and are never resolved or accessed locally.
use crate::{RemoteClient, RemoteError, RemoteRoot, Result, encode_path_segment};
use serde::{Deserialize, Serialize};
use std::time::Duration;

// Relocation probes every corresponding WAV and writes a rollback backup; root
// removal may delete a large indexed subtree. Allow the snapshot-transfer budget
// for large libraries and slow storage rather than the ordinary-write timeout.
const ROOT_MUTATION_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

fn root_mutation_agent() -> crate::abort::Timeouts {
    crate::client::agent(ROOT_MUTATION_RESPONSE_TIMEOUT, Duration::from_secs(30))
}

/// A starting point of the remote folder browser (`GET /fs` without `path`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemotePlace {
    /// `root`, `volume`, `home` or `library_root`; future kinds remain readable.
    pub kind: String,
    pub name: String,
    /// Absolute directory on the server.
    pub path: String,
    #[serde(default)]
    pub network: Option<bool>,
}

/// A child directory from one non-recursive server listing.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteDirectoryEntry {
    pub name: String,
    /// Absolute server path. The server resolves a symbolic link when entered.
    pub path: String,
    #[serde(default)]
    pub is_symlink: bool,
}

/// `GET /fs` reply: either browser starting places or one canonical directory.
/// Entry/file counts are bounded by the server; truncation is explicit.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteDirectory {
    /// Canonical absolute directory; `None` for the starting-places reply.
    #[serde(default)]
    pub path: Option<String>,
    /// Canonical parent; `None` at a filesystem root or for starting places.
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub network: Option<bool>,
    #[serde(default)]
    pub places: Vec<RemotePlace>,
    #[serde(default)]
    pub entries: Vec<RemoteDirectoryEntry>,
    #[serde(default)]
    pub truncated: bool,
    /// WAV files immediately within the directory, excluding subdirectories.
    #[serde(default)]
    pub wav_files: u64,
    /// New servers report all supported source formats; old servers omit this field.
    #[serde(default)]
    pub audio_files: Option<u64>,
    #[serde(default)]
    pub other_files: u64,
    #[serde(default)]
    pub counts_truncated: bool,
}

/// `POST /roots` body: register an existing directory as a sound-library root.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootCreateRequest {
    /// Unique client-visible name without path separators.
    pub name: String,
    /// Existing absolute directory on the server.
    pub path: String,
}

/// `DELETE /roots/{name}` reply. Only database records and root configuration are
/// removed; source audio files and directories remain on the server.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootRemoveReport {
    pub name: String,
    pub removed: u64,
    pub folders_removed: u64,
    pub revision_before: u64,
    pub revision: u64,
}

impl RootRemoveReport {
    /// Whether database records changed and the snapshot must be refreshed. Root
    /// configuration must be refreshed even if the revision stays the same.
    pub fn changed(&self) -> bool {
        self.revision != self.revision_before
    }
}

fn root_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > 128
        || matches!(name, "." | "..")
        || name
            .chars()
            .any(|character| matches!(character, '/' | '\\') || character.is_control())
    {
        return Err(RemoteError::Validation(
            "Use a root name of 1..=128 characters without path separators or control characters"
                .into(),
        ));
    }
    Ok(name)
}

fn server_path(path: &str) -> Result<&str> {
    // Server-native absoluteness, permissions, symlinks and root overlaps are checked
    // on the server. A Windows client must accept macOS/Linux absolute paths too.
    if path.trim().is_empty() || path.len() > 4096 || path.contains('\0') {
        return Err(RemoteError::Validation(
            "Use a non-empty absolute server directory (at most 4096 bytes)".into(),
        ));
    }
    Ok(path)
}

impl RemoteClient {
    /// Browse the connected server's filesystem. `None` returns starting places;
    /// `Some(path)` lists its child directories. Hidden entries require `hidden = true`.
    /// The authenticated server enforces its management policy and filesystem access.
    pub fn browse_directory(&self, path: Option<&str>, hidden: bool) -> Result<RemoteDirectory> {
        let path = match path {
            None => "/fs".to_owned(),
            Some(path) => format!(
                "/fs?path={}&hidden={}",
                encode_path_segment(server_path(path)?),
                u8::from(hidden)
            ),
        };
        self.manage_json("GET", &path, None)
    }

    /// Register an existing remote directory. The server persists root configuration;
    /// scanning is an explicit subsequent action. No source files are changed.
    pub fn create_root(&self, request: &RootCreateRequest) -> Result<RemoteRoot> {
        self.add_root(&request.name, &request.path)
    }

    /// `POST /roots`: register an existing absolute server directory under `name`.
    /// This mutation is bound to the client's current server-library identity.
    pub fn add_root(&self, name: &str, path: &str) -> Result<RemoteRoot> {
        let body = serde_json::json!({"name":root_name(name)?,"path":server_path(path)?});
        self.manage_json("POST", "/roots", Some(&body))
    }

    /// `PATCH /roots/{name}`: change a root's resource path on the server, preserving
    /// existing record identities and Metadata through the server's relink workflow.
    /// Source audio is neither moved nor deleted. Refresh roots and snapshot afterwards.
    /// This can take up to the one-hour response budget on a large library or slow NAS;
    /// a timeout leaves the outcome uncertain, so refresh rather than automatically retry.
    pub fn update_root_path(&self, name: &str, path: &str) -> Result<RemoteRoot> {
        let endpoint = format!("/roots/{}", encode_path_segment(root_name(name)?));
        let body = serde_json::json!({"path":server_path(path)?});
        let agent = root_mutation_agent();
        self.manage_json_with(Some(&agent), "PATCH", &endpoint, Some(&body))
    }

    /// `DELETE /roots/{name}`: unregister a root and forget its database records.
    /// Never deletes source files. Refresh roots even if no records were removed.
    /// Like relocation, large removals get a one-hour response budget and are never
    /// automatically retried; an uncertain reply requires a refresh.
    pub fn remove_root(&self, name: &str) -> Result<RootRemoveReport> {
        let endpoint = format!("/roots/{}", encode_path_segment(root_name(name)?));
        let agent = root_mutation_agent();
        self.manage_json_with(Some(&agent), "DELETE", &endpoint, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{TestResponse, TestServer};
    use std::sync::{Arc, Mutex};

    #[test]
    fn relocation_allows_long_validation_but_bounds_the_small_json_reply() {
        // Waiting out a one-hour network deadline is unsuitable for a regression
        // suite. Verify the budget of the agent used by PATCH/DELETE instead; the loopback
        // mutation test below checks its actual method, headers and no-retry behavior.
        let agent = root_mutation_agent();
        assert_eq!(agent.response, Duration::from_secs(3600));
        assert_eq!(agent.body, Some(Duration::from_secs(30)));
        let ordinary = RemoteClient::new("http://127.0.0.1:1", "").unwrap();
        assert!(agent.response > ordinary.writes.response);
    }

    #[test]
    fn remote_browser_encodes_server_paths_and_reads_both_reply_shapes() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let server = TestServer::start(move |request| {
            assert_eq!(
                request.header("authorization"),
                Some("Bearer private-token")
            );
            assert_eq!(request.method, "GET");
            log.lock().unwrap().push(request.path.clone());
            if request.path == "/api/v1/fs" {
                TestResponse::json(
                    200,
                    r#"{"places":[{"kind":"volume","name":"音效库","path":"/Volumes/SFX","network":true}]}"#,
                )
            } else {
                TestResponse::json(
                    200,
                    r#"{"path":"/Volumes/音效 库?#","parent":"/Volumes","network":false,"entries":[{"name":"Foley","path":"/Volumes/音效 库?#/Foley","is_symlink":true}],"truncated":true,"wav_files":12,"other_files":1,"counts_truncated":true,"future":1}"#,
                )
            }
        });
        let client = RemoteClient::new(&server.url, "private-token").unwrap();
        let places = client.browse_directory(None, false).unwrap();
        assert_eq!(places.path, None);
        assert!(places.entries.is_empty());
        assert_eq!(places.places[0].kind, "volume");
        assert_eq!(places.places[0].network, Some(true));
        let listing = client
            .browse_directory(Some("/Volumes/音效 库?#"), true)
            .unwrap();
        assert_eq!(listing.path.as_deref(), Some("/Volumes/音效 库?#"));
        assert_eq!(listing.parent.as_deref(), Some("/Volumes"));
        assert!(listing.places.is_empty() && listing.entries[0].is_symlink);
        assert!(listing.truncated && listing.counts_truncated);
        assert_eq!((listing.wav_files, listing.other_files), (12, 1));
        client
            .browse_directory(Some("C:\\SFX Library"), false)
            .unwrap();
        client
            .browse_directory(Some("/Volumes/Space "), false)
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "/api/v1/fs",
                "/api/v1/fs?path=%2FVolumes%2F%E9%9F%B3%E6%95%88%20%E5%BA%93%3F%23&hidden=1",
                "/api/v1/fs?path=C%3A%5CSFX%20Library&hidden=0",
                "/api/v1/fs?path=%2FVolumes%2FSpace%20&hidden=0"
            ]
        );
    }

    #[test]
    fn root_mutations_use_correct_methods_library_identity_and_owned_reports() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let server = TestServer::start(move |request| {
            if let Some(response) = crate::test_server::marker_handshake(&request.path) {
                return response;
            }
            if request.path == "/api/v1/library" {
                return TestResponse::json(
                    200,
                    r#"{"library_id":"library-roots","schema_version":5,"identity_bound_writes":true}"#,
                );
            }
            assert_eq!(
                request.header(crate::LIBRARY_ID_HEADER),
                Some("library-roots")
            );
            assert_eq!(
                request.header("authorization"),
                Some("Bearer private-token")
            );
            log.lock().unwrap().push((
                request.method.clone(),
                request.path.clone(),
                request.body.clone(),
            ));
            match request.method.as_str() {
                "POST" => TestResponse::json(
                    201,
                    r#"{"name":"音效 #","server_path":"/Volumes/SFX","online":true}"#,
                ),
                "PATCH" => TestResponse::json(
                    200,
                    r#"{"name":"音效 #","server_path":"/Volumes/New SFX","file_count":2,"online":true}"#,
                ),
                "DELETE" => TestResponse::json(
                    200,
                    r#"{"name":"音效 #","removed":2,"folders_removed":3,"revision_before":4,"revision":5}"#,
                ),
                method => panic!("Unexpected method: {method}"),
            }
        });
        let client = RemoteClient::new(&server.url, "private-token")
            .unwrap()
            .with_library_id("library-roots")
            .unwrap();
        let root = client
            .create_root(&RootCreateRequest {
                name: " 音效 # ".into(),
                path: "/Volumes/SFX ".into(),
            })
            .unwrap();
        assert_eq!(root.name, "音效 #");
        let updated = client
            .update_root_path("音效 #", "/Volumes/New SFX ")
            .unwrap();
        assert_eq!(updated.server_path, "/Volumes/New SFX");
        let removed = client.remove_root("音效 #").unwrap();
        assert_eq!((removed.removed, removed.folders_removed), (2, 3));
        assert!(removed.changed());
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "mutations are never retried");
        assert_eq!(
            (&requests[0].0[..], &requests[0].1[..]),
            ("POST", "/api/v1/roots")
        );
        assert_eq!(
            (&requests[1].0[..], &requests[1].1[..]),
            ("PATCH", "/api/v1/roots/%E9%9F%B3%E6%95%88%20%23")
        );
        assert_eq!(
            (&requests[2].0[..], &requests[2].1[..]),
            ("DELETE", "/api/v1/roots/%E9%9F%B3%E6%95%88%20%23")
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[0].2).unwrap(),
            serde_json::json!({"name":"音效 #","path":"/Volumes/SFX "})
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[1].2).unwrap(),
            serde_json::json!({"path":"/Volumes/New SFX "})
        );
        assert!(requests[2].2.is_empty());
    }

    #[test]
    fn root_errors_and_old_server_capabilities_remain_distinct() {
        let server = TestServer::start(|request| {
            if request.path == "/api/v1/health" {
                return crate::test_server::marker_handshake(&request.path).unwrap();
            }
            if request.path == "/api/v1/library" {
                return TestResponse::json(
                    200,
                    r#"{"library_id":"library-roots","schema_version":5,"identity_bound_writes":true}"#,
                );
            }
            let (status, code, message) = match request.path.as_str() {
                "/api/v1/capabilities" => {
                    return crate::test_server::marker_capabilities(
                        serde_json::from_str(r#"{"manage_mode":"all","can_manage":true}"#).unwrap(),
                    );
                }
                "/api/v1/fs" => (404, "NOT_FOUND", "No such endpoint"),
                "/api/v1/fs?path=%2Fdenied&hidden=0" => {
                    (403, "FS_FORBIDDEN", "Directory access denied")
                }
                "/api/v1/roots/missing" => (404, "NOT_FOUND", "Unknown library root"),
                "/api/v1/roots/busy" => (409, "BUSY", "A scan is running"),
                "/api/v1/roots/forbidden" => (403, "MANAGE_FORBIDDEN", "manage_mode = off"),
                "/api/v1/roots/old" => (405, "METHOD_NOT_ALLOWED", "Method not allowed"),
                _ => (500, "INTERNAL", "Unexpected endpoint"),
            };
            TestResponse::json(
                status,
                &serde_json::json!({"error_code":code,"message":message}).to_string(),
            )
        });
        let client = RemoteClient::new(&server.url, "token").unwrap();
        let capabilities = client.capabilities().unwrap();
        assert!(capabilities.may_manage());
        assert!(!capabilities.root_management && !capabilities.may_manage_roots());
        let supported: crate::Capabilities =
            serde_json::from_str(r#"{"root_management":true,"can_manage":true}"#).unwrap();
        assert!(supported.may_manage_roots());
        let denied: crate::Capabilities =
            serde_json::from_str(r#"{"root_management":true,"can_manage":false}"#).unwrap();
        assert!(!denied.may_manage_roots());
        assert!(matches!(
            client.browse_directory(None, false),
            Err(RemoteError::Unsupported(_))
        ));
        assert!(matches!(
            client.browse_directory(Some("/denied"), false),
            Err(RemoteError::Server { status: 403, .. })
        ));
        assert!(matches!(
            client.remove_root("missing"),
            Err(RemoteError::NotFound(_))
        ));
        assert!(matches!(
            client.update_root_path("busy", "/SFX"),
            Err(RemoteError::Busy { .. })
        ));
        assert!(matches!(
            client.remove_root("forbidden"),
            Err(RemoteError::Forbidden(_))
        ));
        assert!(matches!(
            client.remove_root("old"),
            Err(RemoteError::Unsupported(_))
        ));
        let unchanged: RootRemoveReport = serde_json::from_str(
            r#"{"name":"Empty","removed":0,"folders_removed":0,"revision_before":4,"revision":4}"#,
        )
        .unwrap();
        assert!(!unchanged.changed());
    }

    #[test]
    fn invalid_requests_never_connect_and_stale_library_never_mutates() {
        // The closed port must never be accessed for client-side input failures.
        let client = RemoteClient::new("http://127.0.0.1:1", "").unwrap();
        for name in ["", " ", ".", "..", "a/b", "a\\b", "a\0b", "a\nb"] {
            assert!(matches!(
                client.add_root(name, "/SFX"),
                Err(RemoteError::Validation(_))
            ));
            assert!(matches!(
                client.remove_root(name),
                Err(RemoteError::Validation(_))
            ));
            assert!(matches!(
                client.update_root_path(name, "/SFX"),
                Err(RemoteError::Validation(_))
            ));
        }
        for path in [String::new(), "a\0b".into(), "a".repeat(4097)] {
            assert!(matches!(
                client.browse_directory(Some(&path), false),
                Err(RemoteError::Validation(_))
            ));
            assert!(matches!(
                client.add_root("SFX", &path),
                Err(RemoteError::Validation(_))
            ));
            assert!(matches!(
                client.update_root_path("SFX", &path),
                Err(RemoteError::Validation(_))
            ));
        }
        let writes = Arc::new(Mutex::new(0));
        let counter = writes.clone();
        let server = TestServer::start(move |request| {
            if request.method != "GET" {
                *counter.lock().unwrap() += 1;
            }
            TestResponse::json(
                200,
                r#"{"library_id":"another-library","identity_bound_writes":true}"#,
            )
        });
        let client = RemoteClient::new(&server.url, "")
            .unwrap()
            .with_library_id("displayed-library")
            .unwrap();
        assert!(matches!(
            client.add_root("SFX", "/SFX"),
            Err(RemoteError::LibraryChanged(_))
        ));
        assert!(matches!(
            client.remove_root("SFX"),
            Err(RemoteError::LibraryChanged(_))
        ));
        assert_eq!(*writes.lock().unwrap(), 0);
    }
}
