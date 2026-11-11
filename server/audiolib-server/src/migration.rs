// server/audiolib-server/src/migration.rs
//! Admin migration JSON and online read/write availability rules.
use crate::{ApiError, AppState, Service};
use audiolib_core::{DataProvider, SearchRequest, SearchScope};
use audiolib_db::migration9::{self, Action, Failure, Options, Report, State as MigrationState};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{sync::PoisonError, time::Duration};

pub(crate) const HEADER: &str = audiolib_core::server_upgrade::MIGRATION_HEADER;

/// A single canonical main response, including its original query diagnostics.
pub(crate) struct CachedSearch {
    revision: u64,
    request: Vec<u8>,
    body: Bytes,
}

fn search_body(
    state: &AppState,
    request: &SearchRequest,
    cache: bool,
) -> audiolib_core::Result<Bytes> {
    let json_error = |error: serde_json::Error| audiolib_core::Error::Database(error.to_string());
    if !cache {
        state
            .migration_search_cache()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        return Ok(serde_json::to_vec(&state.provider().search(request)?)
            .map_err(json_error)?
            .into());
    }
    let key = serde_json::to_vec(request).map_err(json_error)?;
    let revision = state.provider().search_revision()?;
    {
        let saved = state
            .migration_search_cache()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(saved) = saved.as_ref()
            && Some(saved.revision) == revision
            && saved.request == key
        {
            return Ok(saved.body.clone());
        }
    }
    let page = state.provider().search(request)?;
    let body: Bytes = serde_json::to_vec(&page).map_err(json_error)?.into();
    // Never retain an oversized Metadata page or publish a response across a
    // commit. The dedicated provider observer covers own and external writes.
    if body.len() <= 4 * 1024 * 1024
        && let Some(revision) = revision
        && state.provider().search_revision()? == Some(revision)
    {
        *state
            .migration_search_cache()
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(CachedSearch {
            revision,
            request: key,
            body: body.clone(),
        });
    }
    Ok(body)
}

/// Explicitly unauthenticated, native migration clients on a loopback listener.
/// Default authentication, forwarded traffic and browser requests keep the admin guard.
pub(crate) fn local_admin(request: &Request, security: &crate::Security) -> bool {
    let headers = request.headers();
    request.uri().path().starts_with("/admin/api/migration/")
        && security.auth_mode() == crate::auth::AuthMode::Off
        && request
            .extensions()
            .get::<crate::ConnectionInfo>()
            .is_some_and(|info| {
                info.peer.ip().to_canonical().is_loopback()
                    && info
                        .local
                        .is_some_and(|local| local.ip().to_canonical().is_loopback())
            })
        && !crate::auth::has_forwarding_headers(headers)
        && !headers.contains_key("origin")
        && !headers.contains_key("sec-fetch-site")
        && (matches!(*request.method(), Method::GET | Method::HEAD)
            || headers
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.split(';').next() == Some("application/json")))
}

