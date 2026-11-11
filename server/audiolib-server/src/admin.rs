// server/audiolib-server/src/admin.rs
//! LAN-only web admin page (`/admin`) and its JSON API (`/admin/api/...`): public URL,
//! authentication mode, library management mode, generated tokens, the optional
//! native TLS listener and the library roots (server disk browser, add / remove roots,
//! scans of a root; the admin guard below is the authority).
//!
//! The default guard requires LAN-direct access (socket peer in a private
//! range and no forwarding header, see [`crate::auth::classify`]; public peers of the TLS
//! listener are external) and its `Host` must be an IP address or a LAN name (DNS
//! rebinding protection). Changing requests additionally need `X-AudioLib-Admin: 1` and,
//! when the browser sends `Origin`, the same origin. Otherwise 403. There is no password.
//! Native loopback migration requests with authentication explicitly off have the
//! narrow exception documented by [`crate::migration::local_admin`].
use crate::{
    ApiError, VERSION,
    auth::{
        AdminSettings, AuthMode, ConnectionInfo, ManageMode, RequestOrigin, Source, TlsSettings,
        TokenInfo, lan_ipv4_addresses, normalize_public_url,
    },
    manage::{AddRootRequest, RemoveRootResponse},
    routes::json_body,
    scans::{CurrentScanResponse, ScanAction, ScanRequest, ScanResponse},
    service::{Readiness, Service},
    state::RootResponse,
    tls,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Path, Query, Request, State,
        rejection::{BytesRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::{Deserialize, Deserializer, Serialize};
use std::{sync::Arc, time::Duration};

/// Header every changing admin request must carry with value `1`.
pub const ADMIN_HEADER: &str = "x-audiolib-admin";
const PAGE_HTML: &str = include_str!("admin/index.html");
const PAGE_JS: &str = include_str!("admin/admin.js");
const MAX_ADMIN_BODY_BYTES: usize = 64 * 1024;
/// How long a TLS change waits for the listener to report its new state.
const TLS_APPLY_WAIT: Duration = Duration::from_secs(4);
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; \
    style-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; \
    frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// Admin routes (merged at the root, outside `/api/v1`), guarded by [`guard`].
pub fn router(security: Arc<crate::Security>) -> Router<Service> {
    Router::new()
        .route("/admin", get(page))
        .route("/admin/", get(page))
        .route("/admin/admin.js", get(script))
        .route("/admin/api/status", get(status))
        .route("/admin/api/runtime", get(runtime))
        .route("/admin/api/migration/status", get(crate::migration::status))
        .route(
            "/admin/api/migration/{action}",
            post(crate::migration::action),
        )
        .route("/admin/api/update", get(update_status))
        .route("/admin/api/update/check", post(check_update))
        .route("/admin/api/update/install", post(install_update))
        .route("/admin/api/settings", put(update_settings))
        .route("/admin/api/tls", put(update_tls).delete(disable_tls))
        .route("/admin/api/fs", get(browse_fs))
        .route("/admin/api/roots", get(list_roots).post(add_root))
        .route(
            "/admin/api/roots/{name}",
            axum::routing::delete(remove_root),
        )
        .route("/admin/api/scans", post(start_scan))
        .route("/admin/api/scans/current", get(current_scan))
        .route("/admin/api/scans/{id}", get(scan_status))
        .route("/admin/api/scans/{id}/pause", post(pause_scan))
        .route("/admin/api/scans/{id}/resume", post(resume_scan))
        .route("/admin/api/scans/{id}/cancel", post(cancel_scan))
        .route("/admin/api/tokens", get(list_tokens).post(create_token))
        .route(
            "/admin/api/tokens/{id}",
            axum::routing::delete(revoke_token),
        )
        .layer(DefaultBodyLimit::max(MAX_ADMIN_BODY_BYTES))
        .route_layer(middleware::from_fn_with_state(security, guard))
}

/// LAN-only, LAN-host-only, and header + same-origin for changes; adds no-store and
/// anti-framing headers to every admin response.
async fn guard(
    State(security): State<Arc<crate::Security>>,
    request: Request,
    next: Next,
) -> Response {
    if let Err(error) = check(&request, &security) {
        return with_admin_headers(error.into_response());
    }
    if !request.uri().path().starts_with("/admin/api/migration/")
        && let Some(response) = crate::migration::switching_write(&request)
    {
        return with_admin_headers(response);
    }
    with_admin_headers(next.run(request).await)
}

fn check(request: &Request, security: &crate::Security) -> Result<(), ApiError> {
    let headers = request.headers();
    let connection = request.extensions().get::<ConnectionInfo>();
    let local_migration = crate::migration::local_admin(request, security);
    if !local_migration && security.classify(headers, connection) == RequestOrigin::External {
        return Err(ApiError::admin_lan_only(
            "The admin page is only available directly from the LAN (not through a proxy)",
        ));
    }
    let mut hosts = headers.get_all(header::HOST).iter();
    let host = hosts.next().and_then(|value| value.to_str().ok());
    if hosts.next().is_some()
        || match host {
            Some(host) => !is_lan_host(host),
            None => connection.is_some(),
        }
    {
        return Err(ApiError::admin_lan_only(
            "Open the admin page directly using the server's IP address, localhost or a LAN hostname, with the configured port",
        ));
    }
    let changing = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if changing {
        let marked = headers
            .get(ADMIN_HEADER)
            .is_some_and(|value| value.as_bytes() == b"1");
        let same_origin = crate::auth::same_request_origin(request);
        if (!marked && !local_migration) || !same_origin {
            return Err(ApiError::admin_header_required());
        }
    }
    Ok(())
}

/// IP literal, `localhost`, a single-label name or a `.local` / `.lan` / `.home.arpa` /
/// `.internal` / `.localhost` name (with or without port). Public DNS names are refused
/// so a rebinding attack page cannot reach the admin API.
pub fn is_lan_host(host: &str) -> bool {
    crate::auth::is_lan_host(host)
}

fn with_admin_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

async fn page() -> Response {
    let mut response = (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        PAGE_HTML,
    )
        .into_response();
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    response
}

async fn script() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        PAGE_JS,
    )
        .into_response()
}

