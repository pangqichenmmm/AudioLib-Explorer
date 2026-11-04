// server/audiolib-server/src/routes.rs
//! `/api/v1` routing, bearer authentication and endpoint handlers.
use crate::{
    ApiError, AppState, VERSION,
    auth::{ConnectionInfo, RequestOrigin, same_request_origin},
    list_waveform::{self, BatchRequest, ListWaveformsCapability, SettingsResponse},
    manage::{
        AddRootRequest, CapabilitiesResponse, RemoveAudioRequest, RemoveAudioResponse,
        RemoveFolderResponse, RootPathRequest,
    },
    pcm::{self, PcmPlan, Renderer, WavError},
    preview::{self, Codec, FlacPlan, FlacStream, PreviewRequest, SourceFormat},
    scans::{CurrentScanResponse, ScanAction, ScanRequest, ScanResponse},
    service::{FAILED_MESSAGE, Readiness, Service},
    state::{
        AudioWriteResponse, BatchWriteResponse, OpenError, OpenProgress, SLOW_OPEN_WARNING,
        ServedAudio, SnapshotEncoding, WaveformReply,
    },
    stream::{self, ByteRange},
    waveform::WaveformParams,
};
use audiolib_core::{
    AudioFile, BatchMetadataRequest, DataProvider, MetadataPatch, ScanSession, TagEditMode,
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{
        DefaultBodyLimit, Extension, Path, Query, Request, State,
        rejection::{BytesRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
};
use futures_core::Stream;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    convert::Infallible,
    io::{self, SeekFrom},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use tower_http::{
    timeout::TimeoutLayer,
    trace::{DefaultOnFailure, TraceLayer},
};

/// Base path of every endpoint.
pub const API_BASE: &str = "/api/v1";
/// Read size for streaming snapshot and audio files.
const STREAM_CHUNK_BYTES: usize = 256 * 1024;
/// Bundle download limits are independent of its short preparation lease.
static BUNDLE_DOWNLOAD_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);
const BUNDLE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Time limit for every endpoint except `/library/snapshot`, `/audio/{id}/stream`,
/// `/audio/{id}/preview`, `/audio/{id}/waveform`, `/audio/{id}/artwork` and
/// `/albums/{id}/artwork`, which
/// legitimately take long (large bodies, waking disks, decoding); exceeding it yields
/// 408 `TIMEOUT`.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest accepted JSON request body (a 1 MiB note may double when JSON-escaped).
const MAX_JSON_BODY_BYTES: usize = 8 * 1024 * 1024;

/// This request passed bearer and management checks against one fresh settings
/// snapshot. Later policy changes do not withdraw an already admitted request.
#[derive(Clone, Copy)]
struct ManagedAdmission;

/// `GET /health` reply (always HTTP 200 while the process serves requests).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    /// Compatibility catalog semantics of this build, independent of the library.
    #[serde(default)]
    pub compatibility_catalog_revision: u32,
    /// Startup maintenance phase and committed batch counts, omitted when ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<audiolib_db::SnapshotUpgradeProgress>,
    /// `ok` (library open), `starting` (still opening) or `error` (open failed, retrying).
    pub status: String,
    /// Server crate version.
    pub version: String,
    /// Short reason for `error`; omitted otherwise. Never contains paths or secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Public base URL for clients outside the LAN (admin page or `config.toml`
    /// `public_url`); always present, `null` when not configured.
    #[serde(default)]
    pub public_url: Option<String>,
    /// API version (`v1`, the `/api/v1/` path prefix).
    #[serde(default)]
    pub api_version: String,
    /// Library schema version this build supports (`audiolib_db::SCHEMA_VERSION`).
    #[serde(default)]
    pub schema_version: i64,
    /// Short git commit of the build, `unknown` when it could not be determined.
    #[serde(default)]
    pub commit: String,
}

/// Build the application router. `/health` is public; everything else is authorized per
/// the authentication mode (`crate::auth`). `/capabilities` only needs that
/// authorization; every other route also needs an open library (503 `UNAVAILABLE`
/// before that). Library management (scans and their controls, `POST /audio/remove`,
/// `DELETE /folders/{id}`) is additionally gated by `manage_mode` (403
/// `MANAGE_FORBIDDEN`, checked after authentication). The LAN-only admin page lives at
/// `/admin` (`crate::admin`). Accepts an open [`AppState`] or a [`Service`] that may still
/// be starting. Request tracing records method, URI and status only, never request
/// headers. All routes except the snapshot, audio stream, preview, waveform, embedded
/// cover and `POST /scans` are limited to [`REQUEST_TIMEOUT`].
pub fn router(service: impl Into<Service>) -> Router {
    router_with_request_timeout(service, REQUEST_TIMEOUT)
}

/// [`router`] with another limit for the timed routes (tests use a short one to show that
/// the stream and snapshot routes are not limited at all).
#[doc(hidden)]
pub fn router_with_request_timeout(service: impl Into<Service>, timeout: Duration) -> Router {
    let state: Service = service.into();
    // Library management (scans, removals) is additionally gated by `manage_mode`; the
    // layer sits inside the authentication, so 401 comes before 403.
    let manage = || middleware::from_fn_with_state(state.clone(), require_manage);
    let timed_protected = with_request_timeout(
        timeout,
        Router::new()
            .route("/roots", get(roots))
            .route("/fs", get(browse_fs).route_layer(manage()))
            .route("/roots", post(add_root).route_layer(manage()))
            .route("/roots/{name}/check", post(check_root))
            .route("/audio/{id}", get(audio))
            .route("/audio/{id}/metadata", patch(update_metadata))
            .route("/audio/metadata/batch", post(update_metadata_batch))
            .route("/audio/{id}/plays", post(record_play))
            .route("/audio/remove", post(remove_audio).route_layer(manage()))
            .route("/folders/{id}", delete(remove_folder).route_layer(manage()))
            .route("/history/scans", get(scan_history))
            .route("/history/plays", get(play_history))
            .route("/scans/current", get(current_scan))
            .route("/scans/{session_id}", get(scan_status))
            .route(
                "/scans/{session_id}/pause",
                post(pause_scan).route_layer(manage()),
            )
            .route(
                "/scans/{session_id}/resume",
                post(resume_scan).route_layer(manage()),
            )
            .route(
                "/scans/{session_id}/cancel",
                post(cancel_scan).route_layer(manage()),
            )
            // List thumbnail waveforms (doc/api/README.md §9.1–§9.2).
            .route(
                "/waveforms",
                post(list_waveforms_batch)
                    .route_layer(middleware::map_response(vary_accept_encoding)),
            )
            .route("/waveforms/status", get(list_waveform_status))
            .route(
                "/waveforms/pregeneration/pause",
                post(pause_pregeneration).route_layer(manage()),
            )
            .route(
                "/waveforms/pregeneration/resume",
                post(resume_pregeneration).route_layer(manage()),
            )
            // List waveform cache jobs (doc/api/README.md §9.5).
            .route(
                "/waveforms/jobs",
                post(start_list_waveform_job).route_layer(manage()),
            )
            .route(
                "/waveforms/jobs/current/pause",
                post(pause_list_waveform_job).route_layer(manage()),
            )
            .route(
                "/waveforms/jobs/current/resume",
                post(resume_list_waveform_job).route_layer(manage()),
            )
            .route(
                "/waveforms/jobs/current/stop",
                post(stop_list_waveform_job).route_layer(manage()),
            )
            .layer(DefaultBodyLimit::max(MAX_JSON_BODY_BYTES)),
    );
    let protected = timed_protected
        // A populated root relocation validates all indexed targets and creates a
        // rollback backup before a single atomic commit. It may take longer than
        // 30 seconds; dropping the HTTP request cancels before that commit.
        .route(
            "/roots/{name}",
            patch(update_root_path)
                .delete(remove_root)
                .route_layer(manage()),
        )
        // Not time-limited: on a sleeping disk the scan directory may take long to
        // answer before the session (and its id) exists (doc/api/README.md §6.6).
        .route(
            "/scans",
            post(start_scan)
                .layer(DefaultBodyLimit::max(MAX_JSON_BODY_BYTES))
                .route_layer(manage()),
        )
        // Not time-limited either: on a million-row library the first request after a
        // restart may still wait for the statistics (doc/api/README.md §6.1).
        .route("/library", get(library))
        .route(
            "/library/snapshot",
            get(snapshot).route_layer(middleware::map_response(vary_accept_encoding)),
        )
        // Not time-limited: VACUUM INTO of ~900k rows plus a ~230 MB body (§9.4.2).
        .route("/waveforms/bundle", get(waveform_bundle))
        .route("/audio/{id}/stream", get(audio_stream))
        .route("/audio/{id}/preview", get(audio_preview))
        .route(
            "/audio/{id}/waveform",
            get(audio_waveform).route_layer(middleware::map_response(vary_accept_encoding)),
        )
        .route("/audio/{id}/artwork", get(audio_artwork))
        .route("/albums/{id}/artwork", get(album_artwork))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));
    // `/capabilities` needs authentication but not the library, so it also answers
    // while the library is still opening.
    let open = Router::new().route("/health", get(health)).merge(
        Router::new()
            .route("/capabilities", get(capabilities))
            // Server settings clients may read and (with `manage_mode`) change; like
            // `/capabilities` they answer while the library is still opening.
            .route("/settings", get(get_settings).patch(patch_settings))
            .route_layer(middleware::from_fn_with_state(state.clone(), require_token)),
    );
    let api = with_request_timeout(timeout, open)
        .merge(protected)
        .layer(middleware::from_fn(guard_api_mutation));
    Router::new()
        .nest(API_BASE, api)
        .merge(with_request_timeout(
            timeout,
            crate::admin::router(state.security().clone()),
        ))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        // `ApiError` already logs every 5xx it produces (503 `UNAVAILABLE` at debug); the
        // default ERROR-level failure event would duplicate it on every client poll.
        .layer(
            TraceLayer::new_for_http()
                .on_failure(DefaultOnFailure::new().level(tracing::Level::DEBUG)),
        )
        .with_state(state)
}

/// Check the bearer token, then attach the open [`AppState`] as a request extension.
/// Apply `timeout` to the routes added so far, answering 408 `TIMEOUT` as JSON.
fn with_request_timeout<S: Clone + Send + Sync + 'static>(
    timeout: Duration,
    router: Router<S>,
) -> Router<S> {
    router
        .route_layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            timeout,
        ))
        .route_layer(middleware::map_response(timeout_as_json))
}

/// Replace tower-http's empty 408 body with the JSON error shape.
async fn timeout_as_json(response: Response) -> Response {
    if response.status() == StatusCode::REQUEST_TIMEOUT
        && !response.headers().contains_key(header::CONTENT_TYPE)
    {
        ApiError::timeout().into_response()
    } else {
        response
    }
}

