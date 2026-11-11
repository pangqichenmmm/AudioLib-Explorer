// server/audiolib-server/src/vendor_import.rs
//! Managed, library-bound vendor import API with bounded streaming uploads.
use crate::{ApiError, AppState, Service};
use audiolib_core::{DataProvider, vendor_import::*};
use axum::{
    Extension, Json, Router,
    extract::FromRequest,
    extract::multipart::MultipartRejection,
    extract::rejection::{JsonRejection, QueryRejection},
    extract::{DefaultBodyLimit, Multipart, Path, Query, Request},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

mod server_path;

pub(crate) const UPLOAD_LIMIT: usize = 512 * 1024 * 1024;
const MAX_FILES: usize = 512;

struct ImportRevisionHold {
    state: AppState,
    active: bool,
}

impl ImportRevisionHold {
    fn begin(state: &AppState) -> audiolib_core::Result<Self> {
        state.tracker().begin_hold()?;
        Ok(Self {
            state: state.clone(),
            active: true,
        })
    }

    fn finish(
        mut self,
        result: audiolib_core::Result<VendorImportResult>,
    ) -> audiolib_core::Result<VendorImportResult> {
        let changed = match &result {
            Ok(result) => result.written != 0 || result.restored != 0,
            Err(_) => true,
        };
        self.active = false;
        let revision = self.state.tracker().end_hold(changed);
        let value = result?;
        revision?;
        Ok(value)
    }
}

impl Drop for ImportRevisionHold {
    fn drop(&mut self) {
        if self.active {
            let _ = self.state.tracker().end_hold(true);
        }
    }
}

pub(crate) fn router() -> Router<Service> {
    Router::new()
        .route("/", get(history))
        .route(
            "/preview",
            post(preview).layer(DefaultBodyLimit::max(UPLOAD_LIMIT)),
        )
        .route("/templates", get(templates).put(save_template))
        .route("/{id}", get(status))
        .route("/{id}/rows", get(rows))
        .route("/{id}/candidates", get(candidates))
        .route("/{id}/mapping", axum::routing::put(mapping))
        .route("/{id}/commit", post(commit))
        .route("/{id}/cancel", post(cancel))
        .route("/{id}/rollback", post(rollback))
        .layer(DefaultBodyLimit::max(1024 * 1024))
}

fn bound_library(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let mut ids = headers.get_all("x-audiolib-library-id").iter();
    let id = ids.next().and_then(|v| v.to_str().ok());
    if id != Some(state.library_id()) || ids.next().is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "LIBRARY_ID_REQUIRED",
            "Vendor import requires the currently opened library identity",
        ));
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> audiolib_core::Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ApiError::internal(format!("Vendor import task failed: {e}")))?
        .map_err(ApiError::from)
}

fn upload_error(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "VALIDATION", message)
}

async fn preview(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    request: Request,
) -> Result<Json<VendorImportPreview>, ApiError> {
    bound_library(&state, &headers)?;
    if headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|_| {
                upload_error("Server-path options exceed 1 MiB or have an invalid body")
            })?;
        let input =
            serde_json::from_slice(&body).map_err(|error| upload_error(error.to_string()))?;
        return Ok(Json(server_path::preview(state, input).await?));
    }
    let multipart: Result<Multipart, MultipartRejection> =
        Multipart::from_request(request, &()).await;
    let multipart =
        multipart.map_err(|e| ApiError::new(e.status(), "VALIDATION", e.body_text()))?;
    preview_upload(state, multipart).await
}

async fn preview_upload(
    state: AppState,
    mut multipart: Multipart,
) -> Result<Json<VendorImportPreview>, ApiError> {
    let directory = tempfile::tempdir()
        .map_err(audiolib_core::Error::from)
        .map_err(ApiError::from)?;
    let mut files = Vec::new();
    let mut options = VendorImportOptions::default();
    let mut saw_options = false;
    let mut total = 0_u64;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| upload_error(e.to_string()))?
    {
        if field.name() == Some("options") {
            if saw_options {
                return Err(upload_error("Duplicate vendor import options"));
            }
            saw_options = true;
            let mut bytes = Vec::new();
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|e| upload_error(e.to_string()))?
            {
                if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
                    return Err(upload_error("Vendor import options exceed 1 MiB"));
                }
                bytes.extend_from_slice(&chunk);
            }
            options = serde_json::from_slice(&bytes).map_err(|e| upload_error(e.to_string()))?;
            continue;
        }
        if !matches!(field.name(), Some("file" | "files")) {
            return Err(upload_error("Expected a vendor table file or options"));
        }
        if files.len() >= MAX_FILES {
            return Err(upload_error("One preview accepts at most 512 files"));
        }
        let name = field.file_name().unwrap_or("vendor.csv").to_owned();
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.chars().any(|c| {
                matches!(c, '/' | '\\' | ':' | '<' | '>' | '|' | '?' | '*') || c.is_control()
            })
        {
            return Err(upload_error("Invalid vendor file name"));
        }
        let parent = directory.path().join(files.len().to_string());
        tokio::fs::create_dir(&parent)
            .await
            .map_err(audiolib_core::Error::from)
            .map_err(ApiError::from)?;
        let path = parent.join(name);
        let mut output = tokio::fs::File::create(&path)
            .await
            .map_err(audiolib_core::Error::from)
            .map_err(ApiError::from)?;
        let mut length = 0_u64;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| upload_error(e.to_string()))?
        {
            total = total.saturating_add(chunk.len() as u64);
            if total > UPLOAD_LIMIT as u64 {
                return Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "UPLOAD_TOO_LARGE",
                    "Vendor upload batch exceeds 512 MiB",
                ));
            }
            // Retain at most max+1 bytes so the parser records a per-file oversize
            // refusal while other files in the same bounded request can proceed.
            let remaining =
                (VendorLimits::default().max_file_bytes + 1).saturating_sub(length) as usize;
            let take = chunk.len().min(remaining);
            if take != 0 {
                output
                    .write_all(&chunk[..take])
                    .await
                    .map_err(audiolib_core::Error::from)
                    .map_err(ApiError::from)?;
            }
            length = length.saturating_add(chunk.len() as u64);
        }
        output
            .flush()
            .await
            .map_err(audiolib_core::Error::from)
            .map_err(ApiError::from)?;
        drop(output);
        files.push(path);
    }
    if files.is_empty() {
        return Err(upload_error("Select at least one vendor table"));
    }
    let result = blocking(move || {
        let _keep_uploads = directory;
        state
            .provider()
            .vendor_import_preview_files(&files, &options)
    })
    .await?;
    Ok(Json(result))
}

