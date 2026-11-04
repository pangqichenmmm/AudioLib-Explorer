// crates/audiolib-remote/src/error.rs
//! Remote failures classified for clear GUI/CLI messages; never contain secrets.
use serde::Deserialize;

pub type Result<T> = std::result::Result<T, RemoteError>;

/// Kind of a TLS failure, for a message the user can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsFailure {
    /// The chain does not lead to a root in the OS trust store (unknown issuer, missing
    /// intermediate, self-signed, bad signature, wrong key usage).
    Untrusted,
    /// The certificate is not valid for the host name that was connected to.
    HostnameMismatch,
    Expired,
    /// Not valid yet (often a wrong local clock).
    NotYetValid,
    Revoked,
    /// Any other handshake failure (no TLS on that port, protocol mismatch, alert, reset).
    Handshake,
}

impl TlsFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "certificate untrusted",
            Self::HostnameMismatch => "hostname mismatch",
            Self::Expired => "certificate expired",
            Self::NotYetValid => "certificate not yet valid",
            Self::Revoked => "certificate revoked",
            Self::Handshake => "handshake failed",
        }
    }
}

impl std::fmt::Display for TlsFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Which side must act on a [`RemoteError::PeerIncompatible`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PeerIncompatibility {
    /// The server predates compatibility markers or speaks an older protocol.
    ServerUpgradeRequired,
    /// The server reports this client is too old (or speaks an older protocol).
    ClientUpgradeRequired,
    /// One side's application version is on the other's known-bad list.
    Blocked,
    /// The peer's declared catalog contradicts its reported version.
    Inconsistent,
    /// The server refused this client's compatibility catalog as invalid
    /// (`409 PEER_CATALOG_INVALID`).
    ClientCatalogInvalid,
    /// `409 PROTOCOL_UNSUPPORTED` with a `sub_code` this client does not know: the
    /// protocols are incompatible but the side to act is unknown.
    ProtocolUnsupported,
}

impl PeerIncompatibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServerUpgradeRequired => "server upgrade required",
            Self::ClientUpgradeRequired => "client upgrade required",
            Self::Blocked => "blocked version",
            Self::Inconsistent => "inconsistent catalog",
            Self::ClientCatalogInvalid => "invalid client catalog",
            Self::ProtocolUnsupported => "protocol unsupported",
        }
    }
}

/// Network failure kinds recognised inside [`RemoteError::Unreachable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkFailure {
    /// The host name did not resolve.
    Dns,
    /// The host answered but nothing listens on the port.
    Refused,
}

/// Detail markers written by [`RemoteError::from_transport`] (and the connection test)
/// into `Unreachable` so [`RemoteError::network_failure`] can tell the cases apart.
pub(crate) const DNS_FAILED: &str = "DNS lookup failed";
pub(crate) const CONNECTION_REFUSED: &str = "connection refused";

/// A classified server-mode failure. Display text is English; use
/// [`RemoteError::localized`] for the desktop's Chinese/English presentation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RemoteError {
    #[error("Server unreachable: {0}")]
    Unreachable(String),
    #[error("Request timed out: {0}")]
    Timeout(String),
    #[error("Unauthorized: {0}")]
    Unauthorized(String),
    /// `401` to a request sent without a token (none configured): the server needs one.
    #[error("The server requires an access token: {0}")]
    TokenRequired(String),
    #[error("Not found: {0}")]
    NotFound(String),
    /// `409 BUSY` (snapshot slots, scans) or `503 BUSY` (the server's cap on concurrent
    /// audio streams): retry shortly; neither the token nor the server state is at fault.
    /// `retry_after` is the server's `Retry-After` in seconds, when sent.
    #[error("Server busy: {message}")]
    Busy {
        message: String,
        retry_after: Option<u64>,
    },
    /// `503 UNAVAILABLE` while the server is still opening (or retrying to open) its
    /// library: retry later; the token and configuration are not at fault.
    /// `retry_after` is the server's `Retry-After` in seconds, when sent.
    #[error("Server is starting: {message}")]
    Starting {
        message: String,
        retry_after: Option<u64>,
    },
    /// `503 UNAVAILABLE` on `/audio/{id}/stream` (§6.4): the record's library root is
    /// offline (NAS share unmounted or unreachable); the server and library are fine.
    /// `message` is the server's text, e.g. `Library root "SFX" is offline: …`.
    #[error("Library root offline: {message}")]
    RootOffline {
        message: String,
        retry_after: Option<u64>,
    },
    #[error("Validation: {0}")]
    Validation(String),
    /// `400 VALIDATION` from the list Waveform job endpoints (§9.5.3) with a `sub_code`
    /// the settings page explains: `CONFIRM_REQUIRED` or `UNSUPPORTED_MODE`.
    #[error("Validation ({sub_code}): {message}")]
    JobValidation { sub_code: String, message: String },
    /// `415 PREVIEW_UNSUPPORTED_FORMAT` (this preview codec cannot encode the file) or
    /// `503 TRANSCODER_UNAVAILABLE` (no ffmpeg for `codec=flac`) from `/audio/{id}/preview`
    /// (§8.1): use another codec or the original `/stream`; nothing else is wrong.
    #[error("Preview unavailable ({code}): {message}")]
    PreviewUnavailable { code: String, message: String },
    /// `403 MANAGE_FORBIDDEN`: the server's `manage_mode` (`lan` or `off`) refuses this
    /// structural write (scan, removal) for this request; the token is not at fault.
    #[error("Management forbidden by the server: {0}")]
    Forbidden(String),
    /// `403 HOST_FORBIDDEN`: a tokenless request used a `Host` the server does not accept
    /// (not an IP, LAN name or its configured `public_url` / `lan_url` host).
    #[error("Host name refused by the server: {0}")]
    HostForbidden(String),
    /// `403 ADMIN_LAN_ONLY`: the admin page/API is only served to direct LAN peers using a
    /// LAN host name (no reverse proxy, no public domain).
    #[error("Admin page is LAN-only: {0}")]
    AdminLanOnly(String),
    /// `429 AUTH_RATE_LIMITED`: too many failed authentications from this address; wait
    /// `retry_after` seconds (the server sends 60) before trying again.
    #[error("Too many failed authentications: {message}")]
    RateLimited {
        message: String,
        retry_after: Option<u64>,
    },
    /// `409 SCAN_STATE` (API §6.6): the scan session's state does not allow this control
    /// (finished, cancelling, finalizing or not running in this server process). Unlike
    /// [`RemoteError::Busy`] retrying does not help; refresh the scan status instead.
    #[error("Scan state does not allow this: {0}")]
    ScanState(String),
    /// `409 JOB_STATE` (API §9.5.3): there is no list Waveform job or it has finished;
    /// refresh the status instead of retrying.
    #[error("Job state does not allow this: {0}")]
    JobState(String),
    /// The cached record/session IDs belong to another library (`409 LIBRARY_CHANGED`
    /// or `LIBRARY_ID_REQUIRED`); reject the write and sync before allowing new edits.
    #[error("Server library changed: {0}")]
    LibraryChanged(String),
    /// `413 PAYLOAD_TOO_LARGE`: the request body exceeds the server's 8 MiB limit.
    #[error("Request too large: {0}")]
    PayloadTooLarge(String),
    /// `507 INSUFFICIENT_STORAGE`: the server's disk is full (it cannot write the snapshot,
    /// scan data or cache it needs). Retrying does not help until space is freed, so no
    /// retry loop treats it as transient.
    #[error("Server disk full (507 INSUFFICIENT_STORAGE): {0}")]
    InsufficientStorage(String),
    #[error("Unsupported by server: {0}")]
    Unsupported(String),
    #[error("Server error {status} {code}: {message}")]
    Server {
        status: u16,
        code: String,
        message: String,
    },
    #[error("Unexpected server response: {0}")]
    Protocol(String),
    /// The file changed on the server while it was streamed (ETag, `If-Range` or length).
    #[error("File changed on the server: {0}")]
    Changed(String),
    #[error("Snapshot rejected: {0}")]
    Snapshot(String),
    /// Compatibility markers refuse this client/server pair (Compatibility_Markers.md §4):
    /// either side detected it (local check or `409 PEER_UPGRADE_REQUIRED`,
    /// `PROTOCOL_UNSUPPORTED`, `PEER_VERSION_BLOCKED`, `PEER_CATALOG_INVALID`, legacy
    /// `PEER_VERSION_UNSUPPORTED`).
    /// The existing cache is always retained.
    #[error("Incompatible client/server ({}): {detail}", reason.as_str())]
    PeerIncompatible {
        reason: PeerIncompatibility,
        detail: String,
    },
    /// The snapshot or cache uses a newer database format than this client supports.
    #[error("{}", audiolib_core::newer_database_message(what, *found, *supported, false))]
    NewerDatabase {
        what: String,
        found: i64,
        supported: i64,
    },
    #[error("Operation cancelled")]
    Cancelled,
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    /// TLS certificate or handshake failure on an `https://` URL (server or WebDAV).
    /// Self-signed or otherwise untrusted certificates are never accepted.
    #[error("TLS {kind}: {detail}")]
    Tls { kind: TlsFailure, detail: String },
}