/// Authorize the request per the authentication mode (401 `UNAUTHORIZED` when a token is
/// needed and missing or invalid; requests that need none ignore `Authorization`), then
/// attach the open [`AppState`].
async fn require_bearer(
    State(service): State<Service>,
    mut request: Request,
    next: Next,
) -> Response {
    let connection = request.extensions().get::<ConnectionInfo>().copied();
    let managed = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .is_some_and(|path| managed_route(request.method(), path.as_str()));
    let authorized = if managed {
        // Delegate before the old in-memory bearer check: another instance may
        // have either tightened or relaxed both policies since this one loaded them.
        authorize_managed(
            service.security().clone(),
            request.headers().clone(),
            connection,
        )
        .await
    } else {
        // Ordinary reads, streams and non-management writes keep their existing
        // admission path; a long root edit must not make them wait for its lease.
        service
            .security()
            .authorize(request.headers(), connection.as_ref())
    };
    if let Err(error) = authorized {
        return error.into_response();
    }
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) && let Err(error) = crate::compatibility::admit_mutation(request.headers())
    {
        return error.into_response();
    }
    if managed {
        request.extensions_mut().insert(ManagedAdmission);
    }
    match service.app_state() {
        Ok(state) => {
            // This cloned AppState pins the provider and its immutable library identity
            // for the whole request, even if Service installs a different AppState.
            if library_mutation(&request) {
                let values = request.headers().get_all(LIBRARY_ID_HEADER);
                let mut values = values.iter();
                let id = values.next().and_then(|value| value.to_str().ok());
                let id = id.filter(|id| !id.trim().is_empty() && values.next().is_none());
                let Some(id) = id else {
                    return ApiError::new(
                        StatusCode::CONFLICT,
                        "LIBRARY_ID_REQUIRED",
                        "Library mutations require X-AudioLib-Library-Id from the opened snapshot",
                    )
                    .into_response();
                };
                if state.library_id() != id {
                    return ApiError::new(
                        StatusCode::CONFLICT,
                        "LIBRARY_CHANGED",
                        "The server library changed; synchronize before writing",
                    )
                    .into_response();
                }
                // The historical playback route only reads an existing row. Every
                // real library mutation requires a modern peer that can consume the
                // actual complete format, including retired DTO fields/features.
                if !legacy_playback_mutation(&request) {
                    let inspecting = state.clone();
                    let format = match blocking(move || inspecting.snapshot_format()).await {
                        Ok(format) => format,
                        Err(error) => return error.into_response(),
                    };
                    if let Err(error) =
                        crate::compatibility::admit_library_mutation(request.headers(), &format)
                    {
                        return error.into_response();
                    }
                }
            }
            request.extensions_mut().insert(state);
            next.run(request).await
        }
        Err(error) => error.into_response(),
    }
}

/// Snapshot identity required by protected library mutations.
pub const LIBRARY_ID_HEADER: &str = "x-audiolib-library-id";

/// Exact methods of every client route carrying `require_manage`, including HEAD
/// for the GET filesystem browser. Match the router's path template, never a raw URI.
fn managed_route(method: &Method, path: &str) -> bool {
    match *method {
        Method::GET | Method::HEAD => path.ends_with("/fs"),
        Method::PATCH => path.ends_with("/roots/{name}") || path.ends_with("/settings"),
        Method::DELETE => path.ends_with("/folders/{id}") || path.ends_with("/roots/{name}"),
        Method::POST => [
            "/roots",
            "/audio/remove",
            "/scans",
            "/scans/{session_id}/pause",
            "/scans/{session_id}/resume",
            "/scans/{session_id}/cancel",
            "/waveforms/pregeneration/pause",
            "/waveforms/pregeneration/resume",
            "/waveforms/jobs",
            "/waveforms/jobs/current/pause",
            "/waveforms/jobs/current/resume",
            "/waveforms/jobs/current/stop",
        ]
        .iter()
        .any(|managed| path.ends_with(*managed)),
        _ => false,
    }
}

/// Managed API admission owns the settings lease only while reloading and checking
/// bearer then management policy. The settings file and secret headers stay on the
/// blocking worker; admitted operations need not be revoked by later policy changes.
async fn authorize_managed(
    security: Arc<crate::Security>,
    headers: HeaderMap,
    connection: Option<ConnectionInfo>,
) -> Result<(), ApiError> {
    blocking_api(move || {
        let _lease = security.settings_lease()?;
        security.authorize(&headers, connection.as_ref())?;
        security.authorize_manage(&headers, connection.as_ref())
    })
    .await
}

fn library_mutation(request: &Request) -> bool {
    let Some(path) = request.extensions().get::<axum::extract::MatchedPath>() else {
        return false;
    };
    let path = path.as_str();
    match *request.method() {
        Method::PATCH => path.ends_with("/audio/{id}/metadata") || path.ends_with("/roots/{name}"),
        Method::DELETE => path.ends_with("/folders/{id}") || path.ends_with("/roots/{name}"),
        Method::POST => [
            "/roots",
            "/audio/metadata/batch",
            "/audio/{id}/plays",
            "/audio/remove",
            "/scans",
            "/scans/{session_id}/pause",
            "/scans/{session_id}/resume",
            "/scans/{session_id}/cancel",
        ]
        .iter()
        .any(|mutation| path.ends_with(*mutation)),
        _ => false,
    }
}

/// Match only the deprecated no-op endpoint, never a caller-controlled URI suffix.
fn legacy_playback_mutation(request: &Request) -> bool {
    request.method() == Method::POST
        && request
            .extensions()
            .get::<axum::extract::MatchedPath>()
            .is_some_and(|path| path.as_str().ends_with("/audio/{id}/plays"))
}

/// Native clients omit Origin. Browser mutations must prove the same authority and
/// listener scheme; forwarded headers never establish a trusted browser origin.
async fn guard_api_mutation(request: Request, next: Next) -> Response {
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) && !same_request_origin(&request)
    {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "ORIGIN_FORBIDDEN",
            "Browser mutations require the server's own origin",
        )
        .into_response();
    }
    next.run(request).await
}

/// Authorize the request per the authentication mode only (no library needed).
async fn require_token(State(service): State<Service>, request: Request, next: Next) -> Response {
    let connection = request.extensions().get::<ConnectionInfo>().copied();
    match service
        .security()
        .authorize(request.headers(), connection.as_ref())
    {
        Ok(()) => next.run(request).await,
        Err(error) => error.into_response(),
    }
}

/// 403 `MANAGE_FORBIDDEN` unless `manage_mode` allows this (authorized) request to
/// manage the library.
async fn require_manage(State(service): State<Service>, request: Request, next: Next) -> Response {
    if request.extensions().get::<ManagedAdmission>().is_none() {
        // Defensive admission for a management route added without updating the
        // outer predicate. Existing managed requests were checked before library ID.
        let connection = request.extensions().get::<ConnectionInfo>().copied();
        if let Err(error) = authorize_managed(
            service.security().clone(),
            request.headers().clone(),
            connection,
        )
        .await
        {
            return error.into_response();
        }
    }
    next.run(request).await
}

/// `GET /capabilities`: the effective `manage_mode`, whether this request counts as
/// external and whether it may manage the library.
async fn capabilities(
    State(service): State<Service>,
    request: Request,
) -> Json<CapabilitiesResponse> {
    let connection = request.extensions().get::<ConnectionInfo>().copied();
    let origin = service
        .security()
        .classify(request.headers(), connection.as_ref());
    let manage_mode = service.security().manage_mode();
    Json(CapabilitiesResponse {
        compatibility: audiolib_core::compatibility::compatibility_manifest(),
        manage_mode,
        external: origin == RequestOrigin::External,
        can_manage: manage_mode.allows(origin),
        root_management: true,
        list_waveforms: Some(ListWaveformsCapability {
            // While the library is opening the bundle endpoint answers 503 UNAVAILABLE
            // (retry); once open it is advertised only when waveforms.db is usable.
            bundle: match service.readiness() {
                Readiness::Ready(state) => state.list_waveforms().wfdb().is_some(),
                _ => true,
            },
            ..ListWaveformsCapability::default()
        }),
        pregenerate_list_waveforms: service.security().pregenerate_list_waveforms(),
        // Not advertised once the library is open without a usable waveforms.db (the job
        // endpoints answer 503 UNAVAILABLE then); while it opens they answer 503 too
        // (retry), as for the bundle.
        list_waveform_jobs: match service.readiness() {
            Readiness::Ready(state) if !state.list_waveforms().jobs_available() => None,
            _ => Some(crate::wf_job::ListWaveformJobsCapability::default()),
        },
    })
}

fn settings_response(
    security: &crate::Security,
    headers: &HeaderMap,
    connection: Option<&ConnectionInfo>,
) -> SettingsResponse {
    let (pregenerate_list_waveforms, pregenerate_list_waveforms_source) =
        security.pregenerate_list_waveforms_with_source();
    let (list_waveform_rate, list_waveform_rate_source) = security.list_waveform_rate_with_source();
    SettingsResponse {
        pregenerate_list_waveforms,
        pregenerate_list_waveforms_source,
        list_waveform_rate,
        list_waveform_rate_source,
        can_change: security
            .manage_mode()
            .allows(security.classify(headers, connection)),
    }
}

/// `GET /settings` (doc/api/README.md §9.2): not gated by `manage_mode`.
async fn get_settings(
    State(service): State<Service>,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
) -> Json<SettingsResponse> {
    let connection = extensions.get::<ConnectionInfo>().copied();
    Json(settings_response(
        service.security(),
        &headers,
        connection.as_ref(),
    ))
}

/// `PATCH /settings` body: only the present fields change; `null` removes the override.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsPatch {
    #[serde(default, deserialize_with = "present_value")]
    pregenerate_list_waveforms: Option<Option<bool>>,
    #[serde(default, deserialize_with = "present_value")]
    list_waveform_rate: Option<Option<u32>>,
}

/// `Some(None)` for an explicit JSON `null`, `None` for an absent field.
fn present_value<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Ok(Some(Option::deserialize(deserializer)?))
}

/// `PATCH /settings`: inside the settings lease (409 `BUSY` while another instance holds
/// it) authenticate, check `manage_mode` (403 `MANAGE_FORBIDDEN`), then validate and
/// write `admin-settings.json`. Takes effect at once.
async fn patch_settings(
    State(service): State<Service>,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<SettingsResponse>, ApiError> {
    let connection = extensions.get::<ConnectionInfo>().copied();
    let security = service.security().clone();
    let reply = blocking_api(move || {
        let lease = security.settings_lease()?;
        security.authorize(&headers, connection.as_ref())?;
        security.authorize_manage(&headers, connection.as_ref())?;
        // Settings remains available during library startup, but its writes use
        // optional modern-peer validation; it does not return library DTOs.
        crate::compatibility::admit_mutation(&headers)?;
        let value: serde_json::Value = json_body(&headers, body)?;
        if !value.is_object() {
            return Err(ApiError::validation(
                "The request body must be a JSON object",
            ));
        }
        let patch: SettingsPatch = serde_json::from_value(value)
            .map_err(|error| ApiError::validation(format!("Invalid settings: {error}")))?;
        if let Some(Some(rate)) = patch.list_waveform_rate
            && !(1..=1000).contains(&rate)
        {
            return Err(ApiError::validation(
                "list_waveform_rate must be an integer in 1..=1000 or null",
            ));
        }
        // One write of admin-settings.json for both fields: a failure changes neither.
        if patch.list_waveform_rate.is_some() || patch.pregenerate_list_waveforms.is_some() {
            security.update_settings_locked(&lease, |settings| {
                if let Some(value) = patch.list_waveform_rate {
                    settings.list_waveform_rate = value;
                }
                if let Some(value) = patch.pregenerate_list_waveforms {
                    settings.pregenerate_list_waveforms = value;
                }
            })?;
            if let Some(value) = patch.list_waveform_rate {
                tracing::info!(?value, "list_waveform_rate changed through PATCH /settings");
            }
            if let Some(value) = patch.pregenerate_list_waveforms {
                tracing::info!(
                    ?value,
                    "pregenerate_list_waveforms changed through PATCH /settings"
                );
            }
        }
        drop(lease);
        Ok(settings_response(&security, &headers, connection.as_ref()))
    })
    .await?;
    Ok(Json(reply))
}

/// `POST /waveforms`: list thumbnail waveforms of up to 200 records (§9.1).
async fn list_waveforms_batch(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let value: serde_json::Value = json_body(&headers, body)?;
    let request = BatchRequest::parse(&value)?;
    let zstd = accepts_zstd(&headers);
    let reply = list_waveform::batch(state, request).await?;
    let json = serde_json::to_vec(&reply)
        .map_err(|error| ApiError::internal(format!("Cannot encode list waveforms: {error}")))?;
    let bytes = if zstd {
        blocking(move || crate::waveform::compress(&json)).await?
    } else {
        json
    };
    let len = bytes.len();
    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if zstd {
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("zstd"));
    }
    Ok(response)
}