/// One detected LAN address.
#[derive(Debug, Serialize)]
struct LanCandidate {
    interface: String,
    url: String,
}

/// Effective TLS state for the page.
#[derive(Debug, Serialize)]
struct TlsView {
    configured: Option<TlsSettings>,
    source: Source,
    state: String,
    port: Option<u16>,
    message: Option<String>,
}

/// `GET /admin/api/status`.
#[derive(Debug, Serialize)]
struct AdminStatus {
    version: &'static str,
    library: &'static str,
    auth_mode: AuthMode,
    auth_mode_source: Source,
    manage_mode: ManageMode,
    manage_mode_source: Source,
    pregenerate_list_waveforms: bool,
    pregenerate_list_waveforms_source: Source,
    list_waveform_rate: u32,
    list_waveform_rate_source: Source,
    public_url: Option<String>,
    public_url_source: Source,
    lan_url: Option<String>,
    lan_url_source: &'static str,
    lan_url_candidates: Vec<LanCandidate>,
    http_port: u16,
    env_token: bool,
    token_count: usize,
    tls: TlsView,
    settings_file: Option<String>,
    tokens_file: Option<String>,
    warnings: Vec<String>,
}

fn admin_status(service: &Service) -> AdminStatus {
    let security = service.security();
    let (auth_mode, auth_mode_source) = security.auth_mode_with_source();
    let (manage_mode, manage_mode_source) = security.manage_mode_with_source();
    let (pregenerate_list_waveforms, pregenerate_list_waveforms_source) =
        security.pregenerate_list_waveforms_with_source();
    let (list_waveform_rate, list_waveform_rate_source) = security.list_waveform_rate_with_source();
    let (public_url, public_url_source) = security.public_url_with_source();
    let (configured, source) = security.tls_with_source();
    let tls_status = security.tls_status();
    let http_port = security.http_port();
    let lan_url_candidates: Vec<LanCandidate> = lan_ipv4_addresses()
        .into_iter()
        .map(|(interface, ip)| LanCandidate {
            interface,
            url: format!("http://{ip}:{http_port}"),
        })
        .collect();
    let (lan_url, lan_url_source) = match security.lan_url_override() {
        Some(url) => (Some(url), "admin"),
        None => match lan_url_candidates.first() {
            Some(candidate) => (Some(candidate.url.clone()), "detected"),
            None => (None, "none"),
        },
    };
    let files = security.files();
    AdminStatus {
        version: VERSION,
        library: match service.readiness() {
            Readiness::Ready(_) => "ok",
            Readiness::Starting => "starting",
            Readiness::Failed(_) => "error",
        },
        auth_mode,
        auth_mode_source,
        manage_mode,
        manage_mode_source,
        pregenerate_list_waveforms,
        pregenerate_list_waveforms_source,
        list_waveform_rate,
        list_waveform_rate_source,
        public_url,
        public_url_source,
        lan_url,
        lan_url_source,
        lan_url_candidates,
        http_port,
        env_token: security.env_token().is_some(),
        token_count: security.token_count(),
        tls: TlsView {
            configured,
            source,
            state: tls_status.state,
            port: tls_status.port,
            message: tls_status.message,
        },
        settings_file: files
            .as_ref()
            .map(|(settings, _)| settings.display().to_string()),
        tokens_file: files.map(|(_, tokens)| tokens.display().to_string()),
        warnings: security.warnings().to_vec(),
    }
}

