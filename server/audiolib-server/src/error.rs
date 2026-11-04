// server/audiolib-server/src/error.rs
//! JSON error replies `{"error_code","message"}` using the codes of `doc/api/README.md` §2.1.
use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

/// `Retry-After` seconds sent with 503 `UNAVAILABLE`.
const UNAVAILABLE_RETRY_SECS: &str = "5";
/// `Retry-After` seconds sent with 503 `BUSY` (stream limit reached).
const STREAM_BUSY_RETRY_SECS: &str = "1";
/// `Retry-After` seconds sent with 503 `BUSY` while a snapshot is being prepared.
const SNAPSHOT_RETRY_SECS: &str = "5";

/// Wire shape of every error reply.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorBody {
    /// Stable machine-readable code, e.g. `VALIDATION` or `UNAUTHORIZED`.
    pub error_code: String,
    /// Human-readable description; server failures omit internal paths and details.
    pub message: String,
    /// Finer reason of some codes, e.g. `UNSUPPORTED_ENCODING` of `VALIDATION`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_code: Option<String>,
    /// The list waveform job in progress (409 `BUSY` of `POST /waveforms/jobs`, §9.5.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

/// An HTTP status plus stable error code; converts from [`audiolib_core::Error`].
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    retry_after: Option<&'static str>,
    sub_code: Option<&'static str>,
    job_id: Option<String>,
}