/// How long to wait before a retry: the server's `Retry-After` (seconds) when it sent a
/// positive one, otherwise `backoff`; never longer than `cap`.
pub(crate) fn retry_wait(
    backoff: std::time::Duration,
    retry_after: Option<u64>,
    cap: std::time::Duration,
) -> std::time::Duration {
    match retry_after {
        Some(seconds) if seconds > 0 => std::time::Duration::from_secs(seconds),
        _ => backoff,
    }
    .min(cap)
}

/// The agreed `{ "error_code", "message" }` error body.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ErrorBody {
    #[serde(default)]
    pub error_code: String,
    #[serde(default)]
    pub message: String,
    /// Refines `VALIDATION` (§9.4, §9.5.3); empty when absent.
    #[serde(default)]
    pub sub_code: String,
}

impl RemoteError {
    /// Map an HTTP status plus the optional JSON error body to a classified error.
    pub(crate) fn from_status(status: u16, body: &str, context: &str) -> Self {
        Self::from_response(status, body, context, None)
    }

    /// Like [`RemoteError::from_status`], keeping a `Retry-After` value (seconds).
    pub(crate) fn from_response(
        status: u16,
        body: &str,
        context: &str,
        retry_after: Option<u64>,
    ) -> Self {
        let parsed: ErrorBody = serde_json::from_str(body).unwrap_or_default();
        let message = if parsed.message.is_empty() {
            let snippet: String = body.chars().take(200).collect();
            if snippet.trim().is_empty() {
                format!("{context}: HTTP {status}")
            } else {
                format!("{context}: HTTP {status} {}", snippet.trim())
            }
        } else {
            format!("{context}: {}", parsed.message)
        };
        match (status, parsed.error_code.as_str()) {
            // §2.3: failed-auth limiter; keep `Retry-After` so nothing retries early.
            // The server currently only emits AUTH_RATE_LIMITED as 429, so every 429 maps
            // to RateLimited; revisit if other 429 codes appear.
            (429, _) | (_, "AUTH_RATE_LIMITED") => Self::RateLimited {
                message,
                retry_after,
            },
            // §2.2: a missing and a wrong token are both `401 UNAUTHORIZED` (only the
            // message differs, which is not matched); requests sent without a token turn it
            // into `TokenRequired` through `without_token`.
            (401, _) | (_, "UNAUTHORIZED") => Self::Unauthorized(message),
            (404, _) | (_, "NOT_FOUND") => Self::NotFound(message),
            // Before the 503 arm: the preview cannot be produced, use another route.
            (415, _) | (_, "PREVIEW_UNSUPPORTED_FORMAT") | (_, "TRANSCODER_UNAVAILABLE") => {
                Self::PreviewUnavailable {
                    code: if parsed.error_code.is_empty() {
                        "HTTP 415".into()
                    } else {
                        parsed.error_code
                    },
                    message,
                }
            }
            (_, "MANAGE_FORBIDDEN") => Self::Forbidden(message),
            (_, "HOST_FORBIDDEN") => Self::HostForbidden(message),
            (_, "ADMIN_LAN_ONLY") => Self::AdminLanOnly(message),
            // Before the generic 409 arm; other 409 codes stay `Busy`.
            (_, "SCAN_STATE") => Self::ScanState(message),
            (_, "JOB_STATE") => Self::JobState(message),
            (_, "LIBRARY_CHANGED" | "LIBRARY_ID_REQUIRED") => Self::LibraryChanged(message),
            // The server rejected this client (no markers or an older protocol).
            (_, "PEER_UPGRADE_REQUIRED") => Self::PeerIncompatible {
                reason: PeerIncompatibility::ClientUpgradeRequired,
                detail: message,
            },
            // `sub_code` `SERVER_UPGRADE_REQUIRED` (or none, as sent before sub codes): the
            // server's protocol is entirely below this client's, so the server, never the
            // client, must upgrade. An unknown `sub_code` gets a generic message.
            (_, "PROTOCOL_UNSUPPORTED") => Self::PeerIncompatible {
                reason: match parsed.sub_code.as_str() {
                    "SERVER_UPGRADE_REQUIRED" | "" => PeerIncompatibility::ServerUpgradeRequired,
                    _ => PeerIncompatibility::ProtocolUnsupported,
                },
                detail: message,
            },
            (_, "PEER_CATALOG_INVALID") => Self::PeerIncompatible {
                reason: PeerIncompatibility::ClientCatalogInvalid,
                detail: message,
            },
            (_, "PEER_VERSION_BLOCKED") => Self::PeerIncompatible {
                reason: PeerIncompatibility::Blocked,
                detail: message,
            },
            // Only 0.6.x–0.7.1 servers send this: their version ranges predate markers.
            (_, "PEER_VERSION_UNSUPPORTED") => Self::PeerIncompatible {
                reason: PeerIncompatibility::ServerUpgradeRequired,
                detail: message,
            },
            (_, "SNAPSHOT_FORMAT_UNSUPPORTED") => Self::Snapshot(message),
            // Before the 503 arm: a `503 BUSY` is a full stream cap, not a starting server.
            (409, _) | (_, "BUSY") | (_, "CANCELLED") => Self::Busy {
                message,
                retry_after,
            },
            // The server gave up on a slow non-streaming request: transient, like a client
            // timeout (keep the cache, retry later), not a token or protocol problem.
            (408, _) | (_, "TIMEOUT") => Self::Timeout(message),
            // §6.4: the record's library root is offline or not answering (503 +
            // `Retry-After: 5`); the server and library are fine.
            (_, "ROOT_OFFLINE") => Self::RootOffline {
                message: if parsed.message.trim().is_empty() {
                    message
                } else {
                    parsed.message.trim().to_owned()
                },
                retry_after,
            },
            (503, _) | (_, "STARTING") | (_, "UNAVAILABLE") | (_, "SERVICE_UNAVAILABLE") => {
                Self::Starting {
                    message,
                    retry_after,
                }
            }
            (413, _) | (_, "PAYLOAD_TOO_LARGE") => Self::PayloadTooLarge(message),
            (507, _) | (_, "INSUFFICIENT_STORAGE") => Self::InsufficientStorage(message),
            (400, _) | (_, "VALIDATION")
                if matches!(
                    parsed.sub_code.as_str(),
                    "CONFIRM_REQUIRED" | "UNSUPPORTED_MODE"
                ) =>
            {
                Self::JobValidation {
                    sub_code: parsed.sub_code,
                    message,
                }
            }
            (400, _) | (_, "VALIDATION") => Self::Validation(message),
            (501, _) | (_, "UNSUPPORTED") => Self::Unsupported(message),
            _ => Self::Server {
                status,
                code: if parsed.error_code.is_empty() {
                    "HTTP".into()
                } else {
                    parsed.error_code
                },
                message,
            },
        }
    }