/// `GET /waveforms/status` (§9.2).
async fn list_waveform_status(
    Extension(state): Extension<AppState>,
) -> Result<Json<list_waveform::ListWaveformStatus>, ApiError> {
    Ok(Json(
        blocking(move || state.list_waveforms().status(&state)).await?,
    ))
}

async fn set_pregeneration_paused(
    state: AppState,
    paused: bool,
) -> Result<Json<list_waveform::ListWaveformStatus>, ApiError> {
    state.list_waveforms().set_paused(paused);
    tracing::info!(paused, "List waveform pre-generation paused/resumed");
    Ok(Json(
        blocking(move || state.list_waveforms().status(&state)).await?,
    ))
}

/// `POST /waveforms/pregeneration/pause` (this process only; `manage_mode`).
async fn pause_pregeneration(
    Extension(state): Extension<AppState>,
) -> Result<Json<list_waveform::ListWaveformStatus>, ApiError> {
    set_pregeneration_paused(state, true).await
}

/// `POST /waveforms/pregeneration/resume`.
async fn resume_pregeneration(
    Extension(state): Extension<AppState>,
) -> Result<Json<list_waveform::ListWaveformStatus>, ApiError> {
    set_pregeneration_paused(state, false).await
}

fn job_refusal(refusal: list_waveform::JobRefusal) -> ApiError {
    match refusal {
        list_waveform::JobRefusal::Busy(id) => ApiError::job_busy(id),
        list_waveform::JobRefusal::NoJob => {
            ApiError::job_state("No list waveform job is running or paused")
        }
        list_waveform::JobRefusal::Unavailable => {
            ApiError::unavailable("The list waveform cache (waveforms.db) is unavailable")
        }
        list_waveform::JobRefusal::Failed(error) => ApiError::from(error),
    }
}

/// `POST /waveforms/jobs` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobRequest {
    mode: serde_json::Value,
    #[serde(default)]
    confirm: Option<serde_json::Value>,
}

/// `POST /waveforms/jobs` (§9.5.3; `manage_mode`): 202 with the full status.
async fn start_list_waveform_job(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<(StatusCode, Json<list_waveform::ListWaveformStatus>), ApiError> {
    let value: serde_json::Value = json_body(&headers, body)?;
    if !value.is_object() {
        return Err(ApiError::validation(
            "The request body must be a JSON object",
        ));
    }
    let request: JobRequest = serde_json::from_value(value)
        .map_err(|error| ApiError::validation(format!("Invalid job request: {error}")))?;
    let mode = match &request.mode {
        serde_json::Value::String(name) => crate::wf_job::JobMode::parse(name).ok_or_else(|| {
            ApiError::unsupported_mode(format!(
                "unsupported list waveform job mode {name:?} (expected missing, rebuild or failed)"
            ))
        })?,
        _ => return Err(ApiError::validation("mode must be a string")),
    };
    match (&request.confirm, mode) {
        (None, crate::wf_job::JobMode::Rebuild) => {
            return Err(ApiError::confirm_required(
                "mode rebuild clears the list waveform cache; send \"confirm\": \"rebuild\"",
            ));
        }
        (Some(serde_json::Value::String(text)), crate::wf_job::JobMode::Rebuild)
            if text == "rebuild" => {}
        (Some(_), crate::wf_job::JobMode::Rebuild) => {
            return Err(ApiError::confirm_required(
                "confirm must be \"rebuild\" for mode rebuild",
            ));
        }
        // Ignored for the other modes.
        _ => {}
    }
    let status = blocking_api(move || {
        state
            .list_waveforms()
            .start_job(mode)
            .map_err(job_refusal)?;
        state
            .list_waveforms()
            .status(&state)
            .map_err(ApiError::from)
    })
    .await?;
    Ok((StatusCode::ACCEPTED, Json(status)))
}

async fn control_list_waveform_job(
    state: AppState,
    action: fn(&list_waveform::ListWaveforms) -> Result<(), list_waveform::JobRefusal>,
) -> Result<Json<list_waveform::ListWaveformStatus>, ApiError> {
    let status = blocking_api(move || {
        action(state.list_waveforms()).map_err(job_refusal)?;
        state
            .list_waveforms()
            .status(&state)
            .map_err(ApiError::from)
    })
    .await?;
    Ok(Json(status))
}

/// `POST /waveforms/jobs/current/pause` (idempotent; persists across restarts).
async fn pause_list_waveform_job(
    Extension(state): Extension<AppState>,
) -> Result<Json<list_waveform::ListWaveformStatus>, ApiError> {
    control_list_waveform_job(state, |list| list.pause_job(true)).await
}

/// `POST /waveforms/jobs/current/resume` (idempotent).
async fn resume_list_waveform_job(
    Extension(state): Extension<AppState>,
) -> Result<Json<list_waveform::ListWaveformStatus>, ApiError> {
    control_list_waveform_job(state, |list| list.pause_job(false)).await
}

/// `POST /waveforms/jobs/current/stop`: the job ends `cancelled`; generated entries stay.
async fn stop_list_waveform_job(
    Extension(state): Extension<AppState>,
) -> Result<Json<list_waveform::ListWaveformStatus>, ApiError> {
    control_list_waveform_job(state, list_waveform::ListWaveforms::stop_job).await
}

/// Run blocking library work off the async executor.
async fn blocking<T, F>(work: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> audiolib_core::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ApiError::internal(format!("Library task failed: {error}")))?
        .map_err(ApiError::from)
}

async fn health(State(service): State<Service>) -> Json<HealthResponse> {
    let (status, message) = match service.readiness() {
        Readiness::Ready(_) => ("ok", None),
        Readiness::Starting => ("starting", None),
        // Details (which may include paths) stay in the log and authenticated 503 replies.
        Readiness::Failed(_) => ("error", Some(FAILED_MESSAGE.to_string())),
    };
    Json(HealthResponse {
        compatibility_catalog_revision: audiolib_core::compatibility::CATALOG_REVISION,
        upgrade: service.upgrade_progress(),
        status: status.into(),
        version: VERSION.into(),
        message,
        public_url: service.security().public_url(),
        api_version: crate::API_VERSION.into(),
        schema_version: audiolib_db::SCHEMA_VERSION,
        commit: crate::COMMIT.into(),
    })
}

async fn library(Extension(state): Extension<AppState>) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(blocking(move || state.library_response()).await?))
}

async fn roots(Extension(state): Extension<AppState>) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(blocking(move || state.roots_response()).await?))
}

/// Directory browser shared with the admin UI, protected by normal API permissions.
#[derive(Deserialize)]
struct FsQuery {
    path: Option<String>,
    hidden: Option<String>,
}

async fn browse_fs(
    Extension(state): Extension<AppState>,
    query: Result<Query<FsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|error| ApiError::validation(error.body_text()))?;
    let shown = query
        .path
        .clone()
        .unwrap_or_else(|| "starting places".into());
    let work = crate::admin::run_fs_blocking(move || match query.path {
        Some(path) => Ok(Json(crate::browse::list_directory(
            &path,
            matches!(query.hidden.as_deref(), Some("1" | "true")),
        )?)
        .into_response()),
        None => {
            let roots = state
                .root_monitor()
                .roots()
                .iter()
                .map(|root| (root.name.clone(), root.server_path.clone()))
                .collect::<Vec<_>>();
            Ok(Json(crate::browse::places(&roots)).into_response())
        }
    });
    crate::admin::with_fs_timeout(&shown, work).await
}

async fn add_root(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request: AddRootRequest = json_body(&headers, body)?;
    let shown = request.path.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let _on_drop = CancelOnDrop(cancel.clone());
    let root = crate::admin::with_fs_timeout(
        &shown,
        crate::admin::run_fs_blocking(move || state.add_root_cancellable(request, cancel)),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(root)).into_response())
}

async fn remove_root(
    Extension(state): Extension<AppState>,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    let cancel = Arc::new(AtomicBool::new(false));
    let _on_drop = CancelOnDrop(cancel.clone());
    Ok(Json(
        crate::admin::run_blocking(move || state.remove_root_cancellable(&name, cancel)).await?,
    )
    .into_response())
}

async fn update_root_path(
    Extension(state): Extension<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request: RootPathRequest = json_body(&headers, body)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let _on_drop = CancelOnDrop(cancel.clone());
    let root =
        crate::admin::run_fs_blocking(move || state.update_root_path(&name, request, cancel))
            .await?;
    Ok(Json(root).into_response())
}

/// `POST /roots/{name}/check`: bounded inline recheck (about 2 s), then the root's
/// `/roots` entry; unknown names are 404 `NOT_FOUND`.
async fn check_root(
    Extension(state): Extension<AppState>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(blocking(move || state.check_root(&name)).await?))
}

/// Parse an `{id}` path segment; a JSON `VALIDATION` error instead of axum's plain-text one.
fn parse_id(raw: &str) -> Result<i64, ApiError> {
    raw.parse()
        .map_err(|_| ApiError::validation(format!("Audio id must be an integer, got {raw:?}")))
}

/// Parse a folder `{id}` path segment like [`parse_id`].
fn parse_folder_id(raw: &str) -> Result<i64, ApiError> {
    raw.parse()
        .map_err(|_| ApiError::validation(format!("Folder id must be an integer, got {raw:?}")))
}

async fn audio(
    Extension(state): Extension<AppState>,
    Path(raw_id): Path<String>,
) -> Result<Json<AudioFile>, ApiError> {
    let id = parse_id(&raw_id)?;
    Ok(Json(blocking(move || state.audio(id)).await?))
}

/// Decode a JSON request body; malformed JSON or wrong field types are 400 `VALIDATION`,
/// an oversized body 413 `PAYLOAD_TOO_LARGE`. JSON mutations require application/json.
pub(crate) fn json_body<T: DeserializeOwned>(
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<T, ApiError> {
    let mut content_types = headers.get_all(header::CONTENT_TYPE).iter();
    let is_json = content_types
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        && content_types.next().is_none();
    if !is_json {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "JSON_CONTENT_TYPE_REQUIRED",
            "JSON mutations require Content-Type: application/json",
        ));
    }
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "PAYLOAD_TOO_LARGE",
                format!("Request body exceeds {MAX_JSON_BODY_BYTES} bytes"),
            )
        } else {
            ApiError::validation(format!(
                "Cannot read request body: {}",
                rejection.body_text()
            ))
        }
    })?;
    serde_json::from_slice(&body)
        .map_err(|error| ApiError::validation(format!("Invalid JSON body: {error}")))
}

/// True when `value` is a JSON object carrying a legacy `rating` key and no current
/// annotation field (`tags`, `color`, `notes`, `favorite`) with a non-null value: what a
/// pre-v5 client sends to change only the rating (removed in schema v5).
fn is_legacy_rating_only(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.contains_key("rating")
        && ["tags", "color", "notes", "favorite"]
            .iter()
            .all(|key| object.get(*key).is_none_or(serde_json::Value::is_null))
}