async fn status(State(service): State<Service>) -> Json<AdminStatus> {
    Json(admin_status(&service))
}

async fn update_status(State(service): State<Service>) -> Json<crate::update::UpdateStatus> {
    Json(service.updates().status())
}

async fn check_update(
    State(service): State<Service>,
) -> Result<Json<crate::update::UpdateStatus>, ApiError> {
    service.updates().check().map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallUpdateBody {
    tag: String,
}

async fn install_update(
    State(service): State<Service>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<crate::update::UpdateStatus>, ApiError> {
    let body: InstallUpdateBody = json_body(&headers, body)?;
    // Updating can repair a failed/opening library. Startup cancellation and its
    // durable-boundary drain are checked by main before replacing any executable.
    // Explicitly requested scans must still finish or be cancelled by the admin.
    if let Readiness::Ready(state) = service.readiness()
        && state.scans().current().is_some()
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "UPDATE_BUSY",
            "Wait for the current scan to finish or cancel it before updating",
        ));
    }
    service.updates().install(&body.tag).map(Json)
}

/// `Some(None)` for an explicit JSON `null`, `None` for an absent field.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Ok(Some(Option::deserialize(deserializer)?))
}

/// Waveform cache usage of `GET /admin/api/runtime`.
#[derive(Debug, Serialize)]
struct CacheUsage {
    /// Estimate (bytes measured by the last periodic trim plus writes since); `null`
    /// until the first trim since startup.
    estimated_bytes: Option<u64>,
    limit_bytes: u64,
}

/// One root of `GET /admin/api/runtime` (cached monitor state, no filesystem access).
#[derive(Debug, Serialize)]
struct RootRuntime {
    name: String,
    online: bool,
    state: crate::roots::RootState,
    message: Option<String>,
    checked_at: Option<u64>,
}

/// Snapshot and revision part of `GET /admin/api/runtime`.
#[derive(Debug, Serialize)]
struct SnapshotView {
    library_id: String,
    revision: u64,
    #[serde(flatten)]
    snapshot: crate::state::SnapshotRuntime,
}

/// Last update check part of `GET /admin/api/runtime`.
#[derive(Debug, Serialize)]
struct UpdateRuntime {
    state: String,
    channel: Option<&'static str>,
    current_version: String,
    /// Release tag selected by the last check, if any.
    selected: Option<String>,
    /// Why no release is offered: `disabled`, `up_to_date`, `error` or `null`.
    skip_reason: Option<&'static str>,
    message: String,
    checked_at: Option<u64>,
}