    /// Like [`RemoteError::from_response`] for the audio endpoints. `ROOT_OFFLINE` is a root
    /// offline everywhere; servers before that code sent `503 UNAVAILABLE` with the message
    /// `Library root "<name>" is offline: …`, which is still recognised. Any other `503`
    /// (the library still opening or failing to open, or an empty body) is a starting
    /// server.
    pub(crate) fn from_audio_response(
        status: u16,
        body: &str,
        context: &str,
        retry_after: Option<u64>,
    ) -> Self {
        match Self::from_response(status, body, context, retry_after) {
            Self::Starting {
                message,
                retry_after,
            } => {
                let parsed: ErrorBody = serde_json::from_str(body).unwrap_or_default();
                let reason = parsed.message.trim();
                if reason
                    .get(..13)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("library root "))
                {
                    Self::RootOffline {
                        message: reason.to_owned(),
                        retry_after,
                    }
                } else {
                    Self::Starting {
                        message,
                        retry_after,
                    }
                }
            }
            other => other,
        }
    }

    /// Classify a transport failure from ureq without exposing request headers.
    pub(crate) fn from_transport(error: ureq::Error, url: &str) -> Self {
        match error {
            // A timeout before the connection exists (resolving, connecting) means the
            // server cannot be reached; after that it means the server answers slowly.
            ureq::Error::Timeout(ureq::Timeout::Resolve | ureq::Timeout::Connect) => {
                Self::Unreachable(format!("{url}: connect timed out"))
            }
            ureq::Error::Timeout(_) => Self::Timeout(url.to_owned()),
            ureq::Error::HostNotFound => {
                Self::Unreachable(format!("{url}: host not found ({DNS_FAILED})"))
            }
            ureq::Error::ConnectionFailed => Self::Unreachable(format!("{url}: connection failed")),
            ureq::Error::Rustls(error) => crate::tls::tls_error(&error, url),
            ureq::Error::Tls(message) => Self::Tls {
                kind: TlsFailure::Handshake,
                detail: format!("{url}: {message}"),
            },
            ureq::Error::Io(error) => Self::from_io(&error, url),
            ureq::Error::BadUri(message) => Self::Validation(format!("{url}: {message}")),
            other => Self::Protocol(format!("{url}: {other}")),
        }
    }

    /// Classify a socket-level failure: TLS (rustls inside the I/O error), DNS, refused
    /// connection, timeout, anything else unreachable.
    pub(crate) fn from_io(error: &std::io::Error, url: &str) -> Self {
        if let Some(tls) = crate::tls::rustls_error_in(error) {
            return crate::tls::tls_error(tls, url);
        }
        match error.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                Self::Timeout(url.to_owned())
            }
            std::io::ErrorKind::ConnectionRefused => {
                Self::Unreachable(format!("{url}: {CONNECTION_REFUSED}"))
            }
            _ if crate::tls::is_dns_failure(error) => {
                Self::Unreachable(format!("{url}: {DNS_FAILED}: {error}"))
            }
            _ => Self::Unreachable(format!("{url}: {error}")),
        }
    }

    /// DNS failure or refused connection behind an [`RemoteError::Unreachable`].
    pub fn network_failure(&self) -> Option<NetworkFailure> {
        match self {
            Self::Unreachable(detail) if detail.contains(DNS_FAILED) => Some(NetworkFailure::Dns),
            Self::Unreachable(detail) if detail.contains(CONNECTION_REFUSED) => {
                Some(NetworkFailure::Refused)
            }
            _ => None,
        }
    }

    /// Human-readable text for the desktop language.
    pub fn localized(&self, chinese: bool) -> String {
        if !chinese {
            return match self {
                Self::Unreachable(detail) => {
                    (match self.network_failure() {
                        Some(NetworkFailure::Dns) => format!(
                            "Cannot resolve the server name (DNS lookup failed). Check the spelling of the domain, your network connection and DNS settings. ({detail})"
                        ),
                        Some(NetworkFailure::Refused) => format!(
                            "The server refused the connection: nothing is listening on that port. Check the port number and that the server or its reverse proxy is running. ({detail})"
                        ),
                        None => format!(
                            "Cannot reach the server. Check the URL, network and that the server is running. ({detail})"
                        ),
                    }) + mdns_hint(detail, false)
                }
                Self::Timeout(detail) => format!("The server did not respond in time. ({detail})"),
                Self::Unauthorized(detail) => format!(
                    "The server rejected the access token (401): it is wrong or has been revoked. {}{}",
                    token_hint(false),
                    mdns_hint(detail, false)
                ),
                Self::TokenRequired(detail) => format!(
                    "The server requires an access token (401). {}{}",
                    token_hint(false),
                    mdns_hint(detail, false)
                ),
                Self::HostForbidden(detail) => format!(
                    "The server refused this host name (403 HOST_FORBIDDEN): requests without a token must use an IP address, a LAN name or the server's configured public_url / lan_url host. Check the access token, or use the server's LAN IP / configured host name instead. ({detail})"
                ),
                Self::AdminLanOnly(detail) => format!(
                    "The server's admin page is only available directly on the local network (403 ADMIN_LAN_ONLY). Open /admin from another device on the LAN using the server's LAN IP, not through a reverse proxy or public domain. ({detail})"
                ),
                Self::RateLimited {
                    message,
                    retry_after,
                } => format!(
                    "Too many failed authentications; the server is temporarily refusing requests from this client's IP address as the server sees it (behind a proxy or NAT, that is the proxy's/router's IP) (429 AUTH_RATE_LIMITED). Until the wait ends even a correct token is rejected. Try again in {} seconds and check the access token. ({message})",
                    retry_after.unwrap_or(60)
                ),
                Self::Tls { kind, detail } => tls_message(*kind, detail, false),
                Self::Starting { message, .. } => format!(
                    "The server is still starting (opening its library); it will be retried shortly. ({message})"
                ),
                Self::RootOffline { message, .. } => format!(
                    "The library root is offline (NAS share not mounted or unreachable); cached sounds still play. ({message})"
                ),
                Self::PayloadTooLarge(detail) => format!(
                    "The request is too large (413, server limit 8 MiB); edit fewer records at once or shorten tags/notes. ({detail})"
                ),
                Self::InsufficientStorage(detail) => format!(
                    "The server's disk is full (507 INSUFFICIENT_STORAGE); free some space on the server and try again. Not retried automatically. ({detail})"
                ),
                Self::Forbidden(detail) => format!(
                    "The server does not allow this management operation from here (403): {}. ({detail})",
                    forbidden_reason(detail, false)
                ),
                Self::ScanState(detail) => format!(
                    "The scan's current state does not allow this (409); refresh the scan progress. ({detail})"
                ),
                Self::JobState(detail) => format!(
                    "There is no list Waveform job in progress (409); refresh the status. ({detail})"
                ),
                Self::LibraryChanged(detail) => format!(
                    "The server library changed or its identity is unconfirmed (409); nothing was written. Sync before editing again. ({detail})"
                ),
                Self::PeerIncompatible { reason, detail } => peer_message(*reason, detail, false),
                other => other.to_string(),
            };
        }
        match self {
            Self::Unreachable(detail) => {
                (match self.network_failure() {
                    Some(NetworkFailure::Dns) => format!(
                        "无法解析服务器域名（DNS 失败），请检查域名拼写、网络连接和 DNS 设置。（{detail}）"
                    ),
                    Some(NetworkFailure::Refused) => format!(
                        "服务器拒绝连接：该端口没有服务在监听。请检查端口号，以及服务器或反向代理是否已启动。（{detail}）"
                    ),
                    None => {
                        format!(
                            "无法连接服务器，请检查地址、网络以及服务器是否已启动。（{detail}）"
                        )
                    }
                }) + mdns_hint(detail, true)
            }
            Self::Timeout(detail) => format!("服务器响应超时。（{detail}）"),
            Self::Unauthorized(detail) => format!(
                "服务器拒绝了访问令牌（401）：令牌错误或已被撤销。{}{}",
                token_hint(true),
                mdns_hint(detail, true)
            ),
            Self::TokenRequired(detail) => format!(
                "服务器要求访问令牌（401）。{}{}",
                token_hint(true),
                mdns_hint(detail, true)
            ),
            Self::HostForbidden(detail) => format!(
                "服务器拒绝了此主机名（403 HOST_FORBIDDEN）：未带令牌的请求只能用 IP 地址、局域网名称或服务器配置的 public_url / lan_url 主机名访问。请检查 token，或改用服务器局域网 IP / 已配置的主机名。（{detail}）"
            ),
            Self::AdminLanOnly(detail) => format!(
                "服务器管理页面仅限局域网内直接访问（403 ADMIN_LAN_ONLY）：请在局域网内的另一台设备上用服务器的局域网 IP 打开 /admin，不要经反向代理或公网域名访问。（{detail}）"
            ),
            Self::RateLimited {
                message,
                retry_after,
            } => format!(
                "认证失败次数过多，服务器暂时拒绝来自本客户端 IP 地址（以服务器所见为准；经代理或 NAT 时为代理/路由器的 IP）的请求（429 AUTH_RATE_LIMITED），等待结束前即使令牌正确也会被拒绝；请 {} 秒后再试，并检查访问令牌是否正确。（{message}）",
                retry_after.unwrap_or(60)
            ),
            Self::Tls { kind, detail } => tls_message(*kind, detail, true),
            Self::NotFound(detail) => format!("服务器上找不到请求的资源：{detail}"),
            Self::Busy { message, .. } => format!("服务器正忙，请稍后重试：{message}"),
            Self::Starting { message, .. } => {
                format!("服务器正在启动（正在打开素材库），稍后会自动重试：{message}")
            }
            Self::RootOffline { message, .. } => format!(
                "音效库根目录离线（NAS 共享未挂载或无法访问），已缓存的音效仍可试听：{message}"
            ),
            Self::Validation(detail) => format!("参数无效：{detail}"),
            Self::JobValidation { message, .. } => format!("参数无效：{message}"),
            Self::PreviewUnavailable { message, .. } => {
                format!("服务器无法生成此文件的试听预览，将改用原始文件：{message}")
            }
            Self::Forbidden(detail) => format!(
                "服务器不允许从这里执行此管理操作（403）：{}。（{detail}）",
                forbidden_reason(detail, true)
            ),
            Self::ScanState(detail) => {
                format!("扫描当前的状态不允许此操作（409），请刷新扫描进度：{detail}")
            }
            Self::JobState(detail) => {
                format!("当前没有进行中的列表波形任务（409），请刷新状态：{detail}")
            }
            Self::LibraryChanged(detail) => {
                format!(
                    "服务器素材库已更换或身份未确认（409），操作未执行；请同步后再编辑：{detail}"
                )
            }
            Self::PayloadTooLarge(detail) => format!(
                "请求过大（413，服务器上限 8 MiB），请减少一次编辑的记录数或缩短标签/备注：{detail}"
            ),
            Self::InsufficientStorage(detail) => format!(
                "服务器磁盘不足（507），请先释放服务器磁盘空间后再试，不会自动重试。（{detail}）"
            ),
            Self::Unsupported(detail) => format!("服务器不支持此操作：{detail}"),
            Self::Server {
                status,
                code,
                message,
            } => format!("服务器错误 {status} {code}：{message}"),
            Self::Protocol(detail) => format!("服务器响应格式异常：{detail}"),
            Self::Snapshot(detail) => format!("快照校验失败，未替换本地缓存：{detail}"),
            Self::PeerIncompatible { reason, detail } => peer_message(*reason, detail, true),
            Self::NewerDatabase {
                what,
                found,
                supported,
            } => audiolib_core::newer_database_message(what, *found, *supported, true),
            Self::Changed(detail) => format!("服务器上的文件已变化，请重新试听或同步：{detail}"),
            Self::Cancelled => "操作已取消".into(),
            Self::Io(error) => format!("文件读写失败：{error}"),
        }
    }
}