/// `PATCH /audio/{id}/metadata`: apply a `MetadataPatch`, reply with the record after
/// the write and the revisions around it. A legacy rating-only patch (pre-v5 clients) is
/// a 200 no-op that returns the current record with `revision_before == revision`.
async fn update_metadata(
    Extension(state): Extension<AppState>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<AudioWriteResponse>, ApiError> {
    let id = parse_id(&raw_id)?;
    let raw: serde_json::Value = json_body(&headers, body)?;
    if is_legacy_rating_only(&raw) {
        return Ok(Json(
            blocking(move || {
                let revision = state.poll_revision()?.revision;
                Ok(AudioWriteResponse {
                    file: state.audio(id)?,
                    revision_before: revision,
                    revision,
                })
            })
            .await?,
        ));
    }
    let patch: MetadataPatch = serde_json::from_value(raw)
        .map_err(|error| ApiError::validation(format!("Invalid JSON body: {error}")))?;
    Ok(Json(
        blocking(move || state.update_metadata(id, &patch)).await?,
    ))
}

/// Wire form of `BatchMetadataRequest`; `tag_mode` may be omitted (`replace`).
#[derive(Deserialize)]
struct BatchMetadataBody {
    ids: Vec<i64>,
    patch: MetadataPatch,
    #[serde(default)]
    tag_mode: TagEditMode,
}

/// `POST /audio/metadata/batch`: apply a `BatchMetadataRequest`, reply with the report,
/// the changed records and the revisions around the write. A legacy rating-only `patch`
/// (pre-v5 clients) changes nothing: 200 with `updated: 0`, the current records and
/// `revision_before == revision` (ids are still validated).
async fn update_metadata_batch(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<BatchWriteResponse>, ApiError> {
    let raw: serde_json::Value = json_body(&headers, body)?;
    if raw.get("patch").is_some_and(is_legacy_rating_only) {
        let ids: Vec<i64> = raw
            .get("ids")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| ApiError::validation(format!("Invalid JSON body: {error}")))?
            .ok_or_else(|| ApiError::validation("Invalid JSON body: missing field `ids`"))?;
        return Ok(Json(
            blocking(move || {
                if ids.is_empty() || ids.len() > 10_000 || ids.iter().any(|id| *id <= 0) {
                    return Err(audiolib_core::Error::Validation(
                        "Batch requires 1..=10000 positive IDs".into(),
                    ));
                }
                let mut seen = std::collections::HashSet::new();
                let files = ids
                    .iter()
                    .copied()
                    .filter(|id| seen.insert(*id))
                    .map(|id| state.audio(id))
                    .collect::<audiolib_core::Result<Vec<_>>>()?;
                let revision = state.poll_revision()?.revision;
                Ok(BatchWriteResponse {
                    report: audiolib_core::BatchMetadataReport { updated: 0 },
                    files,
                    revision_before: revision,
                    revision,
                })
            })
            .await?,
        ));
    }
    let body: BatchMetadataBody = serde_json::from_value(raw)
        .map_err(|error| ApiError::validation(format!("Invalid JSON body: {error}")))?;
    let request = BatchMetadataRequest {
        ids: body.ids,
        patch: body.patch,
        tag_mode: body.tag_mode,
    };
    Ok(Json(
        blocking(move || state.update_metadata_batch(&request)).await?,
    ))
}

/// `POST /audio/{id}/plays` (deprecated): a no-op kept for old clients. Nothing is
/// written; the reply has the metadata patch shape with `revision == revision_before`.
async fn record_play(
    Extension(state): Extension<AppState>,
    Path(raw_id): Path<String>,
) -> Result<Json<AudioWriteResponse>, ApiError> {
    let id = parse_id(&raw_id)?;
    Ok(Json(blocking(move || state.record_play(id)).await?))
}

/// `POST /audio/remove`: remove library records (never files); see `crate::manage`.
async fn remove_audio(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<RemoveAudioResponse>, ApiError> {
    let request: RemoveAudioRequest = json_body(&headers, body)?;
    Ok(Json(
        blocking_api(move || state.remove_audio(&request.ids)).await?,
    ))
}

/// `DELETE /folders/{id}`: remove a folder subtree's records (never files).
async fn remove_folder(
    Extension(state): Extension<AppState>,
    Path(raw_id): Path<String>,
) -> Result<Json<RemoveFolderResponse>, ApiError> {
    let id = parse_folder_id(&raw_id)?;
    Ok(Json(blocking_api(move || state.remove_folder(id)).await?))
}

/// `limit` + `offset` of the history lists (doc/api/README.md §5).
#[derive(Deserialize)]
struct PageQuery {
    limit: Option<String>,
    offset: Option<String>,
}

/// Default page size of `/history/...` (the library allows 1..=10000).
const DEFAULT_HISTORY_LIMIT: usize = 100;

fn page(query: Result<Query<PageQuery>, QueryRejection>) -> Result<(usize, usize), ApiError> {
    let Query(query) = query.map_err(|rejection| {
        ApiError::validation(format!("Invalid query string: {}", rejection.body_text()))
    })?;
    let number = |name: &str, raw: Option<&str>, default: usize| match raw {
        None => Ok(default),
        Some(raw) => raw.parse::<usize>().map_err(|_| {
            ApiError::validation(format!(
                "{name} must be a non-negative integer, got {raw:?}"
            ))
        }),
    };
    let limit = number("limit", query.limit.as_deref(), DEFAULT_HISTORY_LIMIT)?;
    let offset = number("offset", query.offset.as_deref(), 0)?;
    if limit == 0 || limit > 10_000 {
        return Err(ApiError::validation("History limit must be 1..=10000"));
    }
    if i64::try_from(offset).is_err() {
        return Err(ApiError::validation("History offset is too large"));
    }
    Ok((limit, offset))
}

/// `GET /history/scans?limit=&offset=`: `ScanSession[]`, newest first.
async fn scan_history(
    Extension(state): Extension<AppState>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Json<Vec<ScanSession>>, ApiError> {
    let (limit, offset) = page(query)?;
    Ok(Json(
        blocking(move || state.provider().scan_history(limit, offset)).await?,
    ))
}

/// `GET /history/plays?limit=&offset=` (deprecated): always an empty array.
/// Play history is recorded locally by clients only; paging is still validated.
async fn play_history(
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    page(query)?;
    Ok(Json(Vec::new()))
}

/// Run blocking work that already answers with [`ApiError`].
async fn blocking_api<T, F>(work: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ApiError::internal(format!("Library task failed: {error}")))?
}

/// Parse a `{session_id}` path segment.
fn parse_session_id(raw: &str) -> Result<i64, ApiError> {
    raw.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| {
        ApiError::validation(format!(
            "Scan session id must be a positive integer, got {raw:?}"
        ))
    })
}

/// `POST /scans`: start a scan of a configured root (or a directory below it) and
/// answer with its first progress once the library session exists.
async fn start_scan(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<ScanResponse>, ApiError> {
    let request: ScanRequest = json_body(&headers, body)?;
    let started = blocking_api(move || state.start_scan(request)).await?;
    // If the client gives up while the directory is still resolving, the scan still
    // starts; `GET /scans/current` shows it.
    match started.await {
        Ok(result) => result.map(Json),
        Err(_) => Err(ApiError::internal("The scan thread ended unexpectedly")),
    }
}

/// `GET /scans/current`: `{"scan": ScanResponse | null}` (always 200).
async fn current_scan(Extension(state): Extension<AppState>) -> Json<CurrentScanResponse> {
    Json(CurrentScanResponse {
        scan: state.scans().current(),
    })
}

/// `GET /scans/{session_id}`.
async fn scan_status(
    Extension(state): Extension<AppState>,
    Path(raw_id): Path<String>,
) -> Result<Json<ScanResponse>, ApiError> {
    let id = parse_session_id(&raw_id)?;
    Ok(Json(blocking_api(move || state.scan_status(id)).await?))
}

async fn control_scan(
    state: AppState,
    raw_id: &str,
    action: ScanAction,
) -> Result<Json<ScanResponse>, ApiError> {
    let id = parse_session_id(raw_id)?;
    Ok(Json(
        blocking_api(move || state.control_scan(id, action)).await?,
    ))
}

/// `POST /scans/{session_id}/pause`.
async fn pause_scan(
    Extension(state): Extension<AppState>,
    Path(raw_id): Path<String>,
) -> Result<Json<ScanResponse>, ApiError> {
    control_scan(state, &raw_id, ScanAction::Pause).await
}

/// `POST /scans/{session_id}/resume`.
async fn resume_scan(
    Extension(state): Extension<AppState>,
    Path(raw_id): Path<String>,
) -> Result<Json<ScanResponse>, ApiError> {
    control_scan(state, &raw_id, ScanAction::Resume).await
}

/// `POST /scans/{session_id}/cancel`.
async fn cancel_scan(
    Extension(state): Extension<AppState>,
    Path(raw_id): Path<String>,
) -> Result<Json<ScanResponse>, ApiError> {
    control_scan(state, &raw_id, ScanAction::Cancel).await
}

/// Open the file on the blocking pool. No time limit applies (a sleeping NAS disk may
/// need 20-30 s or more); if the client disconnects meanwhile, the handler future and
/// with it the stream slot are dropped while the blocking open finishes on its own (the
/// root monitor bounds such leftovers, see `AppState::open_audio`). An open still
/// running after [`SLOW_OPEN_WARNING`] is logged with its root and path.
async fn open_for_stream(state: AppState, id: i64) -> Result<ServedAudio, ApiError> {
    let progress = OpenProgress::default();
    let reporter = progress.clone();
    let task = tokio::task::spawn_blocking(move || state.open_audio_reporting(id, &reporter));
    watch_open(id, "Stream", task, &progress).await
}

/// Await blocking file work, warning once if its file open (while `progress` names a
/// target) is still running after [`SLOW_OPEN_WARNING`]; map open errors to API errors.
async fn watch_open<T>(
    id: i64,
    what: &'static str,
    mut task: tokio::task::JoinHandle<Result<T, OpenError>>,
    progress: &OpenProgress,
) -> Result<T, ApiError> {
    let joined = tokio::select! {
        joined = &mut task => joined,
        () = tokio::time::sleep(SLOW_OPEN_WARNING) => {
            if let Some((root, path)) = progress.target() {
                tracing::warn!(
                    id,
                    root = %root,
                    path = %path,
                    "{what} file open has been waiting for 10 s (sleeping disk or pending macOS permission prompt?)"
                );
            }
            task.await
        }
    };
    match joined {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(open_error(error)),
        Err(error) => Err(ApiError::internal(format!("Library task failed: {error}"))),
    }
}

fn open_error(error: OpenError) -> ApiError {
    match error {
        OpenError::Library(error) => error.into(),
        OpenError::RootOffline { root, reason } => {
            ApiError::root_offline(format!("Library root {root:?} is offline: {reason}"))
        }
        OpenError::ArtworkBusy => ApiError::artwork_busy(),
    }
}

/// Query string of `/audio/{id}/stream`; other parameters are ignored.
#[derive(Deserialize)]
struct StreamQuery {
    download: Option<String>,
}

/// `download=1`/`true` (case-insensitive) asks for an attachment; absent means inline;
/// any other value (or a malformed query string) is 400 `VALIDATION`.
fn wants_download(query: Result<Query<StreamQuery>, QueryRejection>) -> Result<bool, ApiError> {
    let Query(query) = query.map_err(|rejection| {
        ApiError::validation(format!("Invalid query string: {}", rejection.body_text()))
    })?;
    match query.download.as_deref() {
        None => Ok(false),
        Some(value) if value == "1" || value.eq_ignore_ascii_case("true") => Ok(true),
        Some(value) => Err(ApiError::validation(format!(
            "download must be 1 or true, got {value:?}"
        ))),
    }
}

/// `GET`/`HEAD /audio/{id}/stream`: raw file bytes with single-range and ETag support
/// (see [`crate::stream`]). axum answers `HEAD` through this handler without a body.
async fn audio_stream(
    method: Method,
    Extension(state): Extension<AppState>,
    Path(raw_id): Path<String>,
    query: Result<Query<StreamQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = parse_id(&raw_id)?;
    let download = wants_download(query)?;
    let mut trace = PlaybackTrace::start(
        state.list_waveforms().activity(),
        "stream",
        id,
        method == Method::HEAD,
        "file",
    );
    // The permit travels with the response body, so it counts streams until fully sent
    // or dropped by the client.
    let permit = state
        .stream_slots()
        .try_acquire_owned()
        .map_err(|_| ApiError::stream_busy())?;
    trace.slot_acquired();
    if method != Method::HEAD {
        state.note_playback_start();
    }
    let ServedAudio {
        file,
        path,
        len,
        etag,
        filename,
        record,
        ..
    } = open_for_stream(state, id).await?;
    trace.opened();
    trace.format(
        (record.sample_rate, record.bit_depth, record.channels),
        (record.sample_rate, record.bit_depth, record.channels),
    );
    // Same file, same ETag: the disposition never enters the validator.
    let disposition = download
        .then(|| HeaderValue::try_from(stream::attachment_disposition(&filename, id)).ok())
        .flatten();
    let header_text = |name: header::HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
    let common = |response: &mut Response| {
        let headers = response.headers_mut();
        headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-cache"),
        );
        if let Ok(value) = HeaderValue::from_str(&etag) {
            headers.insert(header::ETAG, value);
        }
    };
    let attach = |response: &mut Response| {
        if let Some(value) = &disposition {
            response
                .headers_mut()
                .insert(header::CONTENT_DISPOSITION, value.clone());
        }
    };
    if header_text(header::IF_NONE_MATCH).is_some_and(|value| stream::none_match(value, &etag)) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        common(&mut response);
        attach(&mut response);
        return Ok(response);
    }
    let range = stream::select_range(
        header_text(header::RANGE),
        header_text(header::IF_RANGE),
        &etag,
        len,
    );
    let Some(body_len) = range.body_len(len) else {
        let mut response = ApiError::range_not_satisfiable(format!(
            "Requested range is outside the {len}-byte file"
        ))
        .into_response();
        common(&mut response);
        if let Ok(value) = HeaderValue::try_from(format!("bytes */{len}")) {
            response.headers_mut().insert(header::CONTENT_RANGE, value);
        }
        return Ok(response);
    };
    let mut file = tokio::fs::File::from_std(file);
    let (status, content_range) = match range {
        ByteRange::Partial { start, end } => {
            file.seek(SeekFrom::Start(start))
                .await
                .map_err(|error| ApiError::from(audiolib_core::Error::Io(error)))?;
            (
                StatusCode::PARTIAL_CONTENT,
                Some(format!("bytes {start}-{end}/{len}")),
            )
        }
        _ => (StatusCode::OK, None),
    };
    let body = GuardedStream {
        inner: ReaderStream::with_capacity(file.take(body_len), STREAM_CHUNK_BYTES),
        _guard: permit,
    };
    let mut response = Response::new(Body::from_stream(Traced::new(body, trace)));
    *response.status_mut() = status;
    common(&mut response);
    attach(&mut response);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(stream::content_type(&path)),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body_len));
    if let Some(value) = content_range.and_then(|value| HeaderValue::try_from(value).ok()) {
        headers.insert(header::CONTENT_RANGE, value);
    }
    Ok(response)
}