/// `GET /admin/api/runtime`: in-memory state only (semaphores, scheduler, caches'
/// counters, root monitor, revision tracker); never scans, walks or queries the library.
#[derive(Debug, Serialize)]
struct RuntimeStatus {
    library: &'static str,
    list_waveforms: Option<crate::list_waveform::ListWaveformRuntime>,
    slots: Option<crate::state::SlotUsage>,
    waveform_cache: Option<CacheUsage>,
    snapshot: Option<SnapshotView>,
    roots: Vec<RootRuntime>,
    update: UpdateRuntime,
}

async fn runtime(State(service): State<Service>) -> Result<Json<RuntimeStatus>, ApiError> {
    let update = service.updates().status();
    let skip_reason = match update.state.as_str() {
        "disabled" => Some("disabled"),
        "current" => Some("up_to_date"),
        "error" => Some("error"),
        _ => None,
    };
    let update = UpdateRuntime {
        skip_reason,
        selected: update.tag,
        state: update.state,
        channel: update.channel,
        current_version: update.current_version,
        message: update.message,
        checked_at: update.checked_at,
    };
    let readiness = service.readiness();
    let library = match &readiness {
        Readiness::Ready(_) => "ok",
        Readiness::Starting => "starting",
        Readiness::Failed(_) => "error",
    };
    let Readiness::Ready(state) = readiness else {
        return Ok(Json(RuntimeStatus {
            library,
            list_waveforms: None,
            slots: None,
            waveform_cache: None,
            snapshot: None,
            roots: Vec::new(),
            update,
        }));
    };
    let monitor = state.root_monitor();
    let roots = monitor
        .roots()
        .iter()
        .enumerate()
        .map(|(index, root)| {
            let status = monitor.status(index);
            RootRuntime {
                name: root.name.clone(),
                online: status.online(),
                state: status.state,
                message: status.message,
                checked_at: status.checked_at,
            }
        })
        .collect();
    let revision = state.tracker().current()?;
    let (estimated_bytes, limit_bytes) = state.waveform_cache_usage();
    Ok(Json(RuntimeStatus {
        library,
        list_waveforms: Some(state.list_waveforms().runtime(&state)),
        slots: Some(state.slot_usage()),
        waveform_cache: Some(CacheUsage {
            estimated_bytes,
            limit_bytes,
        }),
        snapshot: Some(SnapshotView {
            library_id: revision.library_id,
            revision: revision.revision,
            snapshot: state.snapshot_runtime(),
        }),
        roots,
        update,
    }))
}

/// `PUT /admin/api/settings`: only the fields present change; `null` or `""` removes the
/// admin override (back to `config.toml`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsBody {
    #[serde(default, deserialize_with = "present")]
    public_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    auth_mode: Option<Option<AuthMode>>,
    #[serde(default, deserialize_with = "present")]
    lan_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    manage_mode: Option<Option<ManageMode>>,
    #[serde(default, deserialize_with = "bool_or_empty")]
    pregenerate_list_waveforms: Option<Option<bool>>,
    #[serde(default, deserialize_with = "rate_or_empty")]
    list_waveform_rate: Option<Option<u32>>,
}

/// `list_waveform_rate` of the admin page: 1..=1000, or `null` / `""` to remove the
/// admin override (§9.5.4).
fn rate_or_empty<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<u32>>, D::Error> {
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None | Some(serde_json::Value::Null) => Ok(Some(None)),
        Some(serde_json::Value::String(text)) if text.is_empty() => Ok(Some(None)),
        Some(serde_json::Value::Number(n))
            if n.as_u64().is_some_and(|v| (1..=1000).contains(&v)) =>
        {
            Ok(Some(Some(n.as_u64().unwrap_or(1) as u32)))
        }
        Some(other) => Err(serde::de::Error::custom(format!(
            "list_waveform_rate must be 1..=1000, null or \"\", got {other}"
        ))),
    }
}

/// A present boolean field where `null` or `""` removes the admin override.
fn bool_or_empty<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<bool>>, D::Error> {
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None | Some(serde_json::Value::Null) => Ok(Some(None)),
        Some(serde_json::Value::Bool(value)) => Ok(Some(Some(value))),
        Some(serde_json::Value::String(text)) if text.is_empty() => Ok(Some(None)),
        Some(other) => Err(serde::de::Error::custom(format!(
            "pregenerate_list_waveforms must be true, false, null or \"\", got {other}"
        ))),
    }
}