/// Why `MANAGE_FORBIDDEN` was returned, from the server's message: `manage_mode = lan`
/// (external requests refused) or `off` (remote management disabled).
pub(crate) fn forbidden_reason(message: &str, chinese: bool) -> &'static str {
    // §6.9 messages end in `(manage_mode = lan)` / `(manage_mode = off)`.
    let lower = message.to_ascii_lowercase().replace(' ', "");
    let disabled =
        lower.contains("manage_mode=off") || lower.contains("disabled") || message.contains("关闭");
    let lan_only = lower.contains("manage_mode=lan")
        || lower.contains("lan")
        || lower.contains("localnetwork")
        || message.contains("局域网");
    match (lan_only && !disabled, disabled, chinese) {
        (true, _, true) => "仅限局域网执行",
        (true, _, false) => "only allowed from the server's local network",
        (false, true, true) => "服务器已关闭远程管理",
        (false, true, false) => "remote management is disabled on the server",
        (false, false, true) => "服务器的管理权限设置拒绝了此请求",
        (false, false, false) => "the server's management setting refused the request",
    }
}

/// True when `text` contains a URL whose host is an mDNS `.local` name.
pub(crate) fn has_mdns_host(text: &str) -> bool {
    text.match_indices("://").any(|(index, _)| {
        let rest = &text[index + 3..];
        let authority = rest
            .split(['/', '?', '#', ' ', ')', '）'])
            .next()
            .unwrap_or("");
        let host = authority.rsplit('@').next().unwrap_or("");
        let host = match host.rsplit_once(':') {
            Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
            _ => host,
        };
        host.trim_end_matches('.')
            .to_ascii_lowercase()
            .ends_with(".local")
    })
}

/// Extra advice for a failed connection to a `.local` host name: proxies in fake-IP mode
/// (and some Windows setups) do not resolve mDNS names, so use the server's LAN IP.
fn mdns_hint(detail: &str, chinese: bool) -> &'static str {
    match (has_mdns_host(detail), chinese) {
        (false, _) => "",
        (true, true) => {
            " 提示：.local 主机名可能被代理（fake-ip 模式）或系统 DNS 接管而无法直连服务器，请改用服务器的局域网 IP 地址。"
        }
        (true, false) => {
            " Hint: .local host names can be captured by a proxy (fake-IP mode) or fail to resolve; use the server's LAN IP address instead."
        }
    }
}

/// Where to get a token: the server's web admin page, reachable on the LAN only. The
/// client cannot know the server's LAN address, so the wording stays generic.
fn token_hint(chinese: bool) -> &'static str {
    if chinese {
        "请在局域网内打开服务器的网页管理页面（http://<服务器局域网地址>:8790/admin）生成令牌，然后填入“数据源设置 → 访问令牌”。"
    } else {
        "Generate a token on the server's web admin page (open http://<LAN server address>:8790/admin from the local network), then enter it under Data source settings → Access token."
    }
}

fn tls_message(kind: TlsFailure, detail: &str, chinese: bool) -> String {
    match (kind, chinese) {
        (TlsFailure::Untrusted, true) => format!(
            "服务器证书不受信任（颁发机构未知、证书链不完整或为自签名证书）。服务器需要使用公共受信任的证书（例如 Let's Encrypt）；本版本不接受自签名证书。（{detail}）"
        ),
        (TlsFailure::Untrusted, false) => format!(
            "The server certificate is not trusted (unknown issuer, incomplete chain or self-signed). The server needs a publicly trusted certificate (e.g. Let's Encrypt); self-signed certificates are not accepted in this version. ({detail})"
        ),
        (TlsFailure::HostnameMismatch, true) => format!(
            "证书与服务器地址不匹配：证书不是为该主机名签发的。请使用证书中的域名连接（HTTPS 通常不能直接用 IP 地址访问）。（{detail}）"
        ),
        (TlsFailure::HostnameMismatch, false) => format!(
            "The certificate does not match the server address: it was not issued for this host name. Connect with the domain name on the certificate (HTTPS usually does not work with a bare IP address). ({detail})"
        ),
        (TlsFailure::Expired, true) => format!(
            "服务器证书已过期，请联系服务器管理员续期证书，并确认本机日期和时间正确。（{detail}）"
        ),
        (TlsFailure::Expired, false) => format!(
            "The server certificate has expired. Ask the server administrator to renew it, and check this PC's date and time. ({detail})"
        ),
        (TlsFailure::NotYetValid, true) => {
            format!("服务器证书尚未生效，请检查本机日期和时间是否正确。（{detail}）")
        }
        (TlsFailure::NotYetValid, false) => format!(
            "The server certificate is not valid yet. Check this PC's date and time. ({detail})"
        ),
        (TlsFailure::Revoked, true) => {
            format!("服务器证书已被吊销，请联系服务器管理员更换证书。（{detail}）")
        }
        (TlsFailure::Revoked, false) => format!(
            "The server certificate has been revoked. Ask the server administrator to replace it. ({detail})"
        ),
        (TlsFailure::Handshake, true) => format!(
            "TLS 握手失败：请确认该端口提供 HTTPS（若服务器只提供 HTTP，请改用 http://），以及反向代理配置正确。（{detail}）"
        ),
        (TlsFailure::Handshake, false) => format!(
            "The TLS handshake failed. Check that this port serves HTTPS (use http:// if the server only speaks HTTP) and that the reverse proxy is configured correctly. ({detail})"
        ),
    }
}