/// Query string of `/audio/{id}/preview`; other parameters are ignored.
#[derive(Deserialize)]
struct PreviewQuery {
    codec: Option<String>,
    sr: Option<String>,
    bits: Option<String>,
    ch: Option<String>,
    t: Option<String>,
}

/// `GET`/`HEAD /audio/{id}/preview?codec=pcm|flac&sr=&bits=&ch=[&t=]`: a smaller
/// rendition for low-bandwidth playback (doc/api/README.md §8.1). `pcm` is an
/// in-process, sized and rangeable WAV; `flac` a live ffmpeg transcode. Not time-limited.
async fn audio_preview(
    Extension(state): Extension<AppState>,
    Path(raw_id): Path<String>,
    method: Method,
    query: Result<Query<PreviewQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = parse_id(&raw_id)?;
    let Query(query) = query.map_err(|rejection| {
        ApiError::validation(format!("Invalid query string: {}", rejection.body_text()))
    })?;
    let request = PreviewRequest::parse(
        query.codec.as_deref(),
        query.sr.as_deref(),
        query.bits.as_deref(),
        query.ch.as_deref(),
        query.t.as_deref(),
    )?;
    let head = method == Method::HEAD;
    match request.codec {
        Codec::Pcm => {
            let trace = PlaybackTrace::start(
                state.list_waveforms().activity(),
                "preview",
                id,
                head,
                "pcm",
            );
            pcm_preview(state, id, &request, head, &headers, trace).await
        }
        Codec::Flac => {
            let trace = PlaybackTrace::start(
                state.list_waveforms().activity(),
                "preview",
                id,
                head,
                "flac",
            );
            flac_preview(state, id, &request, head, &headers, trace).await
        }
    }
}

/// Open audio `id` like `/stream` (root checks, no time limit), then parse its WAV
/// header on the blocking pool.
async fn open_wav(
    state: &AppState,
    id: i64,
) -> Result<(ServedAudio, Result<pcm::WavInfo, WavError>), ApiError> {
    let served = open_for_stream(state.clone(), id).await?;
    let task = tokio::task::spawn_blocking(move || {
        let info = pcm::parse_wav(&served.file, served.len);
        (served, info)
    });
    let (served, info) = task
        .await
        .map_err(|error| ApiError::internal(format!("Library task failed: {error}")))?;
    let info = match info {
        Err(WavError::Io(error)) => {
            return Err(open_error(
                state.decode_failure(&served.path, audiolib_core::Error::Io(error)),
            ));
        }
        other => other,
    };
    Ok((served, info))
}

fn preview_format_headers(headers: &mut HeaderMap, rate: u32, bits: u16, channels: u16) {
    headers.insert("x-preview-sample-rate", HeaderValue::from(rate));
    headers.insert("x-preview-bits", HeaderValue::from(bits));
    headers.insert("x-preview-channels", HeaderValue::from(channels));
}

/// `X-Total-Frames` (output frames from the start, at the output rate) and `X-Duration`
/// (their length in seconds, 6 decimals), the same on both codecs.
fn preview_length_headers(headers: &mut HeaderMap, frames: u64, rate: u32) {
    headers.insert("x-total-frames", HeaderValue::from(frames));
    let seconds = frames as f64 / f64::from(rate.max(1));
    if let Ok(value) = HeaderValue::try_from(format!("{seconds:.6}")) {
        headers.insert("x-duration", value);
    }
}

/// `codec=pcm`: one of the `max_previews` slots (else 503 `BUSY`), Range/ETag rules as
/// `/stream`.
async fn pcm_preview(
    state: AppState,
    id: i64,
    request: &PreviewRequest,
    head: bool,
    headers: &HeaderMap,
    mut trace: PlaybackTrace,
) -> Result<Response, ApiError> {
    let permit = state
        .preview_slots()
        .try_acquire_owned()
        .map_err(|_| ApiError::preview_busy())?;
    trace.slot_acquired();
    if !head {
        state.note_playback_start();
    }
    let (served, info) = open_wav(&state, id).await?;
    trace.opened();
    let info = info.map_err(|error| {
        let reason = match error {
            WavError::Unsupported(reason) => reason,
            WavError::Io(error) => error.to_string(),
        };
        ApiError::preview_unsupported(format!(
            "Audio {id} cannot be previewed with codec=pcm ({reason}); use codec=flac or /stream"
        ))
    })?;
    let Some(factor) = pcm::decimation_factor(info.sample_rate, request.sample_rate) else {
        return Err(ApiError::preview_unsupported(format!(
            "Audio {id} cannot be previewed with codec=pcm at sr={}: {} Hz has no integer \
             factor up to {} reaching it; use codec=flac, a higher sr, or /stream",
            request.sample_rate.unwrap_or_default(),
            info.sample_rate,
            pcm::MAX_DECIMATION_FACTOR
        )));
    };
    let bits = preview::resolve_bits(request.bits, info.format.bits(), info.format.is_float());
    let (channels, mix) = preview::resolve_channels(request.channels, info.channels)?;
    let source = (info.sample_rate, info.format.bits(), info.channels);
    let plan = PcmPlan::new(info, factor, bits, channels, mix);
    trace.format(source, (plan.sample_rate, plan.bits, plan.channels));
    let etag = plan.etag(&served.etag);
    let total = plan.total_len();
    let common = |response: &mut Response| {
        let headers = response.headers_mut();
        headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-cache"),
        );
        if let Ok(value) = HeaderValue::from_str(&etag) {
            headers.insert(header::ETAG, value);
        }
        preview_format_headers(headers, plan.sample_rate, plan.bits, plan.channels);
        headers.insert("x-preview-frames", HeaderValue::from(plan.frames));
        preview_length_headers(headers, plan.frames, plan.sample_rate);
    };
    let header_text = |name: header::HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
    if header_text(header::IF_NONE_MATCH).is_some_and(|value| stream::none_match(value, &etag)) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        common(&mut response);
        return Ok(response);
    }
    let range = stream::select_range(
        header_text(header::RANGE),
        header_text(header::IF_RANGE),
        &etag,
        total,
    );
    let Some(body_len) = range.body_len(total) else {
        let mut response = ApiError::range_not_satisfiable(format!(
            "Requested range is outside the {total}-byte preview"
        ))
        .into_response();
        common(&mut response);
        if let Ok(value) = HeaderValue::try_from(format!("bytes */{total}")) {
            response.headers_mut().insert(header::CONTENT_RANGE, value);
        }
        return Ok(response);
    };
    let (status, first, last) = match range {
        ByteRange::Partial { start, end } => (StatusCode::PARTIAL_CONTENT, start, end),
        _ => (StatusCode::OK, 0, total.saturating_sub(1)),
    };
    let body = if head || body_len == 0 {
        headers_only_body()
    } else {
        let renderer = Renderer::new(plan.clone());
        // 1 MiB source reads with two blocks loading ahead hide NAS latency spikes.
        let file = pcm::ReadAhead::new(Arc::new(served.file), served.len);
        let (sender, receiver) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(4);
        tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let (mut sent, mut gone) = (0_u64, false);
            let result = renderer.render_range(&file, first, last, |chunk| {
                sent += chunk.len() as u64;
                gone = sender.blocking_send(Ok(Bytes::from(chunk))).is_err();
                !gone
            });
            if let Err(error) = result {
                tracing::warn!(id, %error, "PCM preview stopped: reading the source failed");
                let _ = sender.blocking_send(Err(error));
                return;
            }
            tracing::info!(
                id,
                bytes = sent,
                millis = started.elapsed().as_millis() as u64,
                complete = !gone,
                "PCM preview sent"
            );
        });
        Body::from_stream(Traced::new(
            ChannelBody {
                receiver,
                _permit: permit,
            },
            trace,
        ))
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    common(&mut response);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body_len));
    if status == StatusCode::PARTIAL_CONTENT
        && let Ok(value) = HeaderValue::try_from(format!("bytes {first}-{last}/{total}"))
    {
        headers.insert(header::CONTENT_RANGE, value);
    }
    Ok(response)
}