pub(crate) async fn status(
    State(service): State<Service>,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
) -> Result<Response, ApiError> {
    service
        .security()
        .authorize_manage(&headers, extensions.get::<crate::ConnectionInfo>())?;
    let state = service.app_state()?;
    let cancel = service.migration_cancellation();
    let report = tokio::task::spawn_blocking(move || {
        migration9::execute(
            state.provider().database_path(),
            Action::Status,
            &Options::default(),
            &cancel,
        )
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(report_response(StatusCode::OK, report))
}

pub(crate) async fn action(
    State(service): State<Service>,
    Path(action): Path<String>,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
) -> Result<Response, ApiError> {
    // The admin router admits LAN clients or native loopback clients with auth off.
    // Both paths still require the configured management permission.
    let connection = extensions.get::<crate::ConnectionInfo>();
    service.security().authorize_manage(&headers, connection)?;
    let action: Action = action.parse()?;
    if matches!(action, Action::Startup | Action::Status) {
        return Err(ApiError::validation("Unsupported admin action"));
    }
    let state = service.app_state()?;
    let path = state.provider().database_path().to_path_buf();
    let work_path = path.clone();
    let cancel = service.migration_cancellation();
    let mut worker = tokio::task::spawn_blocking(move || {
        migration9::execute(&work_path, action, &Options::default(), &cancel)
    });
    let report = if matches!(action, Action::Start | Action::Resume) {
        match tokio::time::timeout(Duration::from_millis(100), &mut worker).await {
            Ok(result) => result.map_err(|e| ApiError::internal(e.to_string()))?,
            Err(_) => tokio::task::spawn_blocking(move || migration9::status(&path))
                .await
                .map_err(|e| ApiError::internal(e.to_string()))??,
        }
    } else {
        worker
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
    };
    let code = match report.error_code {
        Some(Failure::AlreadyRunning | Failure::Locked | Failure::NotMigrated) => {
            StatusCode::CONFLICT
        }
        Some(_) => StatusCode::UNPROCESSABLE_ENTITY,
        None => StatusCode::OK,
    };
    Ok(report_response(code, report))
}

fn report_response(code: StatusCode, report: Report) -> Response {
    let state = report.state;
    let mut response = (code, Json(report)).into_response();
    response.extensions_mut().insert(state);
    response
}

/// Read status on the blocking pool and attach the canonical header even to errors.
pub(crate) async fn headers(
    State(service): State<Service>,
    mut request: Request,
    next: Next,
) -> Response {
    let report = match service.app_state() {
        Ok(state) => tokio::task::spawn_blocking(move || {
            migration9::status(state.provider().database_path())
        })
        .await
        .ok()
        .and_then(Result::ok),
        Err(_) => None,
    };
    if let Some(report) = &report {
        request.extensions_mut().insert(report.clone());
    }
    // Reserve the documented vendor-import write boundary during publication,
    // including before an importer route is installed. Normal routing owns all
    // other states. Authentication and management policy still precede rejection.
    let vendor = request.uri().path() == "/api/vendor-import"
        || request.uri().path().starts_with("/api/vendor-import/");
    let mut response = if vendor && let Some(response) = switching_write(&request) {
        let connection = request.extensions().get::<crate::ConnectionInfo>();
        match service
            .security()
            .authorize(request.headers(), connection)
            .and_then(|()| {
                service
                    .security()
                    .authorize_manage(request.headers(), connection)
            }) {
            Ok(()) => response,
            Err(error) => error.into_response(),
        }
    } else {
        next.run(request).await
    };
    if let Some(state) = response
        .extensions()
        .get::<MigrationState>()
        .copied()
        .or_else(|| report.map(|r| r.state))
        && !matches!(state, MigrationState::Idle | MigrationState::Done)
    {
        response
            .headers_mut()
            .insert(HEADER, HeaderValue::from_static(state.as_str()));
    }
    response
}
pub(crate) fn switching_write(request: &Request) -> Option<Response> {
    let path = request.uri().path();
    let read_post = path.ends_with("/search") || path.ends_with("/waveforms");
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) && !read_post
        && request
            .extensions()
            .get::<Report>()
            .is_some_and(|r| r.state == MigrationState::Switching)
    {
        Some(unavailable("migration_switching", "30"))
    } else {
        None
    }
}
fn unavailable(code: &'static str, retry: &'static str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [("retry-after", retry)],
        Json(serde_json::json!({"error_code": code})),
    )
        .into_response()
}
pub(crate) async fn search(
    state: AppState,
    mut request: SearchRequest,
) -> Result<Response, ApiError> {
    let (report, page) = tokio::task::spawn_blocking(move || -> audiolib_core::Result<_> {
        let report = migration9::status(state.provider().database_path())?;
        let unavailable = report.switch.as_ref().is_none_or(|s| s.user_version != 9);
        if unavailable && request.scope == SearchScope::Path {
            return Ok((true, None));
        }
        let fallback = unavailable && request.scope == SearchScope::All;
        if fallback {
            request.scope = SearchScope::Main;
        }
        let cache = unavailable
            && matches!(
                report.state,
                MigrationState::Preflight
                    | MigrationState::BackingUp
                    | MigrationState::Running
                    | MigrationState::Paused
                    | MigrationState::Switching
            );
        Ok((fallback, Some(search_body(&state, &request, cache)?)))
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))??;
    let Some(page) = page else {
        return Ok(unavailable("migration_in_progress", "30"));
    };
    let mut response = ([("content-type", "application/json")], page).into_response();
    if report {
        response.headers_mut().insert(
            "x-audiolib-path-search",
            HeaderValue::from_static("unavailable"),
        );
    }
    Ok(response)
}