impl From<RemoteError> for audiolib_core::Error {
    fn from(error: RemoteError) -> Self {
        match error {
            RemoteError::NotFound(message) => Self::NotFound(message),
            RemoteError::Busy { message, .. }
            | RemoteError::ScanState(message)
            | RemoteError::JobState(message)
            | RemoteError::Starting { message, .. }
            | RemoteError::RootOffline { message, .. } => Self::Busy(message),
            RemoteError::Validation(message)
            | RemoteError::JobValidation { message, .. }
            | RemoteError::PayloadTooLarge(message) => Self::Validation(message),
            RemoteError::Cancelled => Self::Cancelled,
            RemoteError::Io(error) => Self::Io(error),
            RemoteError::NewerDatabase {
                what,
                found,
                supported,
            } => Self::NewerDatabase {
                what,
                found,
                supported,
            },
            other => Self::Database(other.to_string()),
        }
    }
}

impl RemoteError {
    /// The server refused the request for lack of a valid token (wrong token, or none
    /// configured while the server requires one).
    /// `429 AUTH_RATE_LIMITED` counts too: it follows repeated auth failures, and
    /// callers then back off (the poller waits at least `UNAUTHORIZED_RETRY`, longer than
    /// the server's `Retry-After: 60`) instead of retrying. The server currently only
    /// emits AUTH_RATE_LIMITED as 429; revisit if other 429 codes appear.
    pub fn is_unauthorized(&self) -> bool {
        matches!(
            self,
            Self::Unauthorized(_) | Self::TokenRequired(_) | Self::RateLimited { .. }
        )
    }

    /// For a request sent without a token: a `401` means the server requires one.
    pub(crate) fn without_token(self) -> Self {
        match self {
            Self::Unauthorized(message) => Self::TokenRequired(message),
            other => other,
        }
    }

    /// True for failures that mean "cannot reach the server right now" (keep using the
    /// cache, retry with backoff) rather than a configuration problem.
    pub fn is_offline(&self) -> bool {
        matches!(self, Self::Unreachable(_) | Self::Timeout(_))
    }

    /// The server could not be reached at all (connection refused, DNS, connect timeout):
    /// the only failure shown as "offline".
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Unreachable(_))
    }

    /// A connection exists but the answer did not arrive in time (response or body
    /// timeout, or the server's own `408 TIMEOUT`): shown as "responding slowly" and
    /// retried quickly, not as offline.
    pub fn is_slow(&self) -> bool {
        matches!(self, Self::Timeout(_))
    }

    /// For a failed write: the server may have committed it anyway (client timeout, the
    /// server's `408 TIMEOUT`, a connection dropped mid-request or an unreadable reply),
    /// so the local snapshot must be re-synced rather than trusted. Classified server
    /// errors (`400`/`404`/`409`/`413`/`5xx`) never change the server library.
    pub fn write_may_have_committed(&self) -> bool {
        matches!(
            self,
            Self::Timeout(_) | Self::Unreachable(_) | Self::Protocol(_)
        )
    }

    /// A copy of this error for sharing one failure with several waiters (`Io` keeps its
    /// kind and text).
    pub(crate) fn duplicate(&self) -> Self {
        match self {
            Self::Unreachable(detail) => Self::Unreachable(detail.clone()),
            Self::Timeout(detail) => Self::Timeout(detail.clone()),
            Self::Unauthorized(detail) => Self::Unauthorized(detail.clone()),
            Self::TokenRequired(detail) => Self::TokenRequired(detail.clone()),
            Self::NotFound(detail) => Self::NotFound(detail.clone()),
            Self::Busy {
                message,
                retry_after,
            } => Self::Busy {
                message: message.clone(),
                retry_after: *retry_after,
            },
            Self::RootOffline {
                message,
                retry_after,
            } => Self::RootOffline {
                message: message.clone(),
                retry_after: *retry_after,
            },
            Self::Starting {
                message,
                retry_after,
            } => Self::Starting {
                message: message.clone(),
                retry_after: *retry_after,
            },
            Self::Validation(detail) => Self::Validation(detail.clone()),
            Self::JobValidation { sub_code, message } => Self::JobValidation {
                sub_code: sub_code.clone(),
                message: message.clone(),
            },
            Self::PreviewUnavailable { code, message } => Self::PreviewUnavailable {
                code: code.clone(),
                message: message.clone(),
            },
            Self::Forbidden(detail) => Self::Forbidden(detail.clone()),
            Self::HostForbidden(detail) => Self::HostForbidden(detail.clone()),
            Self::AdminLanOnly(detail) => Self::AdminLanOnly(detail.clone()),
            Self::RateLimited {
                message,
                retry_after,
            } => Self::RateLimited {
                message: message.clone(),
                retry_after: *retry_after,
            },
            Self::ScanState(detail) => Self::ScanState(detail.clone()),
            Self::JobState(detail) => Self::JobState(detail.clone()),
            Self::LibraryChanged(detail) => Self::LibraryChanged(detail.clone()),
            Self::PayloadTooLarge(detail) => Self::PayloadTooLarge(detail.clone()),
            Self::InsufficientStorage(detail) => Self::InsufficientStorage(detail.clone()),
            Self::Unsupported(detail) => Self::Unsupported(detail.clone()),
            Self::Server {
                status,
                code,
                message,
            } => Self::Server {
                status: *status,
                code: code.clone(),
                message: message.clone(),
            },
            Self::Protocol(detail) => Self::Protocol(detail.clone()),
            Self::Changed(detail) => Self::Changed(detail.clone()),
            Self::Snapshot(detail) => Self::Snapshot(detail.clone()),
            Self::PeerIncompatible { reason, detail } => Self::PeerIncompatible {
                reason: *reason,
                detail: detail.clone(),
            },
            Self::NewerDatabase {
                what,
                found,
                supported,
            } => Self::NewerDatabase {
                what: what.clone(),
                found: *found,
                supported: *supported,
            },
            Self::Cancelled => Self::Cancelled,
            Self::Io(error) => Self::Io(std::io::Error::new(error.kind(), error.to_string())),
            Self::Tls { kind, detail } => Self::Tls {
                kind: *kind,
                detail: detail.clone(),
            },
        }
    }

    /// The server's `Retry-After` hint for a `503` or `BUSY` reply, in seconds.
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            Self::Starting { retry_after, .. }
            | Self::Busy { retry_after, .. }
            | Self::RootOffline { retry_after, .. }
            | Self::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

fn peer_message(reason: PeerIncompatibility, detail: &str, chinese: bool) -> String {
    let text = match (reason, chinese) {
        (PeerIncompatibility::ServerUpgradeRequired, false) => {
            "The server is too old for this client. Upgrade AudioLib Server to the same or a newer release; the local cache was kept."
        }
        (PeerIncompatibility::ServerUpgradeRequired, true) => {
            "服务器版本过旧，与本客户端不兼容。请将 AudioLib Server 升级到相同或更新的版本；本地缓存已保留。"
        }
        (PeerIncompatibility::ClientUpgradeRequired, false) => {
            "The server requires a newer client. Update AudioLib Explorer; the local cache was kept."
        }
        (PeerIncompatibility::ClientUpgradeRequired, true) => {
            "服务器要求更新的客户端。请更新 AudioLib Explorer；本地缓存已保留。"
        }
        (PeerIncompatibility::Blocked, false) => {
            "This client or server version is known to be faulty and was refused. Upgrade both to the latest release; the local cache was kept."
        }
        (PeerIncompatibility::Blocked, true) => {
            "该客户端或服务器版本存在已知问题，已被拒绝。请将两端都升级到最新版本；本地缓存已保留。"
        }
        (PeerIncompatibility::Inconsistent, false) => {
            "The server reported inconsistent version information. Restart or reinstall the server; the local cache was kept."
        }
        (PeerIncompatibility::Inconsistent, true) => {
            "服务器报告的版本信息不一致。请重启或重新安装服务器；本地缓存已保留。"
        }
        (PeerIncompatibility::ClientCatalogInvalid, false) => {
            "This client sent invalid compatibility information. Restart or reinstall AudioLib Explorer; the local cache was kept."
        }
        (PeerIncompatibility::ProtocolUnsupported, false) => {
            "The server and this client speak incompatible API protocols. Upgrade both to the latest release; the local cache was kept."
        }
        (PeerIncompatibility::ProtocolUnsupported, true) => {
            "服务器与本客户端的 API 协议不兼容。请将两端都升级到最新版本；本地缓存已保留。"
        }
        (PeerIncompatibility::ClientCatalogInvalid, true) => {
            "本客户端发送的兼容性信息无效。请重启或重新安装 AudioLib Explorer；本地缓存已保留。"
        }
    };
    if detail.is_empty() {
        text.to_owned()
    } else if chinese {
        format!("{text}（{detail}）")
    } else {
        format!("{text} ({detail})")
    }
}