/// `codec=flac`: takes one of the `max_previews` slots (else 503 `BUSY`), opens the file
/// like `/stream`, answers `If-None-Match` (304) and `HEAD` without ffmpeg, then waits up
/// to [`preview::TRANSCODE_SLOT_WAIT`] for one of `max_transcodes` slots (then 503
/// `BUSY`) and streams ffmpeg's stdout (chunked, no Range).
async fn flac_preview(
    state: AppState,
    id: i64,
    request: &PreviewRequest,
    head: bool,
    headers: &HeaderMap,
    mut trace: PlaybackTrace,
) -> Result<Response, ApiError> {
    let ffmpeg = state.ffmpeg_path().to_path_buf();
    if !preview::ffmpeg_available(&ffmpeg) {
        tracing::warn!(path = %ffmpeg.display(), "ffmpeg is not available for a FLAC preview");
        return Err(ApiError::transcoder_unavailable(
            "ffmpeg is not available on the server; use codec=pcm or /stream",
        ));
    }
    let preview_permit = state
        .preview_slots()
        .try_acquire_owned()
        .map_err(|_| ApiError::preview_busy())?;
    if !head {
        state.note_playback_start();
    }
    let (served, info) = open_wav(&state, id).await?;
    trace.opened();
    let source_format = match &info {
        Ok(info) => (info.sample_rate, info.format.bits(), info.channels),
        Err(_) => (
            served.record.sample_rate,
            served.record.bit_depth,
            served.record.channels,
        ),
    };
    let source = match &info {
        Ok(info) => SourceFormat::from_wav(info),
        Err(_) => SourceFormat::from_record(&served.record),
    };
    let plan = FlacPlan::new(&source, request)?;
    trace.format(source_format, (plan.sample_rate, plan.bits, plan.channels));
    let etag = plan.etag(&served.etag, &preview::ffmpeg_tag(&ffmpeg));
    let set_headers = |headers: &mut HeaderMap| -> Result<(), ApiError> {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/flac"));
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-cache"),
        );
        headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
        headers.insert(header::ETAG, header_value(etag.clone())?);
        if let Some(frames) = plan.total_frames() {
            preview_length_headers(headers, frames, plan.sample_rate);
        }
        headers.insert(
            HeaderName::from_static("x-start-time"),
            header_value(plan.start_time())?,
        );
        headers.insert("x-start-frame", HeaderValue::from(plan.start_frame));
        preview_format_headers(headers, plan.sample_rate, plan.bits, plan.channels);
        Ok(())
    };
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok());
    if if_none_match.is_some_and(|value| stream::none_match(value, &etag)) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        set_headers(response.headers_mut())?;
        response.headers_mut().remove(header::CONTENT_TYPE);
        return Ok(response);
    }
    if head {
        let mut response = Response::new(headers_only_body());
        set_headers(response.headers_mut())?;
        return Ok(response);
    }
    // Waiting for a transcode slot holds only this preview slot, never a /stream slot.
    let transcode_wait = Instant::now();
    let transcode_permit = tokio::time::timeout(
        preview::TRANSCODE_SLOT_WAIT,
        state.transcode_slots().acquire_owned(),
    )
    .await
    .map_err(|_| ApiError::transcode_busy())?
    .map_err(|_| ApiError::internal("Transcode slots closed"))?;
    trace.add_slot_wait(transcode_wait.elapsed());
    let ServedAudio { file, path, .. } = served;
    tracing::debug!(id, path = %path.display(), start = %plan.start_time(), "Starting FLAC preview");
    // On Unix ffmpeg reads the very file that passed the root checks through its stdin
    // descriptor (seekable: /dev/fd/0 is the regular file), never reopening the path.
    #[cfg(unix)]
    let (input, stdin) = ("/dev/fd/0".to_string(), std::process::Stdio::from(file));
    #[cfg(not(unix))]
    let (input, stdin) = {
        drop(file);
        (
            format!("file:{}", path.to_string_lossy()),
            std::process::Stdio::null(),
        )
    };
    let args = plan.ffmpeg_args(&input);
    let mut body = FlacStream::spawn(
        &ffmpeg,
        &args,
        stdin,
        vec![preview_permit, transcode_permit],
        state.transcodes(),
        id,
    )?;
    body.first_chunk().await?;
    trace.transcoder_ready();
    let mut response = Response::new(Body::from_stream(Traced::new(body, trace)));
    set_headers(response.headers_mut())?;
    Ok(response)
}

/// Body of a `HEAD` reply: no data and no size hint, so no `Content-Length: 0` is added
/// over the real one (or to a chunked FLAC reply).
fn headers_only_body() -> Body {
    Body::from_stream(NoData)
}

struct NoData;

impl Stream for NoData {
    type Item = Result<Bytes, Infallible>;
    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

/// Chunks produced on the blocking pool; holds the preview slot until sent or dropped.
/// Dropping it (client gone) makes the producer's next send fail, which stops it.
struct ChannelBody {
    receiver: tokio::sync::mpsc::Receiver<io::Result<Bytes>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Stream for ChannelBody {
    type Item = io::Result<Bytes>;
    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().receiver.poll_recv(context)
    }
}

/// Query string of `/audio/{id}/waveform`; other parameters are ignored.
#[derive(Deserialize)]
struct WaveformQuery {
    buckets: Option<String>,
    colors: Option<String>,
    start_frame: Option<String>,
    end_frame: Option<String>,
}

/// Sets the flag when dropped: the handler future is dropped when the client goes away,
/// which stops the decode at its next check.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// `GET`/`HEAD /audio/{id}/waveform`: cached peak summary of the file
/// (doc/api/README.md §9). Waits for one of `max_waveforms` slots; not time-limited.
async fn audio_waveform(
    Extension(state): Extension<AppState>,
    Path(raw_id): Path<String>,
    query: Result<Query<WaveformQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = parse_id(&raw_id)?;
    let Query(query) = query.map_err(|rejection| {
        ApiError::validation(format!("Invalid query string: {}", rejection.body_text()))
    })?;
    let params = WaveformParams::parse(
        query.buckets.as_deref(),
        query.colors.as_deref(),
        query.start_frame.as_deref(),
        query.end_frame.as_deref(),
    )?;
    let zstd = accepts_zstd(&headers);
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let _permit = state
        .waveform_slots()
        .acquire_owned()
        .await
        .map_err(|_| ApiError::internal("Waveform slots closed"))?;
    let cancel = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    let progress = OpenProgress::default();
    let reporter = progress.clone();
    let task = tokio::task::spawn_blocking(move || {
        state.waveform(
            id,
            &params,
            zstd,
            if_none_match.as_deref(),
            &cancel,
            &reporter,
        )
    });
    let reply = watch_open(id, "Waveform", task, &progress).await?;
    let cache_control = HeaderValue::from_static("private, no-cache");
    Ok(match reply {
        WaveformReply::NotModified { etag } => {
            let mut response = StatusCode::NOT_MODIFIED.into_response();
            let headers = response.headers_mut();
            headers.insert(header::ETAG, header_value(etag)?);
            headers.insert(header::CACHE_CONTROL, cache_control);
            response
        }
        WaveformReply::Body { etag, bytes, zstd } => {
            let len = bytes.len();
            let mut response = Response::new(Body::from(bytes));
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
            headers.insert(header::ETAG, header_value(etag)?);
            headers.insert(header::CACHE_CONTROL, cache_control);
            if zstd {
                headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("zstd"));
            }
            response
        }
    })
}

/// Query string of `/audio/{id}/artwork`; other parameters are ignored.
#[derive(Deserialize)]
struct ArtworkQuery {
    side: Option<String>,
}

/// Directory context is recognized only on the album endpoint. The historical
/// per-audio endpoint continues to ignore unknown query parameters.
#[derive(Deserialize)]
struct AlbumArtworkQuery {
    side: Option<String>,
    folder_id: Option<i64>,
    unassigned_directory: Option<String>,
}

/// `GET`/`HEAD /audio/{id}/artwork`: embedded JPEG/PNG cover (doc/api/README.md §8.2).
/// Not time-limited. No cover, a missing file and a failed extract are 404.
async fn audio_artwork(
    Extension(state): Extension<AppState>,
    Path(raw_id): Path<String>,
    query: Result<Query<ArtworkQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    artwork_response(
        state,
        raw_id,
        query.map(|Query(query)| AlbumArtworkQuery {
            side: query.side,
            folder_id: None,
            unassigned_directory: None,
        }),
        headers,
        false,
    )
    .await
}

/// Album-owned cover; every member resolves the same bounded representative.
async fn album_artwork(
    Extension(state): Extension<AppState>,
    Path(raw_id): Path<String>,
    query: Result<Query<AlbumArtworkQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    artwork_response(
        state,
        raw_id,
        query.map(|Query(query)| query),
        headers,
        true,
    )
    .await
}

async fn artwork_response(
    state: AppState,
    raw_id: String,
    query: Result<AlbumArtworkQuery, QueryRejection>,
    headers: HeaderMap,
    album: bool,
) -> Result<Response, ApiError> {
    let identities = headers.get_all(LIBRARY_ID_HEADER);
    let mut identities = identities.iter();
    let identity = identities
        .next()
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty() && identities.next().is_none())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "LIBRARY_ID_REQUIRED",
                "Artwork reads require X-AudioLib-Library-Id from the opened snapshot",
            )
        })?;
    if identity != state.library_id() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "LIBRARY_CHANGED",
            "The server library changed; synchronize before reading artwork",
        ));
    }
    let album_namespace = if album {
        let expected = state.provider().album_namespace();
        let mut values = headers.get_all("x-audiolib-album-namespace").iter();
        let actual = values.next().and_then(|value| value.to_str().ok());
        if actual != Some(expected.as_str()) || values.next().is_some() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "LIBRARY_CHANGED",
                "The server album namespace changed; synchronize before reading artwork",
            ));
        }
        Some(header_value(expected)?)
    } else {
        None
    };
    // The middleware's AppState clone pins this identity throughout the read.
    let library_id = header_value(state.library_id().to_owned())?;
    let id = parse_id(&raw_id)?;
    let AlbumArtworkQuery {
        side,
        folder_id,
        unassigned_directory,
    } = query.map_err(|rejection| {
        ApiError::validation(format!("Invalid query string: {}", rejection.body_text()))
    })?;
    let side = crate::artwork::parse_side(side.as_deref())?;
    if folder_id.is_some() && unassigned_directory.is_some() {
        return Err(ApiError::validation(
            "folder_id and unassigned_directory are mutually exclusive",
        ));
    }
    let unassigned_directory = unassigned_directory
        .map(|directory| {
            if directory.len() > 4096 {
                return Err(ApiError::validation(
                    "unassigned_directory exceeds 4096 UTF-8 bytes",
                ));
            }
            let canonical = audiolib_db::normalize_source_path_identity(&directory);
            if canonical.len() > 4096 {
                return Err(ApiError::validation(
                    "Canonical directory exceeds 4096 UTF-8 bytes",
                ));
            }
            Ok(canonical)
        })
        .transpose()?;
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let progress = OpenProgress::default();
    let reporter = progress.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    // Decode slots are taken inside the resolver, only on a cover cache miss, so
    // cache hits and `If-None-Match` 304s never wait for or hold one.
    let task = tokio::task::spawn_blocking(move || {
        if album {
            if let Some(folder_id) = folder_id {
                state.album_artwork_in_folder(
                    id,
                    folder_id,
                    side,
                    if_none_match.as_deref(),
                    &cancel,
                    &reporter,
                )
            } else if let Some(directory) = unassigned_directory.as_deref() {
                state.album_artwork_in_unassigned_directory(
                    id,
                    directory,
                    side,
                    if_none_match.as_deref(),
                    &cancel,
                    &reporter,
                )
            } else {
                state
                    .album_artwork(id, side, if_none_match.as_deref(), &cancel, &reporter)
                    .map(|reply| crate::artwork::AlbumArtworkReply {
                        reply,
                        directory_identity: None,
                    })
            }
        } else {
            state
                .artwork(id, side, if_none_match.as_deref(), &reporter)
                .map(|reply| crate::artwork::AlbumArtworkReply {
                    reply,
                    directory_identity: None,
                })
        }
    });
    let resolved = watch_open(id, "Artwork", task, &progress).await?;
    let directory_identity = resolved
        .directory_identity
        .as_deref()
        .map(crate::artwork::directory_identity_hex);
    let cache_control = HeaderValue::from_static("private, no-cache");
    Ok(match resolved.reply {
        crate::artwork::ArtworkReply::NotModified { etag } => {
            let mut response = StatusCode::NOT_MODIFIED.into_response();
            let headers = response.headers_mut();
            headers.insert(header::ETAG, header_value(etag)?);
            headers.insert(header::CACHE_CONTROL, cache_control);
            headers.insert("x-library-id", library_id);
            if let Some(namespace) = album_namespace {
                headers.insert("x-album-id", HeaderValue::from(id));
                headers.insert("x-album-namespace", namespace);
                if let Some(folder_id) = folder_id {
                    headers.insert("x-album-folder-id", HeaderValue::from(folder_id));
                }
                if let Some(identity) = directory_identity {
                    headers.insert("x-album-directory-identity", header_value(identity)?);
                }
            }
            response
        }
        crate::artwork::ArtworkReply::Body { etag, mime, bytes } => {
            let len = bytes.len();
            let mut response = Response::new(Body::from(bytes));
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
            headers.insert(header::ETAG, header_value(etag)?);
            headers.insert(header::CACHE_CONTROL, cache_control);
            headers.insert("x-library-id", library_id);
            if let Some(namespace) = album_namespace {
                headers.insert("x-album-id", HeaderValue::from(id));
                headers.insert("x-album-namespace", namespace);
                if let Some(folder_id) = folder_id {
                    headers.insert("x-album-folder-id", HeaderValue::from(folder_id));
                }
                if let Some(identity) = directory_identity {
                    headers.insert("x-album-directory-identity", header_value(identity)?);
                }
            }
            response
        }
    })
}