impl ApiError {
    /// Build an arbitrary error reply.
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
            sub_code: None,
            job_id: None,
        }
    }
    /// 401 `UNAUTHORIZED` for a missing or wrong bearer token.
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "A valid bearer token is required",
        )
    }
    /// 401 `UNAUTHORIZED` without any token where one is needed (same code as a wrong
    /// token; only the message differs).
    pub fn token_required(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", message)
    }
    /// 401 `UNAUTHORIZED` for a token that is neither the environment token nor a
    /// generated, unrevoked one.
    pub fn token_invalid() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "The bearer token is invalid or has been revoked",
        )
    }
    /// 403 `ADMIN_LAN_ONLY`: the admin page was requested through a proxy, from outside
    /// the LAN, or under a host name that is not a LAN name.
    pub fn admin_lan_only(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "ADMIN_LAN_ONLY", message)
    }
    /// 403 `ADMIN_HEADER_REQUIRED`: a changing admin request without
    /// `X-AudioLib-Admin: 1` (or from another origin).
    pub fn admin_header_required() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "ADMIN_HEADER_REQUIRED",
            "Admin changes need the X-AudioLib-Admin: 1 header from the admin page itself",
        )
    }
    /// 403 `MANAGE_FORBIDDEN`: library management (scans, record or folder removal) is
    /// not allowed for this request by `manage_mode` (`lan` and an external request, or
    /// `off`).
    pub fn manage_forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "MANAGE_FORBIDDEN", message)
    }
    /// 403 `FS_FORBIDDEN`: the operating system refused to list a directory for the
    /// admin disk browser (permissions, or a macOS privacy prompt still pending).
    pub fn fs_forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "FS_FORBIDDEN", message)
    }
    /// 504 `FS_TIMEOUT`: a directory of the admin disk browser did not answer in time
    /// (sleeping disk, unresponsive network share, or a pending macOS permission prompt).
    pub fn fs_timeout(message: impl Into<String>) -> Self {
        Self::new(StatusCode::GATEWAY_TIMEOUT, "FS_TIMEOUT", message)
    }
    /// 503 `BUSY` with `Retry-After: 1` when every admin disk-browser slot is still held
    /// by a file-system call that has not returned (hung network share): the request is
    /// refused instead of piling up one more blocked thread.
    pub fn fs_busy(message: impl Into<String>) -> Self {
        Self {
            retry_after: Some(STREAM_BUSY_RETRY_SECS),
            ..Self::new(StatusCode::SERVICE_UNAVAILABLE, "BUSY", message)
        }
    }
    /// 404 `NOT_FOUND`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "NOT_FOUND", message)
    }
    /// 400 `VALIDATION` for malformed request parameters such as a non-numeric id.
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "VALIDATION", message)
    }
    /// 400 `VALIDATION` with `sub_code` `UNSUPPORTED_ENCODING` (§9.4).
    pub fn unsupported_encoding(message: impl Into<String>) -> Self {
        Self {
            sub_code: Some("UNSUPPORTED_ENCODING"),
            ..Self::validation(message)
        }
    }
    /// 400 `VALIDATION` with `sub_code` `UNSUPPORTED_MODE`: unknown list waveform job
    /// mode (§9.5.3).
    pub fn unsupported_mode(message: impl Into<String>) -> Self {
        Self {
            sub_code: Some("UNSUPPORTED_MODE"),
            ..Self::validation(message)
        }
    }
    /// 400 `VALIDATION` with `sub_code` `CONFIRM_REQUIRED`: `rebuild` without
    /// `"confirm": "rebuild"` (§9.5.3).
    pub fn confirm_required(message: impl Into<String>) -> Self {
        Self {
            sub_code: Some("CONFIRM_REQUIRED"),
            ..Self::validation(message)
        }
    }
    /// 409 `BUSY` carrying the id of the list waveform job in progress (§9.5.3).
    pub fn job_busy(job_id: String) -> Self {
        Self {
            job_id: Some(job_id),
            ..Self::new(
                StatusCode::CONFLICT,
                "BUSY",
                "A list waveform job is already in progress",
            )
        }
    }
    /// 409 `JOB_STATE`: no list waveform job is running or paused (§9.5.3).
    pub fn job_state(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "JOB_STATE", message)
    }
    /// 416 `RANGE_NOT_SATISFIABLE`; the caller adds `Content-Range: bytes */<len>`.
    pub fn range_not_satisfiable(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "RANGE_NOT_SATISFIABLE",
            message,
        )
    }
    /// 410 `SNAPSHOT_EXPIRED`: the process-local immutable snapshot is no longer retained.
    pub fn snapshot_expired() -> Self {
        Self::new(
            StatusCode::GONE,
            "SNAPSHOT_EXPIRED",
            "The snapshot is no longer retained; start a new snapshot download",
        )
    }
    /// 501 `UNSUPPORTED` for operations deliberately unavailable in remote mode.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, "UNSUPPORTED", message)
    }
    /// 503 `UNAVAILABLE` while the library is opening or failed to open (adds `Retry-After`).
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            retry_after: Some(UNAVAILABLE_RETRY_SECS),
            ..Self::new(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE", message)
        }
    }
    /// 503 `ROOT_OFFLINE` with `Retry-After: 5`: the audio file's library root is offline
    /// or not answering (the library itself is available).
    pub fn root_offline(message: impl Into<String>) -> Self {
        Self {
            retry_after: Some(UNAVAILABLE_RETRY_SECS),
            ..Self::new(StatusCode::SERVICE_UNAVAILABLE, "ROOT_OFFLINE", message)
        }
    }
    /// 503 `BUSY` with `Retry-After: 1` when every audio stream slot is in use.
    pub fn stream_busy() -> Self {
        Self {
            retry_after: Some(STREAM_BUSY_RETRY_SECS),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "BUSY",
                "Too many audio streams are active; retry shortly",
            )
        }
    }
    /// 503 `BUSY` + `Retry-After: 5`: another `GET /waveforms/bundle` is in progress.
    pub fn bundle_busy() -> Self {
        Self {
            retry_after: Some("5"),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "BUSY",
                "Waveform bundle preparation or download capacity is busy; retry later",
            )
        }
    }
    /// 503 `BUSY` with `Retry-After: 1` when 4 `POST /waveforms` requests are already
    /// being processed; the same shape as [`ApiError::stream_busy`].
    pub fn list_waveforms_busy() -> Self {
        Self {
            retry_after: Some(STREAM_BUSY_RETRY_SECS),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "BUSY",
                "Too many list waveform requests are in progress; retry shortly",
            )
        }
    }
    /// 503 `BUSY` with `Retry-After: 5`: the snapshot is still being prepared (it keeps
    /// being prepared; retrying gets it).
    pub fn snapshot_preparing() -> Self {
        Self {
            retry_after: Some(SNAPSHOT_RETRY_SECS),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "BUSY",
                "The library snapshot is still being prepared; retry shortly",
            )
        }
    }

    /// 503 `BUSY` with `Retry-After: 1` when every `/audio/{id}/preview` slot
    /// (`max_previews`) is in use; the same status, code and header as
    /// [`ApiError::stream_busy`].
    pub fn preview_busy() -> Self {
        Self {
            retry_after: Some(STREAM_BUSY_RETRY_SECS),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "BUSY",
                "Too many previews are active; retry shortly",
            )
        }
    }
    /// 503 `BUSY` with `Retry-After: 1` when every cover decode slot stayed in use for
    /// [`crate::artwork::ARTWORK_SLOT_WAIT`].
    pub fn artwork_busy() -> Self {
        Self {
            retry_after: Some(STREAM_BUSY_RETRY_SECS),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "BUSY",
                "Artwork requests are busy; retry shortly",
            )
        }
    }
    /// 503 `BUSY` with `Retry-After: 1` when every ffmpeg transcode slot stayed in use
    /// (`codec=flac`); the same status, code and header as [`ApiError::stream_busy`].
    pub fn transcode_busy() -> Self {
        Self {
            retry_after: Some(STREAM_BUSY_RETRY_SECS),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "BUSY",
                "Too many previews are being transcoded; retry shortly",
            )
        }
    }
    /// 503 `TRANSCODER_UNAVAILABLE`: ffmpeg is missing or cannot be started
    /// (`codec=flac`). No `Retry-After`: it needs an administrator, not a retry.
    pub fn transcoder_unavailable(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "TRANSCODER_UNAVAILABLE",
            message,
        )
    }
    /// 415 `PREVIEW_UNSUPPORTED_FORMAT`: the file cannot be previewed with the requested
    /// codec (e.g. `codec=pcm` on ADPCM WAV, Ogg or MP3).
    pub fn preview_unsupported(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "PREVIEW_UNSUPPORTED_FORMAT",
            message,
        )
    }
    /// 408 `TIMEOUT` when a non-streaming request exceeds the request timeout.
    pub fn timeout() -> Self {
        Self::new(
            StatusCode::REQUEST_TIMEOUT,
            "TIMEOUT",
            "The request took too long; retry shortly",
        )
    }
    /// 500 `INTERNAL` for server faults outside `audiolib_core::Error` (e.g. a panicked task).
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL", message)
    }
    /// HTTP status of this reply.
    pub fn status(&self) -> StatusCode {
        self.status
    }
    /// Stable error code of this reply.
    pub fn code(&self) -> &'static str {
        self.code
    }
    /// Human-readable message of this reply.
    pub fn message(&self) -> &str {
        &self.message
    }
    /// Optional machine-readable refinement (`sub_code`) of this reply.
    pub fn sub_code(&self) -> Option<&'static str> {
        self.sub_code
    }
    /// Attach a machine-readable hint as `sub_code`.
    pub fn with_sub_code(self, sub_code: &'static str) -> Self {
        Self {
            sub_code: Some(sub_code),
            ..self
        }
    }
}