impl From<audiolib_core::Error> for RemoteError {
    fn from(error: audiolib_core::Error) -> Self {
        match error {
            audiolib_core::Error::Io(error) => Self::Io(error),
            audiolib_core::Error::Cancelled => Self::Cancelled,
            audiolib_core::Error::NewerDatabase {
                what,
                found,
                supported,
            } => Self::NewerDatabase {
                what,
                found,
                supported,
            },
            other => Self::Snapshot(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RemoteError, retry_wait};

    #[test]
    fn marker_refusal_codes_name_the_side_to_act() {
        use super::PeerIncompatibility as P;
        for (code, sub_code, expected) in [
            ("PEER_UPGRADE_REQUIRED", "", P::ClientUpgradeRequired),
            (
                "PROTOCOL_UNSUPPORTED",
                "SERVER_UPGRADE_REQUIRED",
                P::ServerUpgradeRequired,
            ),
            ("PROTOCOL_UNSUPPORTED", "", P::ServerUpgradeRequired),
            ("PEER_VERSION_BLOCKED", "", P::Blocked),
            ("PEER_CATALOG_INVALID", "", P::ClientCatalogInvalid),
            ("PEER_VERSION_UNSUPPORTED", "", P::ServerUpgradeRequired),
            (
                "PROTOCOL_UNSUPPORTED",
                "FUTURE_HINT",
                P::ProtocolUnsupported,
            ),
        ] {
            let body =
                format!(r#"{{"error_code":"{code}","sub_code":"{sub_code}","message":"refused"}}"#);
            for context in ["GET /snapshot", "PUT /settings", "PATCH /files/1"] {
                let error = RemoteError::from_status(409, &body, context);
                assert!(
                    matches!(&error, RemoteError::PeerIncompatible { reason, .. } if *reason == expected),
                    "{code} {context}: {error:?}"
                );
                let english = error.localized(false);
                let chinese = error.localized(true);
                assert!(english.contains("cache was kept"), "{english}");
                assert!(chinese.contains("缓存已保留"), "{chinese}");
                match expected {
                    P::ServerUpgradeRequired => {
                        assert!(english.contains("Upgrade AudioLib Server"))
                    }
                    P::ClientUpgradeRequired => {
                        assert!(english.contains("Update AudioLib Explorer"))
                    }
                    P::ClientCatalogInvalid => {
                        assert!(english.contains("reinstall AudioLib Explorer"))
                    }
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn protocol_unsupported_sub_codes_choose_the_message() {
        use super::PeerIncompatibility as P;
        let reason = |sub_code: &str| {
            let body = format!(
                r#"{{"error_code":"PROTOCOL_UNSUPPORTED","sub_code":"{sub_code}","message":"refused"}}"#
            );
            match RemoteError::from_status(409, &body, "GET /snapshot") {
                RemoteError::PeerIncompatible { reason, detail } => (
                    reason,
                    RemoteError::PeerIncompatible { reason, detail }.localized(false),
                ),
                other => panic!("{sub_code}: {other:?}"),
            }
        };
        let (upgrade, text) = reason("SERVER_UPGRADE_REQUIRED");
        assert_eq!(upgrade, P::ServerUpgradeRequired);
        assert!(text.contains("Upgrade AudioLib Server"), "{text}");
        assert!(!text.contains("Update AudioLib Explorer"), "{text}");
        for unknown in ["CLIENT_SOMETHING", "FUTURE_HINT"] {
            let (generic, text) = reason(unknown);
            assert_eq!(generic, P::ProtocolUnsupported, "{unknown}");
            assert!(text.contains("incompatible API protocols"), "{text}");
            assert!(!text.contains("Upgrade AudioLib Server to"), "{text}");
            assert!(!text.contains("Update AudioLib Explorer"), "{text}");
        }
    }

    #[test]
    fn mdns_host_failures_suggest_the_lan_ip() {
        assert!(super::has_mdns_host("http://example.local:8790/api/v1"));
        assert!(super::has_mdns_host("https://Server.LOCAL./x"));
        assert!(!super::has_mdns_host("http://203.0.113.2:8790/api/v1"));
        assert!(!super::has_mdns_host("https://example.com/local"));
        let unreachable = RemoteError::Unreachable(
            "http://example.local:8790/api/v1/health: connection failed".into(),
        );
        assert!(unreachable.localized(true).contains("局域网 IP"));
        assert!(unreachable.localized(false).contains("LAN IP"));
        let unauthorized = RemoteError::Unauthorized(
            "GET /library: invalid token (http://example.local:8790/api/v1)".into(),
        );
        assert!(unauthorized.localized(true).contains("局域网 IP"));
        assert!(
            unauthorized
                .duplicate()
                .without_token()
                .localized(false)
                .contains("LAN IP")
        );
        let plain = RemoteError::Unreachable("http://203.0.113.2:8790: connection failed".into());
        assert!(!plain.localized(true).contains("fake-ip"));
    }

    #[test]
    fn auth_rate_limit_and_lan_only_codes_map_to_dedicated_errors() {
        let body = r#"{"error_code":"AUTH_RATE_LIMITED","message":"slow down"}"#;
        let limited = RemoteError::from_response(429, body, "GET /library", Some(60));
        assert!(matches!(
            limited,
            RemoteError::RateLimited {
                retry_after: Some(60),
                ..
            }
        ));
        let bare = RemoteError::from_response(429, "", "GET /library", None);
        assert!(matches!(
            bare,
            RemoteError::RateLimited {
                retry_after: None,
                ..
            }
        ));
        assert!(bare.localized(true).contains("60 秒"));
        assert!(bare.localized(true).contains("即使令牌正确也会被拒绝"));
        assert!(bare.localized(true).contains("本客户端 IP 地址"));
        assert!(!bare.localized(true).contains("本机"));
        assert!(
            bare.localized(false)
                .contains("this client's IP address as the server sees it")
        );
        assert!(!bare.localized(false).contains("this PC"));
        assert!(
            bare.localized(false)
                .contains("even a correct token is rejected")
        );
        let host = r#"{"error_code":"HOST_FORBIDDEN","message":"no"}"#;
        assert!(matches!(
            RemoteError::from_status(403, host, "GET /library"),
            RemoteError::HostForbidden(_)
        ));
        let admin = r#"{"error_code":"ADMIN_LAN_ONLY","message":"no"}"#;
        let admin = RemoteError::from_status(403, admin, "GET /admin");
        assert!(matches!(admin, RemoteError::AdminLanOnly(_)));
        assert!(!admin.is_unauthorized());
    }

    #[test]
    fn insufficient_storage_is_a_disk_full_error_that_is_never_retried() {
        let by_status = RemoteError::from_response(
            507,
            r#"{"error_code":"INSUFFICIENT_STORAGE","message":"no space left"}"#,
            "GET /library/snapshot",
            Some(5),
        );
        assert!(
            matches!(by_status, RemoteError::InsufficientStorage(_)),
            "{by_status:?}"
        );
        // No Retry-After hint is exposed, and it is none of the retried classes.
        assert_eq!(by_status.retry_after(), None);
        assert!(!by_status.is_slow() && !by_status.is_unreachable() && !by_status.is_offline());
        assert!(matches!(
            RemoteError::from_response(507, "", "GET /x", None),
            RemoteError::InsufficientStorage(_)
        ));
        assert!(matches!(
            RemoteError::from_audio_response(507, "", "GET /x", None),
            RemoteError::InsufficientStorage(_)
        ));
        assert!(by_status.localized(true).contains("服务器磁盘不足"));
        assert!(by_status.localized(false).contains("disk is full"));
        assert!(matches!(
            by_status.duplicate(),
            RemoteError::InsufficientStorage(_)
        ));
    }

    #[test]
    fn retry_wait_prefers_retry_after_and_is_capped() {
        use std::time::Duration;
        let cap = Duration::from_secs(10);
        let backoff = Duration::from_millis(500);
        assert_eq!(retry_wait(backoff, Some(3), cap), Duration::from_secs(3));
        assert_eq!(retry_wait(backoff, Some(300), cap), cap);
        assert_eq!(retry_wait(backoff, None, cap), backoff);
        // `Retry-After: 0` must not turn into a busy loop.
        assert_eq!(retry_wait(backoff, Some(0), cap), backoff);
        assert_eq!(retry_wait(Duration::from_secs(60), None, cap), cap);
    }

    #[test]
    fn preview_refusals_are_classified_before_the_503_arm() {
        let unsupported = RemoteError::from_audio_response(
            415,
            r#"{"error_code":"PREVIEW_UNSUPPORTED_FORMAT","message":"Audio 7 cannot be previewed with codec=pcm (WAV codec 0x0011 (IMA ADPCM)); use codec=flac or /stream"}"#,
            "GET /api/v1/audio/7/preview",
            None,
        );
        assert!(
            matches!(&unsupported, RemoteError::PreviewUnavailable { code, .. } if code == "PREVIEW_UNSUPPORTED_FORMAT"),
            "{unsupported:?}"
        );
        let bare = RemoteError::from_audio_response(415, "", "GET /p", None);
        assert!(
            matches!(bare, RemoteError::PreviewUnavailable { .. }),
            "{bare:?}"
        );
        let no_ffmpeg = RemoteError::from_audio_response(
            503,
            r#"{"error_code":"TRANSCODER_UNAVAILABLE","message":"ffmpeg not found"}"#,
            "GET /p",
            None,
        );
        assert!(
            matches!(&no_ffmpeg, RemoteError::PreviewUnavailable { code, .. } if code == "TRANSCODER_UNAVAILABLE"),
            "{no_ffmpeg:?}"
        );
        assert!(no_ffmpeg.localized(true).contains("原始文件"));
        // A busy preview is the shared stream cap, retried like /stream.
        let busy = RemoteError::from_audio_response(
            503,
            r#"{"error_code":"BUSY","message":"too many"}"#,
            "GET /p",
            Some(1),
        );
        assert!(matches!(
            busy,
            RemoteError::Busy {
                retry_after: Some(1),
                ..
            }
        ));
    }

    #[test]
    fn token_errors_follow_the_server_codes_and_point_to_the_admin_page() {
        // §2.2: missing and wrong tokens are both `401 UNAUTHORIZED`; whether the request
        // carried a token decides which message is shown.
        let missing_body = r#"{"error_code":"UNAUTHORIZED","message":"A bearer token is required: this request is not from a trusted direct LAN peer"}"#;
        let required =
            RemoteError::from_response(401, missing_body, "GET /library", None).without_token();
        assert!(
            matches!(required, RemoteError::TokenRequired(_)),
            "{required:?}"
        );
        // The message text is never matched: with a token sent it is a rejection.
        assert!(matches!(
            RemoteError::from_response(401, missing_body, "GET /library", None),
            RemoteError::Unauthorized(_)
        ));
        // A code the server does not send is not treated as a token problem by itself.
        assert!(
            !RemoteError::from_response(
                400,
                r#"{"error_code":"TOKEN_REQUIRED","message":"x"}"#,
                "GET /library",
                None
            )
            .is_unauthorized()
        );
        let rejected = RemoteError::from_response(
            401,
            r#"{"error_code":"UNAUTHORIZED","message":"Invalid token"}"#,
            "GET /library",
            None,
        );
        assert!(
            matches!(rejected, RemoteError::Unauthorized(_)),
            "{rejected:?}"
        );
        // A bare 401 (no body) stays "rejected"; the client turns it into "required" when
        // it sent no token.
        assert!(matches!(
            RemoteError::from_status(401, "", "GET /roots"),
            RemoteError::Unauthorized(_)
        ));
        assert!(matches!(
            RemoteError::from_status(401, "", "GET /roots").without_token(),
            RemoteError::TokenRequired(_)
        ));
        for error in [&required, &rejected] {
            assert!(error.is_unauthorized() && !error.is_offline());
            let chinese = error.localized(true);
            let english = error.localized(false);
            assert!(
                chinese.contains("401") && chinese.contains(":8790/admin"),
                "{chinese}"
            );
            assert!(
                english.contains("401") && english.contains(":8790/admin"),
                "{english}"
            );
            assert!(english.contains("web admin page"));
            assert!(chinese.contains("网页管理页面"));
        }
        assert!(
            required
                .localized(false)
                .contains("requires an access token")
        );
        assert!(
            rejected
                .localized(false)
                .contains("rejected the access token")
        );
        // The token never appears (it is not part of the error at all).
        assert!(!format!("{required:?}{rejected:?}").contains("Bearer"));
    }

    #[test]
    fn transport_failures_name_dns_and_refused_connections() {
        let refused = RemoteError::from_io(
            &std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
            "https://h:8443/api/v1/health",
        );
        assert_eq!(
            refused.network_failure(),
            Some(super::NetworkFailure::Refused)
        );
        assert!(refused.is_offline());
        assert!(refused.localized(false).contains("refused the connection"));
        let dns = RemoteError::from_transport(ureq::Error::HostNotFound, "https://nope:1");
        assert_eq!(dns.network_failure(), Some(super::NetworkFailure::Dns));
        assert!(dns.is_offline());
        assert!(dns.localized(true).contains("DNS"));
        let timeout = RemoteError::from_io(
            &std::io::Error::from(std::io::ErrorKind::TimedOut),
            "https://h",
        );
        assert!(matches!(timeout, RemoteError::Timeout(_)));
        let other = RemoteError::from_io(
            &std::io::Error::from(std::io::ErrorKind::ConnectionReset),
            "https://h",
        );
        assert!(matches!(other, RemoteError::Unreachable(_)) && other.network_failure().is_none());
        assert!(
            other
                .localized(false)
                .starts_with("Cannot reach the server")
        );
        // TLS failures are configuration problems: not offline, not a token problem, and
        // never a "busy"/"starting" server.
        let tls = RemoteError::from_transport(
            ureq::Error::Rustls(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            )),
            "https://h",
        );
        assert!(matches!(
            tls,
            RemoteError::Tls {
                kind: super::TlsFailure::Untrusted,
                ..
            }
        ));
        assert!(!tls.is_offline() && !tls.is_unauthorized() && tls.retry_after().is_none());
        assert!(matches!(
            audiolib_core::Error::from(tls),
            audiolib_core::Error::Database(_)
        ));
    }

    #[test]
    fn newer_database_says_the_client_must_be_upgraded() {
        let error = RemoteError::from(audiolib_core::Error::NewerDatabase {
            what: "feature tokenization revision".into(),
            found: 3,
            supported: 1,
        });
        assert!(matches!(error, RemoteError::NewerDatabase { found: 3, .. }));
        let chinese = error.localized(true);
        assert!(
            chinese.starts_with("客户端版本过旧") && chinese.contains("3 > 1"),
            "{chinese}"
        );
        assert!(!chinese.contains("快照校验失败"), "{chinese}");
        let english = error.localized(false);
        assert!(
            english.contains("too old") && english.contains("Upgrade AudioLib Explorer"),
            "{english}"
        );
        assert!(matches!(
            audiolib_core::Error::from(error),
            audiolib_core::Error::NewerDatabase { supported: 1, .. }
        ));
    }

    #[test]
    fn root_offline_is_classified_by_its_error_code() {
        let body = r#"{"error_code":"ROOT_OFFLINE","message":"Library root \"SFX\" is offline: Root is not answering reliably: timed out"}"#;
        for error in [
            RemoteError::from_audio_response(503, body, "GET /api/v1/audio/7/stream", Some(5)),
            RemoteError::from_response(503, body, "GET /api/v1/audio/7/waveform", Some(5)),
        ] {
            assert!(
                matches!(&error, RemoteError::RootOffline { message, retry_after: Some(5) } if message.starts_with("Library root \"SFX\"")),
                "{error:?}"
            );
            assert!(error.localized(true).contains("根目录"));
        }
        // The code decides, whatever the message says.
        let terse = RemoteError::from_audio_response(
            503,
            r#"{"error_code":"ROOT_OFFLINE","message":"earlier file opens have not finished yet"}"#,
            "GET /api/v1/audio/7/stream",
            Some(5),
        );
        assert!(
            matches!(terse, RemoteError::RootOffline { .. }),
            "{terse:?}"
        );
        // UNAVAILABLE (or no body) on an audio endpoint is the library still opening.
        for body in [
            r#"{"error_code":"UNAVAILABLE","message":"The library is still opening; retry shortly"}"#,
            "",
        ] {
            let starting =
                RemoteError::from_audio_response(503, body, "GET /api/v1/audio/7/stream", None);
            assert!(
                matches!(starting, RemoteError::Starting { .. }),
                "{body}: {starting:?}"
            );
        }
    }

    #[test]
    fn audio_503_is_a_root_offline_only_when_it_names_a_root() {
        let root = RemoteError::from_audio_response(
            503,
            r#"{"error_code":"UNAVAILABLE","message":"Library root \"SFX\" is offline: Root directory not found (volume not mounted?)"}"#,
            "GET /api/v1/audio/7/stream",
            Some(5),
        );
        assert!(
            matches!(&root, RemoteError::RootOffline { message, retry_after: Some(5) } if message.starts_with("Library root \"SFX\"")),
            "{root:?}"
        );
        // The whole library still opening (or failing to open) is a starting server.
        for body in [
            r#"{"error_code":"UNAVAILABLE","message":"The library is still opening; retry shortly"}"#,
            r#"{"error_code":"UNAVAILABLE","message":"The library could not be opened; the server keeps retrying"}"#,
            "",
        ] {
            let starting =
                RemoteError::from_audio_response(503, body, "GET /api/v1/audio/7/stream", Some(5));
            assert!(
                matches!(
                    starting,
                    RemoteError::Starting {
                        retry_after: Some(5),
                        ..
                    }
                ),
                "{body}: {starting:?}"
            );
            assert!(!starting.localized(true).contains("根目录"));
        }
    }

    #[test]
    fn manage_forbidden_is_its_own_error_and_not_a_token_problem() {
        for (body, cn, en) in [
            (
                r#"{"error_code":"MANAGE_FORBIDDEN","message":"Management is limited to the LAN"}"#,
                "仅限局域网",
                "local network",
            ),
            (
                r#"{"error_code":"MANAGE_FORBIDDEN","message":"Remote management is disabled"}"#,
                "已关闭远程管理",
                "disabled",
            ),
        ] {
            let error = RemoteError::from_response(403, body, "POST /scans", None);
            assert!(matches!(error, RemoteError::Forbidden(_)), "{error:?}");
            assert!(!error.is_unauthorized() && !error.is_offline());
            assert!(!error.write_may_have_committed());
            assert!(
                error.localized(true).contains(cn),
                "{}",
                error.localized(true)
            );
            assert!(error.localized(false).contains(en));
            assert!(matches!(error.duplicate(), RemoteError::Forbidden(_)));
        }
        // `409 SCAN_STATE` is its own error; any other 409 is still `Busy`.
        let state = RemoteError::from_response(
            409,
            r#"{"error_code":"SCAN_STATE","message":"Scan session 17 has already finished"}"#,
            "POST /scans/17/pause",
            None,
        );
        assert!(matches!(state, RemoteError::ScanState(_)), "{state:?}");
        assert!(state.localized(true).contains("扫描当前的状态不允许"));
        assert!(!state.write_may_have_committed() && !state.is_offline());
        assert!(matches!(state.duplicate(), RemoteError::ScanState(_)));
        for body in [r#"{"error_code":"CONFLICT","message":"x"}"#, "not json"] {
            assert!(matches!(
                RemoteError::from_response(409, body, "POST /scans", None),
                RemoteError::Busy { .. }
            ));
        }
        // The admin page's LAN rule has its own error; an unknown 403 stays a server error.
        assert!(matches!(
            RemoteError::from_status(
                403,
                r#"{"error_code":"ADMIN_LAN_ONLY","message":"x"}"#,
                "GET /x"
            ),
            RemoteError::AdminLanOnly(_)
        ));
        assert!(matches!(
            RemoteError::from_status(403, r#"{"error_code":"OTHER_403","message":"x"}"#, "GET /x"),
            RemoteError::Server { status: 403, .. }
        ));
    }

    #[test]
    fn connect_phase_timeouts_are_unreachable_but_response_timeouts_are_slow() {
        let url = "http://h:8790/api/v1/library";
        for phase in [ureq::Timeout::Resolve, ureq::Timeout::Connect] {
            let error = RemoteError::from_transport(ureq::Error::Timeout(phase), url);
            assert!(
                error.is_unreachable() && !error.is_slow(),
                "{phase:?}: {error:?}"
            );
            assert!(error.is_offline());
        }
        for phase in [ureq::Timeout::RecvResponse, ureq::Timeout::RecvBody] {
            let error = RemoteError::from_transport(ureq::Error::Timeout(phase), url);
            assert!(
                error.is_slow() && !error.is_unreachable(),
                "{phase:?}: {error:?}"
            );
            assert!(error.is_offline());
        }
        let refused = RemoteError::from_io(
            &std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
            url,
        );
        assert!(refused.is_unreachable() && !refused.is_slow());
        let stalled =
            RemoteError::from_io(&std::io::Error::from(std::io::ErrorKind::TimedOut), url);
        assert!(stalled.is_slow());
    }

    #[test]
    fn server_request_timeout_is_transient() {
        let timeout = RemoteError::from_response(
            408,
            r#"{"error_code":"TIMEOUT","message":"request took longer than 30 s"}"#,
            "GET /library",
            None,
        );
        assert!(
            matches!(timeout, RemoteError::Timeout(ref message) if message.contains("30 s")),
            "{timeout:?}"
        );
        assert!(timeout.is_offline() && timeout.is_slow() && !timeout.is_unreachable());
        assert!(!timeout.is_unauthorized());
        assert!(timeout.localized(true).contains("超时"));
        assert!(!timeout.localized(false).contains("token"));
        assert!(matches!(
            crate::poll::RevisionCheck::classify(None, Err(timeout), false),
            crate::poll::RevisionCheck::Slow(_)
        ));
        // A bare 408 without a body maps the same way.
        assert!(matches!(
            RemoteError::from_status(408, "", "GET /roots"),
            RemoteError::Timeout(_)
        ));
        // Other new server replies stay what they are.
        assert!(matches!(
            RemoteError::from_status(
                405,
                r#"{"error_code":"METHOD_NOT_ALLOWED","message":"use GET"}"#,
                "POST /library"
            ),
            RemoteError::Server { status: 405, ref code, .. } if code == "METHOD_NOT_ALLOWED"
        ));
    }

    #[test]
    fn busy_keeps_retry_after_and_is_neither_starting_nor_a_token_problem() {
        let capped = RemoteError::from_response(
            503,
            r#"{"error_code":"BUSY","message":"too many audio streams"}"#,
            "GET /api/v1/audio/7/stream",
            Some(1),
        );
        assert!(matches!(
            capped,
            RemoteError::Busy { ref message, retry_after: Some(1) } if message.contains("too many audio streams")
        ));
        assert_eq!(capped.retry_after(), Some(1));
        assert!(!capped.is_offline());
        assert!(matches!(
            capped.duplicate(),
            RemoteError::Busy {
                retry_after: Some(1),
                ..
            }
        ));
        let chinese = capped.localized(true);
        assert!(chinese.contains("正忙") && !chinese.contains("启动") && !chinese.contains("令牌"));
        let english = capped.localized(false);
        assert!(
            english.contains("busy") && !english.contains("starting") && !english.contains("token")
        );
        assert!(matches!(
            audiolib_core::Error::from(capped),
            audiolib_core::Error::Busy(_)
        ));
        // Snapshot slots stay `409 BUSY` (no Retry-After).
        assert!(matches!(
            RemoteError::from_status(409, r#"{"error_code":"BUSY"}"#, "GET /library/snapshot"),
            RemoteError::Busy {
                retry_after: None,
                ..
            }
        ));
        // A 503 without the BUSY code is still a starting server.
        assert!(matches!(
            RemoteError::from_response(
                503,
                r#"{"error_code":"UNAVAILABLE"}"#,
                "GET /library",
                Some(5)
            ),
            RemoteError::Starting {
                retry_after: Some(5),
                ..
            }
        ));
    }
}