fn url_field(value: Option<String>) -> Result<Option<String>, ApiError> {
    match value.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(text) => normalize_public_url(text)
            .map(Some)
            .map_err(ApiError::validation),
    }
}

/// How long a disk-browser request waits for the file system.
const FS_WAIT: Duration = Duration::from_secs(15);

/// Await blocking file-system work for at most [`FS_WAIT`]. A hung `opendir` (sleeping
/// disk, dead network share, pending macOS privacy prompt) stays blocked on its own
/// thread; the admin page gets a clear 504 instead of a generic timeout.
pub(crate) async fn with_fs_timeout<T>(
    path: &str,
    work: impl std::future::Future<Output = Result<T, ApiError>>,
) -> Result<T, ApiError> {
    match tokio::time::timeout(FS_WAIT, work).await {
        Ok(result) => result,
        Err(_) => Err(ApiError::fs_timeout(format!(
            "读取 {path} 超时（{} 秒无响应）：磁盘可能在休眠、网络共享无响应，或 macOS 正在等待“访问网络卷/文件夹”的权限弹窗——请在服务器屏幕上点击允许后重试",
            FS_WAIT.as_secs()
        ))),
    }
}

/// Concurrent admin file-system calls (disk browser, adding a root). A call that hangs
/// on a dead share keeps its slot until the kernel returns, even after the 15 s
/// [`FS_WAIT`] answer, so repeated clicks cannot pile up blocked threads.
const FS_SLOT_COUNT: usize = 4;
static FS_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(FS_SLOT_COUNT);
/// Reserved for the administrator: client API filesystem requests cannot acquire it.
static ADMIN_FS_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

async fn run_admin_fs_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let permit = ADMIN_FS_SLOTS
        .try_acquire()
        .map_err(|_| ApiError::fs_busy("An administrator filesystem operation is still pending"))?;
    run_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
}

/// [`run_blocking`] holding one of the [`FS_SLOTS`] for as long as `work` runs; 503
/// `BUSY` when all are taken.
pub(crate) async fn run_fs_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let permit = FS_SLOTS.try_acquire().map_err(|_| {
        ApiError::fs_busy(format!(
            "已有 {FS_SLOT_COUNT} 个磁盘读取仍未返回（磁盘休眠或网络共享无响应），请稍后重试"
        ))
    })?;
    run_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
}

pub(crate) async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ApiError::internal(format!("Admin task failed: {error}")))?
}