impl From<audiolib_core::Error> for ApiError {
    fn from(error: audiolib_core::Error) -> Self {
        use audiolib_core::Error;
        let message = error.to_string();
        match error {
            Error::Validation(_) => Self::new(StatusCode::BAD_REQUEST, "VALIDATION", message),
            Error::NotFound(_) => Self::new(StatusCode::NOT_FOUND, "NOT_FOUND", message),
            Error::Busy(_) => Self::new(StatusCode::CONFLICT, "BUSY", message),
            Error::Cancelled => Self::new(StatusCode::CONFLICT, "CANCELLED", message),
            Error::Database(_) | Error::NewerDatabase { .. } => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "DATABASE", message)
            }
            Error::Audio(_) => Self::new(StatusCode::INTERNAL_SERVER_ERROR, "AUDIO", message),
            Error::Io(io) if io.kind() == std::io::ErrorKind::StorageFull => Self::new(
                StatusCode::INSUFFICIENT_STORAGE,
                "INSUFFICIENT_STORAGE",
                message,
            ),
            Error::Io(_) => Self::new(StatusCode::INTERNAL_SERVER_ERROR, "IO", message),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status == StatusCode::SERVICE_UNAVAILABLE {
            // Expected while the library opens or streams are saturated; keep the log quiet.
            tracing::debug!(code = self.code, message = %self.message, "Temporarily unavailable");
        } else if self.status.is_server_error() {
            tracing::error!(code = self.code, message = %self.message, "Request failed");
        } else {
            tracing::debug!(code = self.code, message = %self.message, "Request rejected");
        }
        let status = self.status;
        // Log the original diagnostic above, but do not expose filesystem paths or
        // database/decoder diagnostics to remote callers, even with a valid token.
        let message = match status {
            StatusCode::INTERNAL_SERVER_ERROR => {
                "An internal server error occurred; consult the server logs".to_string()
            }
            StatusCode::INSUFFICIENT_STORAGE => {
                "The server has insufficient storage; contact the administrator".to_string()
            }
            _ => self.message,
        };
        let mut response = (
            self.status,
            Json(ErrorBody {
                error_code: self.code.to_string(),
                message,
                sub_code: self.sub_code.map(str::to_string),
                job_id: self.job_id,
            }),
        )
            .into_response();
        use axum::body::HttpBody as _;
        // State the JSON length explicitly instead of relying on the body's size hint:
        // a `HEAD` reply drops the body but must keep the same `Content-Length` as `GET`.
        if let Some(length) = response.body().size_hint().exact() {
            response
                .headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        }
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static(seconds));
        }
        if status == StatusCode::TOO_MANY_REQUESTS && self.code == "AUTH_RATE_LIMITED" {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
        }
        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"audiolib\""),
            );
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiolib_core::Error;

    #[tokio::test]
    async fn internal_diagnostics_never_reach_error_bodies() {
        use http_body_util::BodyExt;
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::INSUFFICIENT_STORAGE,
        ] {
            let response =
                ApiError::new(status, "IO", "secret /srv/private/library.db").into_response();
            assert_eq!(response.status(), status);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let body: ErrorBody = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body.error_code, "IO");
            assert!(!body.message.contains("secret"));
            assert!(!body.message.contains("/srv/private"));
        }
    }

    #[test]
    fn core_errors_map_to_documented_status_and_codes() {
        let cases = [
            (Error::Validation("v".into()), 400, "VALIDATION"),
            (Error::NotFound("n".into()), 404, "NOT_FOUND"),
            (Error::Busy("b".into()), 409, "BUSY"),
            (
                Error::Io(std::io::ErrorKind::StorageFull.into()),
                507,
                "INSUFFICIENT_STORAGE",
            ),
            (Error::Cancelled, 409, "CANCELLED"),
            (Error::Database("d".into()), 500, "DATABASE"),
            (Error::Audio("a".into()), 500, "AUDIO"),
            (Error::Io(std::io::Error::other("i")), 500, "IO"),
        ];
        for (error, status, code) in cases {
            let api = ApiError::from(error);
            assert_eq!(api.status().as_u16(), status);
            assert_eq!(api.code(), code);
        }
        assert_eq!(ApiError::unauthorized().status(), StatusCode::UNAUTHORIZED);
        for error in [ApiError::token_required("x"), ApiError::token_invalid()] {
            assert_eq!(
                (error.status().as_u16(), error.code()),
                (401, "UNAUTHORIZED")
            );
            assert!(
                error
                    .into_response()
                    .headers()
                    .contains_key(header::WWW_AUTHENTICATE)
            );
        }
        let lan_only = ApiError::admin_lan_only("x");
        assert_eq!(
            (lan_only.status().as_u16(), lan_only.code()),
            (403, "ADMIN_LAN_ONLY")
        );
        let csrf = ApiError::admin_header_required();
        assert_eq!(
            (csrf.status().as_u16(), csrf.code()),
            (403, "ADMIN_HEADER_REQUIRED")
        );
        let manage = ApiError::manage_forbidden("x");
        assert_eq!(
            (manage.status().as_u16(), manage.code()),
            (403, "MANAGE_FORBIDDEN")
        );
        assert_eq!(ApiError::unsupported("x").code(), "UNSUPPORTED");
        assert_eq!(ApiError::validation("x").status(), StatusCode::BAD_REQUEST);
        let unavailable = ApiError::unavailable("x");
        assert_eq!(unavailable.status().as_u16(), 503);
        assert_eq!(unavailable.code(), "UNAVAILABLE");
        let response = unavailable.into_response();
        assert_eq!(response.headers()[header::RETRY_AFTER], "5");
        let busy = ApiError::stream_busy();
        assert_eq!((busy.status().as_u16(), busy.code()), (503, "BUSY"));
        assert_eq!(busy.into_response().headers()[header::RETRY_AFTER], "1");
        let timeout = ApiError::timeout();
        assert_eq!(
            (timeout.status().as_u16(), timeout.code()),
            (408, "TIMEOUT")
        );
        let conflict = ApiError::from(Error::Busy("b".into())).into_response();
        assert!(!conflict.headers().contains_key(header::RETRY_AFTER));
        let busy = ApiError::preview_busy();
        assert_eq!((busy.status().as_u16(), busy.code()), (503, "BUSY"));
        assert_eq!(busy.into_response().headers()[header::RETRY_AFTER], "1");
        let busy = ApiError::transcode_busy();
        assert_eq!((busy.status().as_u16(), busy.code()), (503, "BUSY"));
        assert_eq!(busy.into_response().headers()[header::RETRY_AFTER], "1");
        let missing = ApiError::transcoder_unavailable("x");
        assert_eq!(
            (missing.status().as_u16(), missing.code()),
            (503, "TRANSCODER_UNAVAILABLE")
        );
        assert!(
            !missing
                .into_response()
                .headers()
                .contains_key(header::RETRY_AFTER)
        );
        let unsupported = ApiError::preview_unsupported("x");
        assert_eq!(
            (unsupported.status().as_u16(), unsupported.code()),
            (415, "PREVIEW_UNSUPPORTED_FORMAT")
        );
        let range = ApiError::range_not_satisfiable("x");
        assert_eq!(range.status().as_u16(), 416);
        assert_eq!(range.code(), "RANGE_NOT_SATISFIABLE");
    }
}