/// `Vary: Accept-Encoding` on every `/library/snapshot` and `/audio/{id}/waveform`
/// reply (errors and 304 too).
async fn vary_accept_encoding(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    response
}

/// Whether `Accept-Encoding` accepts `zstd` (any `q` other than 0).
fn accepts_zstd(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|item| {
            let mut parts = item.split(';').map(str::trim);
            let coding = parts.next().unwrap_or_default();
            let zero = parts.any(|param| {
                param
                    .strip_prefix("q=")
                    .or_else(|| param.strip_prefix("Q="))
                    .and_then(|q| q.trim().parse::<f32>().ok())
                    .is_some_and(|q| q <= 0.0)
            });
            coding.eq_ignore_ascii_case("zstd") && !zero
        })
}

#[derive(Deserialize)]
struct SnapshotQuery {
    snapshot_id: Option<String>,
}

/// A pinned response must preserve its encoding, including an explicit identity refusal.
fn accepts_snapshot_encoding(headers: &HeaderMap, encoding: SnapshotEncoding) -> bool {
    let wanted = match encoding {
        SnapshotEncoding::Identity => "identity",
        SnapshotEncoding::Zstd => "zstd",
    };
    let mut explicit = None;
    let mut wildcard = None;
    for item in headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
    {
        let mut parts = item.split(';').map(str::trim);
        let coding = parts.next().unwrap_or_default();
        let quality = parts
            .find_map(|part| {
                part.strip_prefix("q=")
                    .or_else(|| part.strip_prefix("Q="))
                    .and_then(|value| value.trim().parse::<f32>().ok())
            })
            .unwrap_or(1.0);
        if coding.eq_ignore_ascii_case(wanted) {
            explicit = Some(quality > 0.0);
        } else if coding == "*" {
            wildcard = Some(quality > 0.0);
        }
    }
    explicit
        .or(wildcard)
        .unwrap_or(encoding == SnapshotEncoding::Identity)
}

/// `GET /library/snapshot`: latest snapshot, or an exact retained representation selected
/// by `snapshot_id`. Range offsets always refer to the selected encoded file's bytes.
async fn snapshot(
    Extension(state): Extension<AppState>,
    query: Result<Query<SnapshotQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|rejection| {
        ApiError::validation(format!("Invalid snapshot query: {}", rejection.body_text()))
    })?;
    if let Some(id) = &query.snapshot_id
        && !uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == *id)
    {
        return Err(ApiError::validation("snapshot_id must be a canonical UUID"));
    }
    // Refuse unsupported current formats before starting a costly backup. A pinned
    // request is admitted against its own immutable format below instead.
    if query.snapshot_id.is_none() {
        let format_state = state.clone();
        let format = blocking(move || format_state.snapshot_format()).await?;
        crate::compatibility::admit(&headers, &format)?;
    }
    let wanted_encoding = if accepts_zstd(&headers) {
        SnapshotEncoding::Zstd
    } else {
        SnapshotEncoding::Identity
    };
    let permit = state.snapshot_slots().try_acquire_owned().map_err(|_| {
        ApiError::from(audiolib_core::Error::Busy(
            "Too many library snapshots are being prepared; retry shortly".into(),
        ))
    })?;
    let lease = if let Some(id) = query.snapshot_id {
        // A pinned request never polls the revision or prepares a replacement.
        let lookup = state.clone();
        blocking(move || Ok(lookup.retained_snapshot(&id)))
            .await?
            .ok_or_else(ApiError::snapshot_expired)?
    } else {
        // Detached single-flight preparation continues past the response wait budget.
        let (prepared, file) = state
            .snapshot_within_budget(wanted_encoding == SnapshotEncoding::Zstd)
            .await
            .ok_or_else(ApiError::snapshot_preparing)??;
        let retain = state.clone();
        blocking(move || retain.retain_snapshot(prepared, file, wanted_encoding)).await?
    };
    let representation = lease.representation();
    crate::compatibility::admit(&headers, &representation.snapshot_format)?;
    if !accepts_snapshot_encoding(&headers, representation.encoding) {
        return Err(ApiError::new(
            StatusCode::NOT_ACCEPTABLE,
            "SNAPSHOT_ENCODING_NOT_ACCEPTABLE",
            "Accept-Encoding does not accept the retained snapshot representation",
        ));
    }
    let mut file_handle = tokio::fs::File::open(&representation.file.path)
        .await
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ApiError::snapshot_expired()
            } else {
                ApiError::from(audiolib_core::Error::Io(error))
            }
        })?;
    let length = file_handle
        .metadata()
        .await
        .map_err(|error| ApiError::from(audiolib_core::Error::Io(error)))?
        .len();
    if length != representation.file.bytes {
        return Err(ApiError::snapshot_expired());
    }
    let etag = representation.file.etag();
    let disposition = header_value(format!(
        "attachment; filename=\"audiolib-{}-r{}.db\"",
        filename_token(&representation.library_id),
        representation.revision
    ))?;
    let mut response_headers = HeaderMap::new();
    crate::compatibility::response_headers(&mut response_headers, &representation.snapshot_format)?;
    response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response_headers.insert(header::CONTENT_DISPOSITION, disposition);
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response_headers.insert(header::ETAG, header_value(etag.clone())?);
    response_headers.insert(
        "x-snapshot-id",
        header_value(representation.file.id.clone())?,
    );
    response_headers.insert(
        "x-library-id",
        header_value(representation.library_id.clone())?,
    );
    response_headers.insert(
        "x-library-revision",
        header_value(representation.revision.to_string())?,
    );
    response_headers.insert(
        "x-uncompressed-length",
        HeaderValue::from(representation.uncompressed_bytes),
    );
    let header_text = |name: HeaderName| headers.get(name).and_then(|value| value.to_str().ok());
    if header_text(header::IF_NONE_MATCH).is_some_and(|value| stream::none_match(value, &etag)) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().extend(response_headers);
        return Ok(response);
    }
    let range = stream::select_range(
        header_text(header::RANGE),
        header_text(header::IF_RANGE),
        &etag,
        length,
    );
    let Some(body_len) = range.body_len(length) else {
        let mut response = ApiError::range_not_satisfiable(format!(
            "Requested range is outside the {length}-byte snapshot"
        ))
        .into_response();
        response.headers_mut().extend(response_headers);
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            header_value(format!("bytes */{length}"))?,
        );
        return Ok(response);
    };
    let status = if let ByteRange::Partial { start, end } = range {
        file_handle
            .seek(SeekFrom::Start(start))
            .await
            .map_err(|error| ApiError::from(audiolib_core::Error::Io(error)))?;
        response_headers.insert(
            header::CONTENT_RANGE,
            header_value(format!("bytes {start}-{end}/{length}"))?,
        );
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response_headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body_len));
    if representation.encoding == SnapshotEncoding::Zstd {
        response_headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("zstd"));
    }
    tracing::info!(
        bytes = body_len,
        revision = representation.revision,
        encoding = ?representation.encoding,
        "Streaming library snapshot"
    );
    // A compressed response retains only its representation, not the source database.
    let stream = GuardedStream {
        inner: ReaderStream::with_capacity(file_handle.take(body_len), STREAM_CHUNK_BYTES),
        _guard: (lease, permit),
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    response.headers_mut().extend(response_headers);
    Ok(response)
}

#[derive(Deserialize)]
struct BundleQuery {
    encoding: Option<String>,
}

/// `GET /waveforms/bundle?encoding=i8-minmax-base64` (§9.4.2): a read-only SQLite
/// cached immutable snapshot of `waveforms.db`. Preparation and downloads have separate
/// limits; a slow response cannot hold the generation permit.
async fn waveform_bundle(
    Extension(state): Extension<AppState>,
    Query(query): Query<BundleQuery>,
    request_headers: HeaderMap,
) -> Result<Response, ApiError> {
    let encoding = query
        .encoding
        .as_deref()
        .unwrap_or(crate::wfdb::I8_ENCODING);
    if list_waveform::ListEncoding::parse(encoding)? != list_waveform::ListEncoding::I8 {
        return Err(ApiError::unsupported_encoding(format!(
            "bundles exist only for {}",
            crate::wfdb::I8_ENCODING
        )));
    }
    let permit = BUNDLE_DOWNLOAD_SLOTS
        .try_acquire()
        .map_err(|_| ApiError::bundle_busy())?;
    let (bundle, permit) = blocking_api(move || {
        let list = state.list_waveforms();
        let wfdb = list
            .wfdb()
            .ok_or_else(|| ApiError::unavailable("waveforms.db is unavailable"))?;
        let parent = wfdb.path().parent().unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(parent).map_err(|error| {
            tracing::warn!(%error, "Cannot create the list waveform cache directory");
            ApiError::unavailable(format!(
                "The list waveform cache directory cannot be created ({})",
                error.kind()
            ))
        })?;
        let revision = state.revision().map_err(ApiError::from)?;
        let bundle = wfdb
            .prepared_bundle(&revision.library_id, revision.revision)
            .map_err(|error| match error {
                audiolib_core::Error::Busy(_) => ApiError::bundle_busy(),
                other => ApiError::from(other),
            })?;
        Ok((bundle, permit))
    })
    .await?;
    // Conditional request (§9.4.2): a matching If-None-Match (weak comparison, lists
    // and `*`) gets 304 with the ETag and no body; the file is never opened or streamed.
    // `private, no-cache` lets clients store the bundle but forces revalidation.
    let cache_control = HeaderValue::from_static("private, no-cache");
    if request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| stream::none_match(value, &bundle.etag))
    {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        let headers = response.headers_mut();
        headers.insert(header::CACHE_CONTROL, cache_control);
        headers.insert(header::ETAG, header_value(bundle.etag.clone())?);
        headers.insert("x-library-id", header_value(bundle.library_id.clone())?);
        headers.insert(
            "x-library-revision",
            header_value(bundle.revision.to_string())?,
        );
        drop((bundle, permit));
        return Ok(response);
    }
    let file = tokio::fs::File::open(&bundle.path)
        .await
        .map_err(|error| ApiError::internal(format!("Cannot open the bundle: {error}")))?;
    let len = file
        .metadata()
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .len();
    let etag = bundle.etag.clone();
    let library_id = bundle.library_id.clone();
    let revision = bundle.revision;
    let body = bounded_bundle_stream(file, (bundle, permit), BUNDLE_DOWNLOAD_TIMEOUT);
    let mut response = Response::new(Body::from_stream(body));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.sqlite3"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    headers.insert(header::CACHE_CONTROL, cache_control);
    headers.insert(header::ETAG, header_value(etag)?);
    headers.insert("x-library-id", header_value(library_id)?);
    headers.insert("x-library-revision", header_value(revision.to_string())?);
    Ok(response)
}