async fn status(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<VendorImportStatus>, ApiError> {
    bound_library(&state, &headers)?;
    Ok(Json(
        blocking(move || state.provider().vendor_import_status(&id)).await?,
    ))
}

#[derive(Default, Deserialize)]
struct RowPage {
    #[serde(default)]
    offset: u64,
    limit: Option<u64>,
}

async fn rows(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    page: Result<Query<RowPage>, QueryRejection>,
) -> Result<Json<Vec<VendorRowPreview>>, ApiError> {
    bound_library(&state, &headers)?;
    let Query(page) = page.map_err(|e| ApiError::new(e.status(), "VALIDATION", e.body_text()))?;
    let limit = page.limit.unwrap_or(100);
    if limit == 0 || limit > 1000 {
        return Err(upload_error("Preview row limit must be 1..1000"));
    }
    Ok(Json(
        blocking(move || state.provider().vendor_import_rows(&id, page.offset, limit)).await?,
    ))
}

async fn mapping(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    options: Result<Json<VendorImportOptions>, JsonRejection>,
) -> Result<Json<VendorImportPreview>, ApiError> {
    bound_library(&state, &headers)?;
    let Json(options) =
        options.map_err(|e| ApiError::new(e.status(), "VALIDATION", e.body_text()))?;
    Ok(Json(
        blocking(move || state.provider().vendor_import_mapping(&id, &options)).await?,
    ))
}

#[derive(Deserialize)]
struct CandidatePage {
    row_id: String,
    #[serde(default)]
    offset: u64,
    limit: Option<u64>,
}

async fn candidates(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    page: Result<Query<CandidatePage>, QueryRejection>,
) -> Result<Json<VendorCandidatePage>, ApiError> {
    bound_library(&state, &headers)?;
    let Query(page) = page.map_err(|e| ApiError::new(e.status(), "VALIDATION", e.body_text()))?;
    let limit = page.limit.unwrap_or(32);
    if limit == 0 || limit > 100 {
        return Err(upload_error("Candidate page limit must be 1..100"));
    }
    Ok(Json(
        blocking(move || {
            state
                .provider()
                .vendor_import_candidates(&id, &page.row_id, page.offset, limit)
        })
        .await?,
    ))
}

pub(crate) async fn history(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
) -> Result<Json<VendorImportHistory>, ApiError> {
    bound_library(&state, &headers)?;
    Ok(Json(
        blocking(move || state.provider().vendor_import_history()).await?,
    ))
}

async fn templates(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<VendorMappingTemplate>>, ApiError> {
    bound_library(&state, &headers)?;
    Ok(Json(
        blocking(move || state.provider().vendor_import_templates()).await?,
    ))
}

async fn save_template(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    template: Result<Json<VendorMappingTemplate>, JsonRejection>,
) -> Result<Json<VendorMappingTemplate>, ApiError> {
    bound_library(&state, &headers)?;
    let Json(template) =
        template.map_err(|e| ApiError::new(e.status(), "VALIDATION", e.body_text()))?;
    Ok(Json(
        blocking(move || state.provider().vendor_import_save_template(&template)).await?,
    ))
}

async fn cancel(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<VendorImportStatus>, ApiError> {
    bound_library(&state, &headers)?;
    Ok(Json(
        blocking(move || state.provider().vendor_import_cancel(&id)).await?,
    ))
}

async fn commit(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<VendorImportResult>, ApiError> {
    bound_library(&state, &headers)?;
    let mut keys = headers.get_all("idempotency-key").iter();
    let key = keys
        .next()
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty() && v.len() <= 200)
        .ok_or_else(|| upload_error("Commit requires Idempotency-Key"))?
        .to_owned();
    if keys.next().is_some() {
        return Err(upload_error("Duplicate Idempotency-Key"));
    }
    Ok(Json(
        blocking(move || {
            let before = state.provider().vendor_import_status(&id)?;
            if before
                .result
                .as_ref()
                .is_some_and(|r| r.state.as_deref() == Some("committed"))
            {
                return state.provider().vendor_import_commit(&id, &key);
            }
            let hold = ImportRevisionHold::begin(&state)?;
            hold.finish(state.provider().vendor_import_commit(&id, &key))
        })
        .await?,
    ))
}

async fn rollback(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<VendorImportResult>, ApiError> {
    bound_library(&state, &headers)?;
    Ok(Json(
        blocking(move || {
            let before = state.provider().vendor_import_status(&id)?;
            if before
                .result
                .as_ref()
                .is_some_and(|r| r.state.as_deref() == Some("rolled_back"))
            {
                return state.provider().vendor_import_rollback(&id);
            }
            let hold = ImportRevisionHold::begin(&state)?;
            hold.finish(state.provider().vendor_import_rollback(&id))
        })
        .await?,
    ))
}