async fn update_settings(
    State(service): State<Service>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<AdminStatus>, ApiError> {
    let body: SettingsBody = json_body(&headers, body)?;
    let public_url = body.public_url.map(url_field).transpose()?;
    let lan_url = body.lan_url.map(url_field).transpose()?;
    let auth_mode = body.auth_mode;
    let manage_mode = body.manage_mode;
    let pregenerate = body.pregenerate_list_waveforms;
    let rate = body.list_waveform_rate;
    let security = service.security().clone();
    run_blocking(move || {
        security.update_settings(|settings: &mut AdminSettings| {
            if let Some(value) = public_url {
                settings.public_url = value;
            }
            if let Some(value) = lan_url {
                settings.lan_url = value;
            }
            if let Some(value) = auth_mode {
                settings.auth_mode = value;
            }
            if let Some(value) = manage_mode {
                settings.manage_mode = value;
            }
            if let Some(value) = pregenerate {
                settings.pregenerate_list_waveforms = value;
            }
            if let Some(value) = rate {
                settings.list_waveform_rate = value;
            }
        })
    })
    .await?;
    tracing::info!("Admin settings changed");
    Ok(Json(admin_status(&service)))
}

/// Wait until the TLS manager reports the newest settings (or give up after a while,
/// e.g. when no manager runs); the page shows whatever state is current.
async fn wait_for_tls(service: &Service) {
    let security = service.security();
    let generation = security.tls_watch().borrow().0;
    let deadline = tokio::time::Instant::now() + TLS_APPLY_WAIT;
    while tokio::time::Instant::now() < deadline {
        let status = security.tls_status();
        if status.generation >= generation && status.state != "starting" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn update_tls(
    State(service): State<Service>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<AdminStatus>, ApiError> {
    let settings: TlsSettings = json_body(&headers, body)?;
    let security = service.security().clone();
    let http_port = security.http_port();
    run_blocking(move || {
        tls::validate(&settings, http_port).map_err(ApiError::validation)?;
        security.update_settings(|admin| admin.tls = Some(settings))
    })
    .await?;
    wait_for_tls(&service).await;
    Ok(Json(admin_status(&service)))
}

async fn disable_tls(State(service): State<Service>) -> Result<Json<AdminStatus>, ApiError> {
    let security = service.security().clone();
    run_blocking(move || security.update_settings(|admin| admin.tls = None)).await?;
    wait_for_tls(&service).await;
    Ok(Json(admin_status(&service)))
}

#[derive(Debug, Serialize)]
struct TokenList {
    tokens: Vec<TokenInfo>,
}

async fn list_tokens(State(service): State<Service>) -> Json<TokenList> {
    Json(TokenList {
        tokens: service.security().tokens(),
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTokenBody {
    name: String,
}

/// `POST /admin/api/tokens` reply: the plaintext `secret` appears only here.
#[derive(Debug, Serialize)]
struct CreatedToken {
    token: TokenInfo,
    secret: String,
}

async fn create_token(
    State(service): State<Service>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let body: CreateTokenBody = json_body(&headers, body)?;
    let security = service.security().clone();
    let (token, secret) = run_blocking(move || security.create_token(&body.name)).await?;
    Ok((StatusCode::CREATED, Json(CreatedToken { token, secret })).into_response())
}

async fn revoke_token(
    State(service): State<Service>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let security = service.security().clone();
    if run_blocking(move || security.revoke_token(&id)).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("No such token"))
    }
}

/// Query of `GET /admin/api/fs`.
#[derive(Debug, Deserialize)]
struct FsQuery {
    path: Option<String>,
    hidden: Option<String>,
}

/// `GET /admin/api/fs[?path=<absolute directory>[&hidden=1]]`: without `path` the
/// starting points, with it the sub-directories of that directory (read only).
async fn browse_fs(
    State(service): State<Service>,
    query: Result<Query<FsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|rejection| {
        ApiError::validation(format!("Invalid query string: {}", rejection.body_text()))
    })?;
    match query.path {
        None => {
            let roots: Vec<(String, std::path::PathBuf)> = match service.app_state() {
                Ok(state) => state
                    .root_monitor()
                    .roots()
                    .iter()
                    .map(|root| (root.name.clone(), root.server_path.clone()))
                    .collect(),
                Err(_) => Vec::new(),
            };
            let places = with_fs_timeout(
                "starting places",
                run_admin_fs_blocking(move || Ok(crate::browse::places(&roots))),
            )
            .await?;
            Ok(Json(places).into_response())
        }
        Some(path) => {
            let hidden = matches!(query.hidden.as_deref(), Some("1" | "true"));
            let shown = path.clone();
            let listing = with_fs_timeout(
                &shown,
                run_admin_fs_blocking(move || crate::browse::list_directory(&path, hidden)),
            )
            .await?;
            Ok(Json(listing).into_response())
        }
    }
}

/// `GET /admin/api/roots`: the `GET /api/v1/roots` entries (without needing a token).
async fn list_roots(State(service): State<Service>) -> Result<Json<Vec<RootResponse>>, ApiError> {
    let state = service.app_state()?;
    Ok(Json(
        run_blocking(move || Ok(state.roots_response()?)).await?,
    ))
}

/// `POST /admin/api/roots` `{ "name", "path" }` → 201 with the new root's entry.
async fn add_root(
    State(service): State<Service>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request: AddRootRequest = json_body(&headers, body)?;
    let state = service.app_state()?;
    let shown = request.path.clone();
    let root = with_fs_timeout(
        &shown,
        run_admin_fs_blocking(move || state.add_root(request)),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(root)).into_response())
}

/// `DELETE /admin/api/roots/{name}`.
async fn remove_root(
    State(service): State<Service>,
    Path(name): Path<String>,
) -> Result<Json<RemoveRootResponse>, ApiError> {
    let state = service.app_state()?;
    Ok(Json(run_blocking(move || state.remove_root(&name)).await?))
}

/// `POST /admin/api/scans`: same body and reply as `POST /api/v1/scans`.
async fn start_scan(
    State(service): State<Service>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<ScanResponse>, ApiError> {
    let request: ScanRequest = json_body(&headers, body)?;
    let state = service.app_state()?;
    let started = run_blocking(move || state.start_scan(request)).await?;
    match started.await {
        Ok(result) => result.map(Json),
        Err(_) => Err(ApiError::internal("The scan thread ended unexpectedly")),
    }
}

async fn current_scan(
    State(service): State<Service>,
) -> Result<Json<CurrentScanResponse>, ApiError> {
    let state = service.app_state()?;
    Ok(Json(CurrentScanResponse {
        scan: state.scans().current(),
    }))
}

fn session_id(raw: &str) -> Result<i64, ApiError> {
    raw.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| {
        ApiError::validation(format!(
            "Scan session id must be a positive integer, got {raw:?}"
        ))
    })
}

async fn scan_status(
    State(service): State<Service>,
    Path(raw): Path<String>,
) -> Result<Json<ScanResponse>, ApiError> {
    let id = session_id(&raw)?;
    let state = service.app_state()?;
    Ok(Json(run_blocking(move || state.scan_status(id)).await?))
}

async fn control(
    service: Service,
    raw: String,
    action: ScanAction,
) -> Result<Json<ScanResponse>, ApiError> {
    let id = session_id(&raw)?;
    let state = service.app_state()?;
    Ok(Json(
        run_blocking(move || state.control_scan(id, action)).await?,
    ))
}

async fn pause_scan(
    State(service): State<Service>,
    Path(raw): Path<String>,
) -> Result<Json<ScanResponse>, ApiError> {
    control(service, raw, ScanAction::Pause).await
}

async fn resume_scan(
    State(service): State<Service>,
    Path(raw): Path<String>,
) -> Result<Json<ScanResponse>, ApiError> {
    control(service, raw, ScanAction::Resume).await
}

async fn cancel_scan(
    State(service): State<Service>,
    Path(raw): Path<String>,
) -> Result<Json<ScanResponse>, ApiError> {
    control(service, raw, ScanAction::Cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hung_file_system_calls_cannot_pile_up_threads() {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let wait = std::sync::Arc::new(std::sync::Mutex::new(wait));
        let mut hung = Vec::new();
        for _ in 0..FS_SLOT_COUNT {
            let wait = wait.clone();
            hung.push(tokio::spawn(run_fs_blocking(move || {
                let _ = wait.lock().unwrap().recv();
                Ok(())
            })));
        }
        while FS_SLOTS.available_permits() > 0 {
            tokio::task::yield_now().await;
        }
        run_admin_fs_blocking(|| Ok(())).await.unwrap();
        let refused = run_fs_blocking(|| Ok(())).await.unwrap_err();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        for _ in 0..FS_SLOT_COUNT {
            release.send(()).unwrap();
        }
        for task in hung {
            task.await.unwrap().unwrap();
        }
        assert_eq!(FS_SLOTS.available_permits(), FS_SLOT_COUNT);
        run_fs_blocking(|| Ok(())).await.unwrap();
    }

    #[test]
    fn only_lan_host_names_reach_the_admin_page() {
        for host in [
            "127.0.0.1:8790",
            "192.0.2.10:8790",
            "192.0.2.10",
            "[::1]:8790",
            "localhost:8790",
            "myhost.local:8790",
            "myhost.local.",
            "myhost",
            "nas.lan",
            "router.home.arpa",
            "8.8.8.8",
        ] {
            assert!(is_lan_host(host), "{host}");
        }
        for host in [
            "audiolib.example.com:8443",
            "evil.example.com",
            "rebind.attacker.net:8790",
            "[nope]",
            "[::1]:bad",
            "[::1]garbage",
            "localhost:bad",
            "user@localhost",
            "localhost:0",
            "localhost..",
            "-bad.lan",
            "",
        ] {
            assert!(!is_lan_host(host), "{host}");
        }
    }
}