/// Relay one buffered chunk with a wall-clock deadline. The task owns file/permit/lease,
/// so expiry releases them even when HTTP backpressure stops polling the response body.
fn bounded_bundle_stream(
    mut file: tokio::fs::File,
    guard: impl Send + 'static,
    budget: Duration,
) -> BundleBody {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        let _guard = guard;
        let pump = async {
            loop {
                let mut buffer = vec![0; STREAM_CHUNK_BYTES];
                match file.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(read) => {
                        buffer.truncate(read);
                        if sender.send(Ok(Bytes::from(buffer))).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                        break;
                    }
                }
            }
        };
        if tokio::time::timeout(budget, pump).await.is_err() {
            tracing::warn!("Waveforms bundle download exceeded its time limit");
            let _ = sender.try_send(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Waveforms bundle download timed out",
            )));
        }
    });
    BundleBody { receiver }
}

struct BundleBody {
    receiver: tokio::sync::mpsc::Receiver<io::Result<Bytes>>,
}

impl Stream for BundleBody {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().receiver.poll_recv(context)
    }
}

fn header_value(value: String) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(&value).map_err(|error| ApiError::internal(error.to_string()))
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "NOT_FOUND", "No such endpoint")
}

/// 405 as JSON; axum still adds the route's `Allow` header to this reply.
async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "METHOD_NOT_ALLOWED",
        "Method not allowed for this endpoint",
    )
}

/// Restrict a value to `[A-Za-z0-9._-]` (max 64 chars) for use inside a quoted
/// `Content-Disposition` filename, so the header can never be invalid.
fn filename_token(value: &str) -> String {
    let token: String = value
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if token.is_empty() {
        "library".into()
    } else {
        token
    }
}

/// A byte stream that keeps `_guard` (temporary files, permits) alive until it is dropped.
/// Timing of one `/stream` or `/preview` request, logged as a single info line
/// (`Playback timing`) when the response body ends or is dropped, or when the handler
/// returns without a body (`HEAD`, `304`, errors). A few `Instant::now()` calls per
/// request; nothing per chunk beyond a byte count.
struct PlaybackTrace {
    route: &'static str,
    id: i64,
    head: bool,
    codec: &'static str,
    started: Instant,
    /// Stage boundary of the next measurement.
    mark: Instant,
    slot_wait: Duration,
    open: Option<Duration>,
    transcoder: Option<Duration>,
    first_chunk: Option<Duration>,
    source: Option<(u32, u16, u16)>,
    output: Option<(u32, u16, u16)>,
    bytes: u64,
    complete: bool,
    activity: list_waveform::ListActivity,
}

impl PlaybackTrace {
    fn start(
        activity: list_waveform::ListActivity,
        route: &'static str,
        id: i64,
        head: bool,
        codec: &'static str,
    ) -> Self {
        let now = Instant::now();
        Self {
            route,
            id,
            head,
            codec,
            started: now,
            mark: now,
            slot_wait: Duration::ZERO,
            open: None,
            transcoder: None,
            first_chunk: None,
            source: None,
            output: None,
            bytes: 0,
            complete: false,
            activity,
        }
    }

    /// The stream / preview slot was taken (`try_acquire`: never waits today).
    fn slot_acquired(&mut self) {
        let now = Instant::now();
        self.slot_wait += now - self.mark;
        self.mark = now;
    }

    /// Time spent waiting for a later slot (FLAC transcode slot).
    fn add_slot_wait(&mut self, waited: Duration) {
        self.slot_wait += waited;
        self.mark = Instant::now();
    }

    /// The file is open (root checks, open, and for previews the WAV header read).
    fn opened(&mut self) {
        let now = Instant::now();
        self.open = Some(now - self.mark);
        self.mark = now;
    }

    /// ffmpeg produced its first output (FLAC previews).
    fn transcoder_ready(&mut self) {
        let now = Instant::now();
        self.transcoder = Some(now - self.mark);
        self.mark = now;
    }

    fn format(&mut self, source: (u32, u16, u16), output: (u32, u16, u16)) {
        self.source = Some(source);
        self.output = Some(output);
    }
}

impl Drop for PlaybackTrace {
    fn drop(&mut self) {
        let ms = |duration: Duration| duration.as_millis() as u64;
        let (src_rate, src_bits, src_channels) = self.source.unzip3();
        let (rate, bits, channels) = self.output.unzip3();
        tracing::info!(
            route = self.route,
            id = self.id,
            head = self.head,
            codec = self.codec,
            sample_rate = ?rate,
            bits = ?bits,
            channels = ?channels,
            src_sample_rate = ?src_rate,
            src_bits = ?src_bits,
            src_channels = ?src_channels,
            slot_wait_ms = ms(self.slot_wait),
            open_ms = ?self.open.map(ms),
            transcoder_ms = ?self.transcoder.map(ms),
            first_chunk_ms = ?self.first_chunk.map(ms),
            total_ms = ms(self.started.elapsed()),
            bytes = self.bytes,
            complete = self.complete,
            list_on_demand_in_flight = self.activity.on_demand_in_flight,
            list_pregen_in_flight = self.activity.pregen_in_flight,
            list_outstanding_stats = self.activity.outstanding_stats,
            "Playback timing"
        );
    }
}

trait Unzip3 {
    fn unzip3(self) -> (Option<u32>, Option<u16>, Option<u16>);
}

impl Unzip3 for Option<(u32, u16, u16)> {
    fn unzip3(self) -> (Option<u32>, Option<u16>, Option<u16>) {
        match self {
            Some((a, b, c)) => (Some(a), Some(b), Some(c)),
            None => (None, None, None),
        }
    }
}

/// A response body that records its first chunk, byte count and end in a
/// [`PlaybackTrace`] (logged when the body is dropped).
struct Traced<S> {
    inner: S,
    trace: PlaybackTrace,
}

impl<S> Traced<S> {
    fn new(inner: S, trace: PlaybackTrace) -> Self {
        Self { inner, trace }
    }
}

impl<S: Stream<Item = io::Result<Bytes>> + Unpin> Stream for Traced<S> {
    type Item = io::Result<Bytes>;
    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_next(context);
        match &poll {
            Poll::Ready(Some(Ok(chunk))) => {
                if this.trace.first_chunk.is_none() {
                    this.trace.first_chunk = Some(this.trace.started.elapsed());
                }
                this.trace.bytes += chunk.len() as u64;
            }
            Poll::Ready(None) => this.trace.complete = true,
            _ => {}
        }
        poll
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

struct GuardedStream<S, G> {
    inner: S,
    _guard: G,
}

impl<S: Stream + Unpin, G: Unpin> Stream for GuardedStream<S, G> {
    type Item = S::Item;
    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<S::Item>> {
        Pin::new(&mut self.get_mut().inner).poll_next(context)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unpolled_bundle_body_releases_its_download_lease_at_deadline() {
        let file = tempfile::tempfile().unwrap();
        file.set_len((STREAM_CHUNK_BYTES * 4) as u64).unwrap();
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = slots.clone().acquire_owned().await.unwrap();
        let body = bounded_bundle_stream(
            tokio::fs::File::from_std(file),
            permit,
            Duration::from_millis(30),
        );
        // Never poll the body: a stream-local timeout would never wake up here.
        let released = tokio::time::timeout(Duration::from_secs(2), slots.acquire())
            .await
            .expect("an unpolled download retained its lease")
            .unwrap();
        drop(released);
        drop(body);
    }

    struct Chunks(Vec<io::Result<Bytes>>);

    impl Stream for Chunks {
        type Item = io::Result<Bytes>;
        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let chunks = &mut self.get_mut().0;
            Poll::Ready((!chunks.is_empty()).then(|| chunks.remove(0)))
        }
    }

    #[test]
    fn traced_bodies_record_first_chunk_bytes_and_completion() {
        let mut trace = PlaybackTrace::start(
            list_waveform::ListActivity::default(),
            "preview",
            7,
            false,
            "pcm",
        );
        trace.slot_acquired();
        trace.opened();
        trace.format((96_000, 24, 2), (48_000, 16, 2));
        assert!(trace.open.is_some());
        let mut body = Traced::new(
            Chunks(vec![
                Ok(Bytes::from_static(b"abc")),
                Ok(Bytes::from_static(b"de")),
            ]),
            trace,
        );
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(body.trace.first_chunk.is_none());
        while let Poll::Ready(Some(_)) = Pin::new(&mut body).poll_next(&mut context) {
            assert!(body.trace.first_chunk.is_some());
        }
        assert_eq!(body.trace.bytes, 5);
        assert!(body.trace.complete);
        assert_eq!(body.trace.output, Some((48_000, 16, 2)));
    }

    #[test]
    fn management_admission_matches_every_managed_route_and_exact_method() {
        // Keep this table aligned with all route_layer(manage()) registrations above.
        for (method, path) in [
            (Method::GET, "/fs"),
            (Method::HEAD, "/fs"),
            (Method::POST, "/roots"),
            (Method::PATCH, "/roots/{name}"),
            (Method::DELETE, "/roots/{name}"),
            (Method::POST, "/audio/remove"),
            (Method::DELETE, "/folders/{id}"),
            (Method::POST, "/scans"),
            (Method::POST, "/scans/{session_id}/pause"),
            (Method::POST, "/scans/{session_id}/resume"),
            (Method::POST, "/scans/{session_id}/cancel"),
            (Method::POST, "/waveforms/pregeneration/pause"),
            (Method::POST, "/waveforms/pregeneration/resume"),
            (Method::PATCH, "/settings"),
        ] {
            assert!(managed_route(&method, path), "{method} {path}");
            assert!(
                managed_route(&method, &format!("{API_BASE}{path}")),
                "nested {method} {path}"
            );
        }
        for (method, path) in [
            (Method::GET, "/roots"),
            (Method::HEAD, "/roots"),
            (Method::POST, "/roots/{name}/check"),
            (Method::GET, "/library"),
            (Method::GET, "/library/snapshot"),
            (Method::GET, "/capabilities"),
            (Method::GET, "/audio/{id}/stream"),
            (Method::HEAD, "/audio/{id}/stream"),
            (Method::GET, "/audio/{id}/preview"),
            (Method::GET, "/audio/{id}/waveform"),
            (Method::GET, "/audio/{id}/artwork"),
            (Method::HEAD, "/audio/{id}/artwork"),
            (Method::PATCH, "/audio/{id}/metadata"),
            (Method::POST, "/audio/metadata/batch"),
            (Method::POST, "/audio/{id}/plays"),
            (Method::GET, "/history/scans"),
            (Method::GET, "/history/plays"),
            (Method::GET, "/scans/current"),
            (Method::GET, "/scans/{session_id}"),
            (Method::GET, "/scans/{session_id}/pause"),
            (Method::GET, "/audio/remove"),
            (Method::POST, "/fs"),
            (Method::POST, "/folders/{id}"),
            (Method::PUT, "/roots/{name}"),
            (Method::POST, "/waveforms"),
            (Method::GET, "/waveforms/status"),
            (Method::GET, "/settings"),
        ] {
            assert!(!managed_route(&method, path), "{method} {path}");
        }
    }

    #[test]
    fn filename_tokens_are_header_safe() {
        assert_eq!(
            filename_token("c8385998-114b-45b0-9949-9e79ae7c08cc"),
            "c8385998-114b-45b0-9949-9e79ae7c08cc"
        );
        assert_eq!(filename_token("a\"b\r\nc/é"), "a_b__c__");
        assert_eq!(filename_token(""), "library");
        assert_eq!(filename_token(&"x".repeat(200)).len(), 64);
        let header = format!(
            "attachment; filename=\"{}\"",
            filename_token("\u{7f}\0\"\n")
        );
        assert!(HeaderValue::from_str(&header).is_ok());
    }
}
