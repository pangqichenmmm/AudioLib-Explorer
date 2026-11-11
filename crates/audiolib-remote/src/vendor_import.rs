// crates/audiolib-remote/src/vendor_import.rs
//! Vendor table import on the server (design §4.5, §4.7): preview (read-only), commit
//! with `Idempotency-Key`, rollback. Every call carries manage auth and
//! `X-AudioLib-Library-Id`. Only explicit migration refusals may be retried.

use crate::client::RemoteClient;
use crate::error::{RemoteError, Result};
use audiolib_core::vendor_import::*;
use std::path::Path;
use std::time::Duration;

/// PR #225 reserves this origin-relative boundary, but does not implement import
/// handlers. `capabilities.vendor_import.endpoint` can override the provisional
/// prefix. Preview/commit/rollback still require acceptance with that deployment.
pub const VENDOR_IMPORT_PATH: &str = audiolib_core::server_upgrade::VENDOR_IMPORT_API;
const MAX_REPLY_BYTES: u64 = 16 * 1024 * 1024;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// `multipart/form-data` body with one `file` part. Quotes and line breaks in the name
/// are replaced so the header cannot be broken out of.
pub(crate) fn multipart_body(boundary: &str, file_name: &str, bytes: &[u8]) -> Vec<u8> {
    let safe: String = file_name
        .chars()
        .map(|c| {
            if matches!(c, '"' | '\r' | '\n' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut body = Vec::with_capacity(bytes.len() + 256);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"file\"; filename=\"{safe}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

impl RemoteClient {
    fn vendor_call(
        &self,
        path: &str,
        content_type: Option<String>,
        body: Option<&[u8]>,
        idempotency_key: Option<&str>,
    ) -> Result<String> {
        self.vendor_http("POST", path, content_type, body, idempotency_key)
    }

    fn vendor_http(
        &self,
        method: &str,
        path: &str,
        content_type: Option<String>,
        body: Option<&[u8]>,
        idempotency_key: Option<&str>,
    ) -> Result<String> {
        let capabilities = self.capabilities()?;
        if !capabilities.may_manage() {
            return Err(RemoteError::Forbidden(
                "Vendor import requires manage permission".into(),
            ));
        }
        let info = capabilities.vendor_import.ok_or_else(|| {
            RemoteError::Unsupported("This server does not implement vendor import".into())
        })?;
        let prefix = vendor_prefix(info.endpoint.as_deref())?;
        let url = format!("{}{prefix}{path}", self.origin());
        let context = format!("{method} {prefix}{path}");
        let library_id = self.write_library_id()?;
        let compatibility = crate::compatibility::client_header()?;
        let agent = crate::client::agent(UPLOAD_TIMEOUT, Duration::from_secs(30));
        let request = match method {
            "GET" => agent.get(&url).force_send_body(),
            "PUT" => agent.put(&url),
            "POST" => agent.post(&url),
            _ => {
                return Err(RemoteError::Validation(
                    "Unsupported vendor import method".into(),
                ));
            }
        };
        let mut request = request
            .header("x-audiolib-admin", "1")
            .header(crate::client::LIBRARY_ID_HEADER, library_id.as_str())
            .header(
                audiolib_core::compatibility::CLIENT_COMPATIBILITY_HEADER,
                compatibility.as_str(),
            );
        if !self.token.is_empty() {
            request = request.header("Authorization", format!("Bearer {}", self.token));
        }
        if let Some(key) = idempotency_key {
            request = request.header("Idempotency-Key", key);
        }
        let result = match (content_type, body) {
            (Some(kind), Some(body)) => request.header("Content-Type", kind).send(body),
            _ => request.send_empty(),
        };
        let response = result.map_err(|error| RemoteError::from_transport(error, &url))?;
        let mut response = self.checked(response, &context)?;
        response
            .body_mut()
            .with_config()
            .limit(MAX_REPLY_BYTES)
            .read_to_string()
            .map_err(|error| RemoteError::Protocol(format!("{context}: {error}")))
    }

    /// `POST /vendor-import/preview` with the table as a multipart upload. Read-only on
    /// the server; returns the `import_id` used by commit and rollback.
    pub fn vendor_import_preview(&self, file: &Path) -> Result<VendorImportPreview> {
        let info = self.capabilities()?.vendor_import.ok_or_else(|| {
            RemoteError::Unsupported("This server does not implement vendor import".into())
        })?;
        let limits = audiolib_core::vendor_import::VendorLimits::from(&info);
        audiolib_core::vendor_import::precheck(file, limits)?
            .map_err(|error| RemoteError::Validation(error.text(false)))?;
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(file)?
            .take(limits.max_file_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| RemoteError::Validation(format!("{}: {error}", file.display())))?;
        if bytes.len() as u64 > limits.max_file_bytes {
            return Err(RemoteError::Validation(
                "Vendor table grew past the upload limit".into(),
            ));
        }
        let name = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "vendor.csv".into());
        let boundary = format!(
            "audiolib-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        let body = multipart_body(&boundary, &name, &bytes);
        let text = self.vendor_call(
            "/preview",
            Some(format!("multipart/form-data; boundary={boundary}")),
            Some(&body),
            None,
        )?;
        let preview: VendorImportPreview = serde_json::from_str(&text)
            .map_err(|error| RemoteError::Protocol(format!("vendor preview: {error}")))?;
        if preview.import_id.trim().is_empty() {
            return Err(RemoteError::Protocol("vendor preview: no import_id".into()));
        }
        Ok(preview)
    }

    /// Preview a bounded batch of tables with explicit mapping and matching options.
    pub fn vendor_import_preview_files(
        &self,
        files: &[std::path::PathBuf],
        options: &VendorImportOptions,
    ) -> Result<VendorImportPreview> {
        if files.is_empty() || files.len() > 512 {
            return Err(RemoteError::Validation(
                "Select 1..512 vendor tables".into(),
            ));
        }
        let capabilities = self.capabilities()?;
        let info = capabilities.vendor_import.ok_or_else(|| {
            RemoteError::Unsupported("This server does not implement vendor import".into())
        })?;
        let limits = VendorLimits::from(&info);
        let boundary = format!(
            "audiolib-vendor-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        let options =
            serde_json::to_vec(options).map_err(|e| RemoteError::Validation(e.to_string()))?;
        if options.len() > 1024 * 1024 {
            return Err(RemoteError::Validation(
                "Vendor options exceed 1 MiB".into(),
            ));
        }
        let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"options\"\r\nContent-Type: application/json\r\n\r\n").into_bytes();
        body.extend_from_slice(&options);
        body.extend_from_slice(b"\r\n");
        let mut bytes_total = 0_u64;
        for file in files {
            use std::io::Read;
            let length = std::fs::metadata(file)?.len();
            if length > limits.max_file_bytes {
                return Err(RemoteError::Validation(format!(
                    "Vendor file exceeds {} bytes",
                    limits.max_file_bytes
                )));
            }
            bytes_total = bytes_total.saturating_add(length);
            if bytes_total > 510 * 1024 * 1024 {
                return Err(RemoteError::Validation(
                    "Vendor upload batch exceeds the upload budget".into(),
                ));
            }
            let mut bytes = Vec::new();
            std::fs::File::open(file)?
                .take(limits.max_file_bytes.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > limits.max_file_bytes {
                return Err(RemoteError::Validation(
                    "Vendor file grew past its upload limit".into(),
                ));
            }
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            let part = multipart_body(&boundary, &name, &bytes);
            let closing = format!("--{boundary}--\r\n");
            body.extend_from_slice(part.strip_suffix(closing.as_bytes()).ok_or_else(|| {
                RemoteError::Protocol("Invalid vendor multipart encoding".into())
            })?);
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let text = self.vendor_http(
            "POST",
            "/preview",
            Some(format!("multipart/form-data; boundary={boundary}")),
            Some(&body),
            None,
        )?;
        vendor_json(&text)
    }

    /// Read durable import status, including progress and recovery keys.
    pub fn vendor_import_status(&self, id: &str) -> Result<VendorImportStatus> {
        vendor_json(&self.vendor_http(
            "GET",
            &format!("/{}", crate::util::encode_path_segment(id)),
            None,
            None,
            None,
        )?)
    }
    /// Re-preview an existing batch with explicit column mappings or match confirmations.
    pub fn vendor_import_mapping(
        &self,
        id: &str,
        options: &VendorImportOptions,
    ) -> Result<VendorImportPreview> {
        let bytes =
            serde_json::to_vec(options).map_err(|e| RemoteError::Validation(e.to_string()))?;
        vendor_json(&self.vendor_http(
            "PUT",
            &format!("/{}/mapping", crate::util::encode_path_segment(id)),
            Some("application/json".into()),
            Some(&bytes),
            None,
        )?)
    }
    /// Page unresolved and matched rows without downloading the whole table.
    pub fn vendor_import_rows(
        &self,
        id: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<VendorRowPreview>> {
        vendor_json(&self.vendor_http(
            "GET",
            &format!(
                "/{}/rows?offset={offset}&limit={limit}",
                crate::util::encode_path_segment(id)
            ),
            None,
            None,
            None,
        )?)
    }
    /// Read batches associated with the opened library.
    pub fn vendor_import_history(&self) -> Result<VendorImportHistory> {
        vendor_json(&self.vendor_http("GET", "/", None, None, None)?)
    }

    /// Page every candidate of one ambiguous row with bounded reply size.
    pub fn vendor_import_candidates(
        &self,
        id: &str,
        row_id: &str,
        offset: u64,
        limit: u64,
    ) -> Result<VendorCandidatePage> {
        let row = crate::util::encode_path_segment(row_id);
        vendor_json(&self.vendor_http(
            "GET",
            &format!(
                "/{}/candidates?row_id={row}&offset={offset}&limit={limit}",
                crate::util::encode_path_segment(id)
            ),
            None,
            None,
            None,
        )?)
    }
    /// Read reusable column mappings.
    pub fn vendor_import_templates(&self) -> Result<Vec<VendorMappingTemplate>> {
        vendor_json(&self.vendor_http("GET", "/templates", None, None, None)?)
    }
    /// Save an explicitly selected reusable column mapping.
    pub fn vendor_import_save_template(
        &self,
        template: &VendorMappingTemplate,
    ) -> Result<VendorMappingTemplate> {
        let bytes =
            serde_json::to_vec(template).map_err(|e| RemoteError::Validation(e.to_string()))?;
        vendor_json(&self.vendor_http(
            "PUT",
            "/templates",
            Some("application/json".into()),
            Some(&bytes),
            None,
        )?)
    }
    /// Request cancellation at a committed import batch boundary.
    pub fn vendor_import_cancel(&self, id: &str) -> Result<VendorImportStatus> {
        vendor_json(&self.vendor_call(
            &format!("/{}/cancel", crate::util::encode_path_segment(id)),
            None,
            None,
            None,
        )?)
    }

    /// `POST /vendor-import/{id}/commit`; replaying the same key returns the same result.
    pub fn vendor_import_commit(&self, import_id: &str, key: &str) -> Result<VendorImportResult> {
        let id = crate::util::encode_path_segment(import_id);
        let text = self.vendor_call(&format!("/{id}/commit"), None, None, Some(key))?;
        serde_json::from_str(&text)
            .map_err(|error| RemoteError::Protocol(format!("vendor commit: {error}")))
    }

    /// `POST /vendor-import/{id}/rollback`.
    pub fn vendor_import_rollback(&self, import_id: &str) -> Result<VendorImportResult> {
        let id = crate::util::encode_path_segment(import_id);
        let text = self.vendor_call(&format!("/{id}/rollback"), None, None, None)?;
        serde_json::from_str(&text)
            .map_err(|error| RemoteError::Protocol(format!("vendor rollback: {error}")))
    }
}

fn vendor_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text)
        .map_err(|e| RemoteError::Protocol(format!("Vendor import reply: {e}")))
}

fn vendor_prefix(configured: Option<&str>) -> Result<&str> {
    let path = configured
        .unwrap_or(VENDOR_IMPORT_PATH)
        .trim_end_matches('/');
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.len() < 2
        || path.split('/').any(|part| matches!(part, "." | ".."))
        || !path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/-_.".contains(&byte))
    {
        return Err(RemoteError::Validation(
            "Invalid same-origin vendor import endpoint".into(),
        ));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_is_origin_relative_and_cannot_redirect_credentials() {
        assert_eq!(vendor_prefix(None).unwrap(), "/api/vendor-import");
        assert_eq!(
            vendor_prefix(Some("/custom/import/")).unwrap(),
            "/custom/import"
        );
        for path in [
            "https://example.invalid/import",
            "//example.invalid/import",
            "/api/../admin",
            "/api/%2f",
            "/api?token=x",
            "",
        ] {
            assert!(vendor_prefix(Some(path)).is_err(), "{path}");
        }
    }

    #[test]
    fn migration_errors_keep_their_type_and_delay_for_ui_retries() {
        use audiolib_core::{Error, server_upgrade::MigrationRefusal};
        for (code, refusal) in [
            (
                "migration_in_progress",
                MigrationRefusal::PathSearchInProgress,
            ),
            ("migration_switching", MigrationRefusal::Switching),
        ] {
            let error = RemoteError::from_response(
                503,
                &format!(r#"{{"error_code":"{code}","message":"wait"}}"#),
                "POST /api/v1/search",
                Some(30),
            );
            assert!(matches!(crate::writes::core_error(error, true),
                Error::Migration { refusal: actual, retry_after: 30 } if actual == refusal));
        }
    }

    #[test]
    fn import_routes_preserve_library_identity_key_and_binary_upload() {
        use crate::test_server::{TestResponse, TestServer, marker_capabilities, marker_health};
        for prefix in [VENDOR_IMPORT_PATH, "/custom/import"] {
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = seen.clone();
            let server = TestServer::start(move |request| {
                match request.path.as_str() {
                    "/api/v1/health" => return marker_health(),
                    "/api/v1/capabilities" => {
                        return marker_capabilities(serde_json::json!({
                            "can_manage": true,
                            "vendor_import": {"revision":1, "endpoint":prefix, "max_file_bytes":52428800, "max_rows":200000}
                        }));
                    }
                    "/api/v1/library" => {
                        return TestResponse::json(
                            200,
                            r#"{"library_id":"fixture","identity_bound_writes":true,"schema_version":5,"revision":1}"#,
                        );
                    }
                    _ => {}
                }
                assert_eq!(request.header(crate::LIBRARY_ID_HEADER), Some("fixture"));
                assert_eq!(
                    request.header("authorization"),
                    Some("Bearer fixture-token")
                );
                assert_eq!(request.method, "POST");
                recorded.lock().unwrap().push(request.path.clone());
                if request.path == format!("{prefix}/preview") {
                    assert!(request.body.windows(7).any(|bytes| bytes == b"a,b\n1,2"));
                    TestResponse::json(200, r#"{"import_id":"batch/1","rows":1,"matched":1}"#)
                } else if request.path == format!("{prefix}/batch%2F1/commit") {
                    assert_eq!(request.header("idempotency-key"), Some("stable-key"));
                    TestResponse::json(200, r#"{"import_id":"batch/1","written":1}"#)
                } else {
                    assert_eq!(request.path, format!("{prefix}/batch%2F1/rollback"));
                    TestResponse::json(200, r#"{"import_id":"batch/1","restored":1}"#)
                }
            });
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("vendor.csv");
            std::fs::write(&file, b"a,b\n1,2").unwrap();
            let client = RemoteClient::new(&server.url, "fixture-token")
                .unwrap()
                .with_library_id("fixture")
                .unwrap();
            let preview = client.vendor_import_preview(&file).unwrap();
            assert_eq!(preview.matched, 1);
            for _ in 0..2 {
                assert_eq!(
                    client
                        .vendor_import_commit(&preview.import_id, "stable-key")
                        .unwrap()
                        .written,
                    1
                );
            }
            assert_eq!(
                client
                    .vendor_import_rollback(&preview.import_id)
                    .unwrap()
                    .restored,
                1
            );
            assert_eq!(seen.lock().unwrap().len(), 4);
        }
    }

    #[test]
    fn multipart_body_wraps_one_file_part_and_sanitizes_name() {
        let body = multipart_body("b", "a\"b\r\n.csv", b"x,y");
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with("--b\r\n"));
        assert!(text.contains("filename=\"a_b__.csv\""));
        assert!(text.ends_with("\r\nx,y\r\n--b--\r\n"));
    }
}
