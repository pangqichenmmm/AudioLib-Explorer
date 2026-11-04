// crates/audiolib-remote/src/probe.rs
//! "Test connection": a structured, timed, per-step connectivity report.
//!
//! Steps: DNS + TCP connect, TLS handshake (`https://` only; the same OS trust store as
//! every request), `/health`, token + library (`/library`), roots, a one-byte
//! `/audio/{id}/stream` sample, a small `/audio/{id}/preview` throughput sample, then the
//! optional per-root WebDAV checks.
//!
//! The server's `/audio/{id}/stream` is the primary audio source, so WebDAV steps are
//! optional: a root without a WebDAV URL is skipped with a note, and an unreachable WebDAV
//! URL is a warning that does not fail the report.
use crate::client::{RemoteClient, RemoteLibrary, RemoteRoot, agent};
use crate::error::{RemoteError, Result};
use crate::mapping::{EffectiveRoot, UrlSource, effective_roots, root_status_text};
use crate::secrets::RemoteSecrets;
use crate::stream::WebDavAuth;
use crate::stream::probe_stream;
use crate::sync::{RemoteCache, snapshot_schema_error};
use crate::tls::TlsInfo;
use audiolib_core::config::DataSourceSettings;
use audiolib_core::server_url::{Scheme, ServerAddress};
use serde::Serialize;
use std::io::Read;
use std::time::{Duration, Instant};

/// One checked stage.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStep {
    /// DNS resolution and TCP connect to the server's host and port.
    Connect,
    /// TLS handshake and certificate verification (`https://`); a note for `http://`.
    Tls,
    Health,
    Library,
    Roots,
    /// `GET /audio/{id}/stream` with `Range: bytes=0-0` on one synced record.
    Stream,
    /// A small `GET /audio/{id}/preview` range to estimate audition throughput (optional).
    Preview,
    WebDav,
}

/// Outcome of one stage; `skipped` steps were not attempted (a prerequisite failed, or for
/// optional steps there was nothing to check).
#[derive(Clone, Debug, Serialize)]
pub struct StepReport {
    pub step: ProbeStep,
    /// Root name for WebDAV steps, otherwise empty.
    pub target: String,
    pub ok: bool,
    pub skipped: bool,
    /// The step does not decide the overall result: a failure is a warning (WebDAV, a
    /// sampled record whose file is missing on the server).
    pub optional: bool,
    pub elapsed_ms: u64,
    /// Human-readable, localized result.
    pub message: String,
    /// Effective URL checked by WebDAV steps.
    pub url: Option<String>,
    pub url_source: Option<UrlSource>,
}

/// Full report returned by [`test_connection`].
#[derive(Clone, Debug, Default, Serialize)]
pub struct ConnectionReport {
    pub steps: Vec<StepReport>,
    pub server_version: Option<String>,
    /// `/health` status: `ok`, `starting` or `error` (library not open yet).
    pub server_status: Option<String>,
    pub library: Option<RemoteLibrary>,
    /// Roots from the server, or the supplied cached roots when the server failed.
    pub roots: Vec<RemoteRoot>,
    pub effective_roots: Vec<EffectiveRoot>,
    /// Normalised server base URL that was tested.
    pub server_url: Option<String>,
    /// TLS session details for `https://` servers.
    pub tls: Option<TlsInfo>,
    /// External address announced by the server's `/health` (§6.2), normalised; `None`
    /// when not configured or not a valid http(s) URL.
    pub public_url: Option<String>,
    /// The token check failed with `401` (wrong token, or none while the server requires
    /// one): the desktop focuses the token field.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unauthorized: bool,
}

impl ConnectionReport {
    /// The server's external address when it differs from the tested one, e.g. to offer
    /// "use the external address" after a LAN test.
    pub fn other_public_url(&self) -> Option<&str> {
        let public = self.public_url.as_deref()?;
        let tested = self
            .server_url
            .as_deref()
            .and_then(|url| ServerAddress::parse(url).ok())
            .map(|address| address.api_url().to_ascii_lowercase());
        let announced = ServerAddress::parse(public)
            .ok()?
            .api_url()
            .to_ascii_lowercase();
        (tested.as_deref() != Some(announced.as_str())).then_some(public)
    }
}

impl ConnectionReport {
    /// True when every required step succeeded and none was skipped; optional steps
    /// (WebDAV, see [`StepReport::optional`]) never fail the report.
    pub fn ok(&self) -> bool {
        !self.steps.is_empty()
            && self
                .steps
                .iter()
                .all(|step| step.optional || (step.ok && !step.skipped))
    }

    /// `step · message` lines of optional steps that were attempted and failed.
    pub fn warnings(&self, chinese: bool) -> Vec<String> {
        self.steps
            .iter()
            .filter(|step| step.optional && !step.ok && !step.skipped)
            .map(|step| format!("{} · {}", step_name(step, chinese), step.message))
            .collect()
    }

    /// The server listens but its library is still opening (`/health` `starting`/`error`
    /// or `503 UNAVAILABLE`): a distinct "retry later" state, not a token problem.
    pub fn server_starting(&self) -> bool {
        matches!(self.server_status.as_deref(), Some("starting" | "error"))
    }

    /// One line for a profile's `last_test` (no secrets, no paths): `可连接 · 0.3.6 · r1287`
    /// (plus the number of warnings) when the test passed, else the first failed required
    /// step and the server's or client's message.
    pub fn verdict(&self, chinese: bool) -> String {
        if self.ok() {
            let mut parts = vec![if chinese { "可连接" } else { "ok" }.to_owned()];
            parts.extend(self.server_version.clone());
            if let Some(library) = &self.library {
                parts.push(format!("r{}", library.revision));
            }
            let warnings = self.warnings(chinese).len();
            if warnings > 0 {
                parts.push(if chinese {
                    format!("{warnings} 个警告")
                } else {
                    format!("{warnings} warnings")
                });
            }
            return parts.join(" · ");
        }
        match self
            .steps
            .iter()
            .find(|step| !step.optional && !step.ok && !step.skipped)
        {
            Some(step) => format!("{} · {}", step_name(step, chinese), step.message),
            None if chinese => "测试未完成".to_owned(),
            None => "test incomplete".to_owned(),
        }
    }

    /// Multi-line `√/× step · message · latency` summary for the GUI and CLI.
    pub fn summary(&self, chinese: bool) -> String {
        self.steps
            .iter()
            .map(|step| {
                let mark = if step.skipped {
                    "–"
                } else if step.step == ProbeStep::Health && self.server_starting() {
                    "…"
                } else if step.ok {
                    "√"
                } else if step.optional {
                    "!"
                } else {
                    "×"
                };
                let name = step_name(step, chinese);
                if step.skipped {
                    format!("{mark} {name} · {}", step.message)
                } else {
                    format!("{mark} {name} · {} · {} ms", step.message, step.elapsed_ms)
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn step_name(step: &StepReport, chinese: bool) -> String {
    let name = match (step.step, chinese) {
        (ProbeStep::Connect, true) => "DNS / TCP 连接".to_owned(),
        (ProbeStep::Connect, false) => "DNS / TCP connect".to_owned(),
        (ProbeStep::Tls, true) => "TLS 握手 / 证书".to_owned(),
        (ProbeStep::Tls, false) => "TLS handshake / certificate".to_owned(),
        (ProbeStep::Preview, true) => format!("试听预览吞吐 {}", step.target),
        (ProbeStep::Preview, false) => format!("Preview throughput {}", step.target),
        (ProbeStep::Health, true) => "服务器在线".to_owned(),
        (ProbeStep::Health, false) => "Server health".to_owned(),
        (ProbeStep::Library, true) => "令牌检查 / 素材库".to_owned(),
        (ProbeStep::Library, false) => "Token check / library".to_owned(),
        (ProbeStep::Roots, true) => "扫描根目录".to_owned(),
        (ProbeStep::Roots, false) => "Scan roots".to_owned(),
        (ProbeStep::Stream, true) => format!("服务器音频流 {}", step.target),
        (ProbeStep::Stream, false) => format!("Audio stream {}", step.target),
        (ProbeStep::WebDav, _) => format!("WebDAV {}", step.target),
    };
    name.trim_end().to_owned()
}

fn step(
    step: ProbeStep,
    started: Instant,
    outcome: std::result::Result<String, String>,
) -> StepReport {
    let ok = outcome.is_ok();
    StepReport {
        step,
        target: String::new(),
        ok,
        skipped: false,
        optional: false,
        elapsed_ms: started.elapsed().as_millis() as u64,
        message: outcome.unwrap_or_else(|error| error),
        url: None,
        url_source: None,
    }
}

fn skipped(step: ProbeStep, chinese: bool) -> StepReport {
    StepReport {
        step,
        target: String::new(),
        ok: false,
        skipped: true,
        optional: false,
        elapsed_ms: 0,
        message: if chinese {
            "已跳过（前一步失败）"
        } else {
            "skipped (previous step failed)"
        }
        .into(),
        url: None,
        url_source: None,
    }
}

/// An optional step that had nothing to check: shown as `–` with an explanation.
fn note(step: ProbeStep, message: String) -> StepReport {
    StepReport {
        step,
        target: String::new(),
        ok: false,
        skipped: true,
        optional: true,
        elapsed_ms: 0,
        message,
        url: None,
        url_source: None,
    }
}

/// Check `/audio/{id}/stream` with a one-byte range on a present record of the synced
/// snapshot of the same library. Without such a snapshot the step is a note; a sampled
/// record whose file is missing on the server (`404`) is a warning. As §11 specifies,
/// everything else fails the step: `401`, `503` (also an offline root), an unreachable
/// server and a timeout (with a hint that the disk may be waking up).
fn stream_step(
    client: &RemoteClient,
    library: Option<&RemoteLibrary>,
    cache: Option<&RemoteCache>,
    chinese: bool,
) -> (StepReport, Option<i64>) {
    let started = Instant::now();
    let Some(id) = library
        .zip(cache)
        .and_then(|(library, cache)| cache.sample_audio_id(&library.library_id))
    else {
        let skipped = note(
            ProbeStep::Stream,
            if chinese {
                "未检查：本机还没有该库的同步快照（同步后可抽样检查音频流）".into()
            } else {
                "not checked: no synced snapshot of this library yet (sync to sample a record)"
                    .into()
            },
        );
        return (skipped, None);
    };
    let outcome = probe_stream(&client.stream_source(id));
    let missing = matches!(outcome, Err(RemoteError::NotFound(_)));
    // A sleeping NAS disk can take 20-30 s to answer the first audio request; the short
    // probe then times out. Still a failure, with a hint about the wake-up.
    let waking = matches!(outcome, Err(RemoteError::Timeout(_)));
    let message = outcome
        .map(|probe| match (probe.status, chinese) {
            (206, true) => format!(
                "可用（Range 206 · {} 字节{}）",
                probe.total.unwrap_or_default(),
                if probe.etag.is_some() { " · ETag" } else { "" }
            ),
            (206, false) => format!(
                "available (Range 206 · {} bytes{})",
                probe.total.unwrap_or_default(),
                if probe.etag.is_some() { " · ETag" } else { "" }
            ),
            (416, true) => "可用（该记录为空文件）".to_owned(),
            (416, false) => "available (the record is an empty file)".to_owned(),
            (status, true) => format!("可用，但服务器未按 Range 返回（HTTP {status}）"),
            (status, false) => format!("available, but the server ignored Range (HTTP {status})"),
        })
        .map_err(|error| {
            let text = error.localized(chinese);
            match (missing, chinese) {
                (true, true) => format!("{text}（该记录的文件在服务器上缺失，不影响其他试听）"),
                (true, false) => format!(
                    "{text} (this record's file is missing on the server; other records still play)"
                ),
                _ if waking && chinese => format!(
                    "{text}（音效库磁盘可能正在从休眠中唤醒，试听最多等待 {} 秒）",
                    crate::FIRST_BYTE_TIMEOUT.as_secs()
                ),
                _ if waking => format!(
                    "{text} (the library disk may be waking from sleep; playback waits up to {} s)",
                    crate::FIRST_BYTE_TIMEOUT.as_secs()
                ),
                _ => text,
            }
        });
    let mut entry = step(ProbeStep::Stream, started, message);
    entry.target = format!("#{id}");
    entry.optional = missing;
    let sampled = entry.ok.then_some(id);
    (entry, sampled)
}

/// Bytes requested by the preview throughput sample (about 2.7 s of 48 kHz/16-bit stereo).
const PREVIEW_SAMPLE_BYTES: u64 = 512 * 1024;

/// Fetch the first [`PREVIEW_SAMPLE_BYTES`] of `/audio/{id}/preview?codec=pcm&sr=48000
/// &bits=16` (§8.1) with the token and report the throughput. Optional: a failure (an
/// older server without previews, a format previews do not support) is a warning.
fn preview_step(client: &RemoteClient, id: Option<i64>, chinese: bool) -> StepReport {
    let Some(id) = id else {
        return note(
            ProbeStep::Preview,
            if chinese {
                "未检查：没有可抽样的记录（需要已同步快照且音频流检查通过）".into()
            } else {
                "not checked: no record to sample (needs a synced snapshot and a passing stream check)"
                    .into()
            },
        );
    };
    let started = Instant::now();
    let url = format!(
        "{}/audio/{id}/preview?codec=pcm&sr=48000&bits=16",
        client.api_url()
    );
    let outcome = (|| -> Result<(u64, Duration, Duration, String)> {
        // The shared keep-alive pool, like auditions (a stale pooled connection is retried).
        let agent = agent(Duration::from_secs(15), Duration::from_secs(30));
        let authorization = client.stream_source(id).auth.header();
        let range = format!("bytes=0-{}", PREVIEW_SAMPLE_BYTES - 1);
        let mut response = crate::abort::call_fresh(|| {
            let request = agent.get(&url).header("Range", &range);
            match &authorization {
                Some(value) => request.header("Authorization", value),
                None => request,
            }
        })
        .map_err(|error| RemoteError::from_transport(error, &url))?;
        let first_byte = started.elapsed();
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = response
                .body_mut()
                .with_config()
                .limit(64 * 1024)
                .read_to_string()
                .unwrap_or_default();
            let error = RemoteError::from_audio_response(status, &body, &url, None);
            return Err(
                if client.stream_source(id).auth == crate::stream::SourceAuth::None {
                    error.without_token()
                } else {
                    error
                },
            );
        }
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("?")
                .to_owned()
        };
        let format = format!(
            "{} Hz/{}-bit/{} ch",
            header("x-preview-sample-rate"),
            header("x-preview-bits"),
            header("x-preview-channels")
        );
        // At most the sample, even from a server that ignores Range (200, whole file).
        let mut reader = response
            .into_body()
            .into_reader()
            .take(PREVIEW_SAMPLE_BYTES);
        let mut buffer = vec![0u8; 64 * 1024];
        let mut received = 0u64;
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|error| RemoteError::from_io(&error, &url))?;
            if read == 0 {
                break;
            }
            received += read as u64;
        }
        Ok((received, first_byte, started.elapsed(), format))
    })();
    let message = outcome
        .map(|(bytes, first_byte, total, format)| {
            let seconds = total.as_secs_f64().max(0.001);
            let mbps = bytes as f64 * 8.0 / seconds / 1_000_000.0;
            if chinese {
                format!(
                    "PCM {format} · {bytes} 字节 / {seconds:.2} 秒 ≈ {mbps:.1} Mbps（首字节 {} ms；48 kHz/16-bit 立体声试听约需 1.5 Mbps）",
                    first_byte.as_millis()
                )
            } else {
                format!(
                    "PCM {format} · {bytes} bytes in {seconds:.2} s ≈ {mbps:.1} Mbps (first byte {} ms; 48 kHz/16-bit stereo audition needs about 1.5 Mbps)",
                    first_byte.as_millis()
                )
            }
        })
        .map_err(|error| {
            if chinese {
                format!("{}（可选检查，不影响服务器流试听）", error.localized(true))
            } else {
                format!(
                    "{} (optional check; server-stream playback is unaffected)",
                    error.localized(false)
                )
            }
        });
    let mut entry = step(ProbeStep::Preview, started, message);
    entry.target = format!("#{id}");
    entry.optional = true;
    entry
}

/// DNS + TCP, then the TLS handshake for `https://`. Returns whether the server can be
/// talked to (later steps are skipped otherwise).
fn network_steps(report: &mut ConnectionReport, address: &ServerAddress, chinese: bool) -> bool {
    let started = Instant::now();
    let host = address.bare_host();
    let port = address.effective_port();
    let connected = crate::tls::connect_tcp(host, port, crate::client::CONNECT_TIMEOUT);
    let stream = match connected {
        Ok((addresses, stream, dns, tcp)) => {
            let shown = addresses
                .iter()
                .take(3)
                .map(|address| address.ip().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let peer = stream
                .peer_addr()
                .map(|peer| peer.to_string())
                .unwrap_or_default();
            report.steps.push(step(
                ProbeStep::Connect,
                started,
                Ok(if chinese {
                    format!(
                        "{host} → {shown}（DNS {} ms）· 已连接 {peer}（TCP {} ms）",
                        dns.as_millis(),
                        tcp.as_millis()
                    )
                } else {
                    format!(
                        "{host} → {shown} (DNS {} ms) · connected to {peer} (TCP {} ms)",
                        dns.as_millis(),
                        tcp.as_millis()
                    )
                }),
            ));
            stream
        }
        Err(error) => {
            report.steps.push(step(
                ProbeStep::Connect,
                started,
                Err(error.localized(chinese)),
            ));
            if address.scheme == Scheme::Https {
                report.steps.push(skipped(ProbeStep::Tls, chinese));
            }
            return false;
        }
    };
    if address.scheme == Scheme::Http {
        drop(stream);
        report.steps.push(note(
            ProbeStep::Tls,
            if chinese {
                "未使用：http:// 连接不加密（仅适合局域网；外网请使用 https://）".into()
            } else {
                "not used: http:// is unencrypted (fine on the LAN; use https:// over the internet)"
                    .into()
            },
        ));
        return true;
    }
    let started = Instant::now();
    match crate::tls::handshake(stream, host, Duration::from_secs(10)) {
        Ok(info) => {
            // The handshake verified the name; a multi-name (SAN) certificate's CN may be
            // another host, so show both rather than suggest a mismatch.
            let subject = info.subject.as_deref().unwrap_or("?");
            let name = if subject.eq_ignore_ascii_case(&info.server_name) {
                subject.to_owned()
            } else if chinese {
                format!("{}（CN {subject}）", info.server_name)
            } else {
                format!("for {} (CN {subject})", info.server_name)
            };
            let issuer = info.issuer.as_deref().unwrap_or("?");
            let not_after = info.not_after.as_deref().unwrap_or("?");
            let certificate = if chinese {
                format!("证书 {name} · 颁发者 {issuer} · 到期 {not_after}")
            } else {
                format!("certificate {name} · issuer {issuer} · expires {not_after}")
            };
            let message = if chinese {
                format!(
                    "{} · {} · {certificate} · 已通过{}验证",
                    info.protocol, info.cipher_suite, info.trust_store
                )
            } else {
                format!(
                    "{} · {} · {certificate} · verified by the {}",
                    info.protocol, info.cipher_suite, info.trust_store
                )
            };
            report.tls = Some(info);
            report
                .steps
                .push(step(ProbeStep::Tls, started, Ok(message)));
            true
        }
        Err(error) => {
            report
                .steps
                .push(step(ProbeStep::Tls, started, Err(error.localized(chinese))));
            false
        }
    }
}

/// Check a WebDAV collection with `PROPFIND Depth: 0` (falls back to `HEAD` when the
/// server rejects the method). Returns the HTTP status on success (2xx/207).
pub fn probe_webdav(url: &str, auth: &WebDavAuth) -> Result<u16> {
    let agent = agent(Duration::from_secs(8), Duration::from_secs(10));
    let target = if url.ends_with('/') {
        url.to_owned()
    } else {
        format!("{url}/")
    };
    let mut builder = ureq::http::Request::builder()
        .method(
            ureq::http::Method::from_bytes(b"PROPFIND")
                .map_err(|error| RemoteError::Protocol(error.to_string()))?,
        )
        .uri(&target)
        .header("Depth", "0");
    if let Some(header) = auth.header() {
        builder = builder.header("Authorization", header);
    }
    let request = builder
        .body(())
        .map_err(|error| RemoteError::Validation(format!("{url}: {error}")))?;
    let mut status = agent
        .run(request)
        .map_err(|error| RemoteError::from_transport(error, url))?
        .status()
        .as_u16();
    if matches!(status, 405 | 501) {
        let mut head = agent.head(&target);
        if let Some(header) = auth.header() {
            head = head.header("Authorization", &header);
        }
        status = head
            .call()
            .map_err(|error| RemoteError::from_transport(error, url))?
            .status()
            .as_u16();
    }
    match status {
        200..=299 => Ok(status),
        401 | 403 => Err(RemoteError::Unauthorized(format!(
            "WebDAV {url}: HTTP {status}; check the WebDAV username and password"
        ))),
        _ => Err(RemoteError::from_status(
            status,
            "",
            &format!("WebDAV {url}"),
        )),
    }
}

/// Run health, token/library, roots, a one-byte audio stream check (on a record of the
/// synced snapshot in `cache`, when there is one for the server's library) and optional
/// per-root WebDAV checks. Roots fall back to `cached_roots` when the server is unavailable
/// so WebDAV URLs (including a global base URL) can still be verified.
/// The library step also verifies snapshot schema compatibility. A successful API
/// response is retained in the report even when its schema cannot be synced. Messages
/// are localized; secrets never appear.
pub fn test_connection(
    settings: &DataSourceSettings,
    secrets: &RemoteSecrets,
    cached_roots: &[RemoteRoot],
    cache: Option<&RemoteCache>,
    chinese: bool,
) -> ConnectionReport {
    let mut report = ConnectionReport::default();
    let address = ServerAddress::parse(&settings.server_url);
    let client = match &address {
        Ok(address) => match RemoteClient::new(&address.base_url(), &secrets.token) {
            Ok(client) => {
                report.server_url = Some(address.base_url());
                Some(client)
            }
            Err(error) => {
                report.steps.push(step(
                    ProbeStep::Connect,
                    Instant::now(),
                    Err(error.localized(chinese)),
                ));
                None
            }
        },
        Err(error) => {
            report.steps.push(step(
                ProbeStep::Connect,
                Instant::now(),
                Err(error.message(chinese)),
            ));
            None
        }
    };
    let reachable = match (&client, &address) {
        (Some(_), Ok(address)) => network_steps(&mut report, address, chinese),
        _ => false,
    };
    let mut server_roots = None;
    if client.is_some() && !reachable {
        for skipped_step in [
            ProbeStep::Health,
            ProbeStep::Library,
            ProbeStep::Roots,
            ProbeStep::Stream,
            ProbeStep::Preview,
        ] {
            report.steps.push(skipped(skipped_step, chinese));
        }
    }
    if let Some(client) = client.as_ref().filter(|_| reachable) {
        let started = Instant::now();
        let health = client.health();
        // Any /health answer means the server listens; the token is still checked while
        // the library opens (401 vs 503), so continue to the library step.
        let healthy = health.is_ok();
        let outcome = match health {
            Ok(health) => {
                report.server_version = Some(health.version.clone());
                report.server_status = Some(health.status.clone());
                report.public_url = health
                    .public_url
                    .as_deref()
                    .and_then(|url| ServerAddress::parse(url).ok())
                    .map(|address| address.base_url());
                let external = match report.other_public_url() {
                    Some(url) if chinese => format!(" · 服务器外网地址：{url}"),
                    Some(url) => format!(" · server external address: {url}"),
                    None => String::new(),
                };
                match health.status.as_str() {
                    "starting" => Err(if chinese {
                        format!(
                            "服务器正在启动（正在打开素材库），请稍后重试 · {}",
                            health.version
                        )
                    } else {
                        format!(
                            "server is starting (opening its library); retry shortly · {}",
                            health.version
                        )
                    }),
                    "error" => Err(if chinese {
                        format!(
                            "服务器无法打开素材库，正在自动重试：{}",
                            health.message.unwrap_or_default()
                        )
                    } else {
                        format!(
                            "server cannot open its library and keeps retrying: {}",
                            health.message.unwrap_or_default()
                        )
                    }),
                    _ => Ok(format!("{} {}{external}", health.status, health.version)),
                }
            }
            // Something answers, but not the AudioLib API: usually a wrong port or path
            // prefix, or a reverse proxy that does not forward to the server.
            Err(RemoteError::NotFound(_)) => Err(if chinese {
                format!(
                    "该地址不是 AudioLib 服务器 API（{}/health 返回 404）：请检查端口、路径前缀和反向代理的转发目标",
                    client.api_url()
                )
            } else {
                format!(
                    "no AudioLib server API at this address ({}/health returned 404): check the port, the path prefix and the reverse proxy target",
                    client.api_url()
                )
            }),
            Err(error) => Err(error.localized(chinese)),
        };
        report.steps.push(step(ProbeStep::Health, started, outcome));
        if healthy {
            let started = Instant::now();
            let library = client.library();
            let authorized = library.is_ok();
            let incompatibility = library.as_ref().ok().and_then(|library| {
                snapshot_schema_error(library.schema_version, chinese).or_else(|| {
                    crate::compatibility::live_library(
                        client,
                        library,
                        report.server_version.as_deref(),
                    )
                    .err()
                    .map(|error| error.localized(chinese))
                })
            });
            let compatible = authorized && incompatibility.is_none();
            report.unauthorized = library
                .as_ref()
                .err()
                .is_some_and(RemoteError::is_unauthorized);
            report.steps.push(step(
                ProbeStep::Library,
                started,
                library
                    .map_err(|error| error.localized(chinese))
                    .and_then(|library| {
                        let text = if chinese {
                            format!(
                                "库 {} · 版本 r{} · schema {}{}",
                                library.library_id,
                                library.revision,
                                library.schema_version,
                                library
                                    .audio_files()
                                    .map(|count| format!(" · {count} 个音效"))
                                    .unwrap_or_default()
                            )
                        } else {
                            format!(
                                "library {} · revision {} · schema {}{}",
                                library.library_id,
                                library.revision,
                                library.schema_version,
                                library
                                    .audio_files()
                                    .map(|count| format!(" · {count} sounds"))
                                    .unwrap_or_default()
                            )
                        };
                        report.library = Some(library);
                        match incompatibility {
                            Some(message) => Err(format!("{text} · {message}")),
                            None => Ok(text),
                        }
                    }),
            ));
            if authorized {
                let started = Instant::now();
                let roots = client.roots();
                report.steps.push(step(
                    ProbeStep::Roots,
                    started,
                    roots
                        .as_ref()
                        .map(|roots| {
                            let states = roots
                                .iter()
                                .map(|root| {
                                    format!("{} {}", root.name, root_status_text(root, chinese))
                                })
                                .collect::<Vec<_>>()
                                .join(if chinese { "；" } else { "; " });
                            match (chinese, roots.is_empty()) {
                                (true, true) => "0 个扫描根目录".to_owned(),
                                (false, true) => "0 scan roots".to_owned(),
                                (true, false) => format!("{} 个扫描根目录：{states}", roots.len()),
                                (false, false) => format!("{} scan roots: {states}", roots.len()),
                            }
                        })
                        .map_err(|error| error.localized(chinese)),
                ));
                server_roots = roots.ok();
                if compatible {
                    let (stream, sampled) =
                        stream_step(client, report.library.as_ref(), cache, chinese);
                    report.steps.push(stream);
                    report.steps.push(preview_step(client, sampled, chinese));
                } else {
                    report.steps.push(skipped(ProbeStep::Stream, chinese));
                    report.steps.push(skipped(ProbeStep::Preview, chinese));
                }
            } else {
                report.steps.push(skipped(ProbeStep::Roots, chinese));
                report.steps.push(skipped(ProbeStep::Stream, chinese));
                report.steps.push(skipped(ProbeStep::Preview, chinese));
            }
        } else {
            report.steps.push(skipped(ProbeStep::Library, chinese));
            report.steps.push(skipped(ProbeStep::Roots, chinese));
            report.steps.push(skipped(ProbeStep::Stream, chinese));
            report.steps.push(skipped(ProbeStep::Preview, chinese));
        }
    }
    report.roots = server_roots.unwrap_or_else(|| cached_roots.to_vec());
    report.effective_roots = effective_roots(&report.roots, &settings.webdav);
    let auth = WebDavAuth {
        username: settings.webdav.username.clone(),
        password: secrets.webdav_password.clone(),
    };
    for root in &report.effective_roots {
        let started = Instant::now();
        let Some(url) = &root.url else {
            let mut entry = note(
                ProbeStep::WebDav,
                if chinese {
                    "未配置 WebDAV，试听走服务器流".into()
                } else {
                    "no WebDAV URL configured; playback uses the server stream".into()
                },
            );
            entry.target = root.name.clone();
            entry.url_source = Some(root.source);
            report.steps.push(entry);
            continue;
        };
        let outcome = probe_webdav(url, &auth)
            .map(|status| {
                if chinese {
                    format!("可访问（HTTP {status}，来源：{}）", root.source.label(true))
                } else {
                    format!(
                        "reachable (HTTP {status}, source: {})",
                        root.source.label(false)
                    )
                }
            })
            .map_err(|error| {
                if chinese {
                    format!(
                        "{}（{}；WebDAV 为可选项，试听仍走服务器流）",
                        error.localized(true),
                        root.source.label(true)
                    )
                } else {
                    format!(
                        "{} ({}; WebDAV is optional, playback still uses the server stream)",
                        error.localized(false),
                        root.source.label(false)
                    )
                }
            });
        let mut entry = step(ProbeStep::WebDav, started, outcome);
        entry.target = root.name.clone();
        entry.url = Some(url.clone());
        entry.url_source = Some(root.source);
        // The server stream is the primary audio source; WebDAV never fails the report.
        entry.optional = true;
        report.steps.push(entry);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{TestResponse, TestServer};
    use crate::util::base64;
    use audiolib_core::config::DataSourceMode;
    use std::sync::{Arc, Mutex};

    #[test]
    fn connection_test_checks_schema_without_losing_successful_api_evidence() {
        for version in [2, 3, 4, 5, 6, 7] {
            for chinese in [false, true] {
                let requests = Arc::new(Mutex::new(Vec::new()));
                let server = {
                    let requests = requests.clone();
                    TestServer::start(move |request| {
                        requests
                            .lock()
                            .unwrap()
                            .push((request.method.clone(), request.path.clone()));
                        match request.path.as_str() {
                            "/api/v1/health" => crate::test_server::marker_health(),
                            "/api/v1/library" => TestResponse::json(
                                200,
                                &format!(
                                    r#"{{"library_id":"schema-test","schema_version":{version},"revision":42,"stats":{{"audio_files":123456}}}}"#
                                ),
                            ),
                            "/api/v1/capabilities" => {
                                crate::test_server::marker_capabilities(serde_json::json!({}))
                            }
                            "/api/v1/roots" => TestResponse::json(200, "[]"),
                            _ => TestResponse::json(500, "{}"),
                        }
                    })
                };
                let settings = DataSourceSettings {
                    mode: DataSourceMode::Server,
                    server_url: server.url.clone(),
                    ..Default::default()
                };
                let report =
                    test_connection(&settings, &RemoteSecrets::default(), &[], None, chinese);
                let supported = (3..=5).contains(&version);
                assert_eq!(report.ok(), supported, "{}", report.summary(chinese));
                assert!(!report.unauthorized && !report.server_starting());
                assert_eq!(
                    report.server_version.as_deref(),
                    Some(env!("CARGO_PKG_VERSION"))
                );
                let library = report.library.as_ref().unwrap();
                assert_eq!((library.schema_version, library.revision), (version, 42));
                assert_eq!(library.audio_files(), Some(123456));
                let step = report
                    .steps
                    .iter()
                    .find(|step| step.step == ProbeStep::Library)
                    .unwrap();
                assert_eq!(step.ok, supported);
                assert!(step.message.contains(&format!("schema {version}")));
                if version == 2 {
                    assert!(step.message.contains(if chinese {
                        "本客户端支持"
                    } else {
                        "this client"
                    }));
                    assert!(!step.skipped && !step.optional);
                } else if version >= 6 {
                    // Schema 6/7 must declare a modern format; without one the
                    // library step fails on the format-missing message.
                    assert!(step.message.contains("format"), "{0}", step.message);
                    assert!(!step.skipped && !step.optional);
                }
                assert!(report.steps.iter().all(|step| {
                    !matches!(step.step, ProbeStep::Health | ProbeStep::Roots) || step.ok
                }));
                let mut expected = vec![
                    ("GET".into(), "/api/v1/health".into()),
                    ("GET".into(), "/api/v1/library".into()),
                ];
                if supported {
                    expected.push(("GET".into(), "/api/v1/capabilities".into()));
                }
                expected.push(("GET".into(), "/api/v1/roots".into()));
                assert_eq!(*requests.lock().unwrap(), expected);
            }
        }
    }

    #[test]
    fn the_verdict_is_one_line_for_the_profile_list() {
        let ok = |step: ProbeStep| StepReport {
            step,
            target: String::new(),
            ok: true,
            skipped: false,
            optional: false,
            elapsed_ms: 3,
            message: "fine".into(),
            url: None,
            url_source: None,
        };
        let mut report = ConnectionReport {
            steps: vec![
                ok(ProbeStep::Connect),
                ok(ProbeStep::Health),
                ok(ProbeStep::Library),
            ],
            server_version: Some("0.3.5".into()),
            library: Some(crate::client::RemoteLibrary {
                library_id: "lib".into(),
                revision: 6,
                ..Default::default()
            }),
            ..ConnectionReport::default()
        };
        assert_eq!(report.verdict(true), "可连接 · 0.3.5 · r6");
        report.steps.push(StepReport {
            optional: true,
            ok: false,
            target: "SFX".into(),
            ..ok(ProbeStep::WebDav)
        });
        assert_eq!(report.verdict(false), "ok · 0.3.5 · r6 · 1 warnings");
        report.steps[2] = StepReport {
            ok: false,
            message: "The server requires an access token (401).".into(),
            ..ok(ProbeStep::Library)
        };
        report.steps.push(skipped(ProbeStep::Roots, true));
        assert_eq!(
            report.verdict(true),
            "令牌检查 / 素材库 · The server requires an access token (401)."
        );
        assert_eq!(ConnectionReport::default().verdict(true), "测试未完成");
    }

    #[test]
    fn starting_server_is_a_distinct_state_and_still_checks_the_token() {
        let api = TestServer::start(|request| match request.path.as_str() {
            "/api/v1/health" => {
                TestResponse::json(200, r#"{"status":"starting","version":"0.3.5"}"#)
            }
            "/api/v1/capabilities" if request.header("authorization") == Some("Bearer tok") => {
                crate::test_server::marker_capabilities(serde_json::json!({}))
            }
            _ if request.header("authorization") != Some("Bearer tok") => TestResponse::json(
                401,
                r#"{"error_code":"UNAUTHORIZED","message":"bad token"}"#,
            ),
            _ => TestResponse::json(
                503,
                r#"{"error_code":"UNAVAILABLE","message":"Library is opening"}"#,
            )
            .with_header("Retry-After", "5"),
        });
        let settings = DataSourceSettings {
            mode: DataSourceMode::Server,
            server_url: api.url.clone(),
            ..Default::default()
        };
        let secrets = RemoteSecrets {
            token: "tok".into(),
            webdav_password: String::new(),
            ..RemoteSecrets::default()
        };
        let report = test_connection(&settings, &secrets, &[], None, false);
        assert!(report.server_starting() && !report.ok());
        // DNS/TCP passes; plain http has only a TLS note.
        assert!(report.steps[0].ok && report.steps[0].step == ProbeStep::Connect);
        assert!(report.steps[1].skipped && report.steps[1].optional);
        assert!(report.tls.is_none());
        assert!(
            report.steps[2].message.contains("starting"),
            "{:#?}",
            report.steps
        );
        assert!(!report.steps[3].ok && report.steps[3].message.contains("starting"));
        assert!(!report.steps[3].message.contains("401"));
        assert!(
            report
                .summary(false)
                .lines()
                .any(|line| line.starts_with("… Server health")),
            "{}",
            report.summary(false)
        );
        let wrong = RemoteSecrets {
            token: "wrong".into(),
            webdav_password: String::new(),
            ..RemoteSecrets::default()
        };
        let report = test_connection(&settings, &wrong, &[], None, true);
        assert!(
            report.steps[3].message.contains("401"),
            "{:#?}",
            report.steps
        );
        // The client keeps the server's Retry-After for polling backoff.
        let client = RemoteClient::new(&api.url, "tok").unwrap();
        let error = client.library().unwrap_err();
        assert_eq!(error.retry_after(), Some(5));
        assert!(matches!(error, RemoteError::Starting { .. }));
    }

    #[test]
    fn report_covers_every_step_with_sources_and_latency() {
        let dav = TestServer::start(|request| {
            let auth = format!("Basic {}", base64(b"me:pw"));
            match (request.method.as_str(), request.path.as_str()) {
                (_, _) if request.header("authorization") != Some(auth.as_str()) => {
                    TestResponse::json(401, "")
                }
                ("PROPFIND", "/dav/SFX/") if request.header("depth") == Some("0") => {
                    TestResponse::json(207, "<multistatus/>")
                }
                ("PROPFIND", "/plain/Foley/") => TestResponse::json(405, ""),
                ("HEAD", "/plain/Foley/") => TestResponse::json(200, ""),
                _ => TestResponse::json(404, ""),
            }
        });
        let dav_url = dav.url.clone();
        let api = TestServer::start(move |request| {
            match request.path.as_str() {
            "/api/v1/health" => TestResponse::json(
                200,
                &serde_json::json!({"status":"ok","version":env!("CARGO_PKG_VERSION"),"public_url":"https://AudioLib.Example.com:8443/"}).to_string(),
            ),
            "/api/v1/capabilities" if request.header("authorization") == Some("Bearer tok") => {
                crate::test_server::marker_capabilities(serde_json::json!({}))
            }
            _ if request.header("authorization") != Some("Bearer tok") => TestResponse::json(
                401,
                r#"{"error_code":"UNAUTHORIZED","message":"bad token"}"#,
            ),
            "/api/v1/library" => TestResponse::json(
                200,
                r#"{"library_id":"L","schema_version":3,"revision":4,"stats":{"audio_files":2}}"#,
            ),
            "/api/v1/roots" => TestResponse::json(
                200,
                &format!(
                    r#"[{{"name":"SFX","server_path":"/V/SFX","webdav_url":"{dav_url}/dav/SFX"}},{{"name":"Foley","server_path":"/V/Foley","webdav_url":null}},{{"name":"Music","server_path":"/V/Music","webdav_url":null}}]"#
                ),
            ),
            _ => TestResponse::json(404, "{}"),
        }
        });
        let mut settings = DataSourceSettings {
            mode: DataSourceMode::Server,
            server_url: api.url.clone(),
            ..Default::default()
        };
        settings.webdav.username = "me".into();
        settings
            .webdav
            .root_urls
            .insert("Foley".into(), format!("{}/plain/Foley", dav.url));
        let secrets = RemoteSecrets {
            token: "tok".into(),
            webdav_password: "pw".into(),
            ..RemoteSecrets::default()
        };
        let report = test_connection(&settings, &secrets, &[], None, false);
        assert_eq!(report.steps.len(), 10, "{:#?}", report.steps);
        assert_eq!(report.server_url.as_deref(), Some(api.url.as_str()));
        assert!(report.steps[0].ok, "{:#?}", report.steps);
        assert!(
            report.steps[2..5].iter().all(|step| step.ok),
            "{:#?}",
            report.steps
        );
        // No synced snapshot to sample: the stream and preview steps are informational skips.
        assert_eq!(report.steps[5].step, ProbeStep::Stream);
        assert!(report.steps[5].skipped && report.steps[5].optional);
        assert_eq!(report.steps[6].step, ProbeStep::Preview);
        assert!(report.steps[6].skipped && report.steps[6].optional);
        assert!(report.steps[7].ok && report.steps[8].ok);
        assert_eq!(report.steps[7].url_source, Some(UrlSource::Server));
        assert_eq!(report.steps[8].url_source, Some(UrlSource::User));
        // A root without a WebDAV URL is skipped, not failed: playback uses the server stream.
        let music = &report.steps[9];
        assert!(music.skipped && music.optional && music.url.is_none());
        assert_eq!(music.url_source, Some(UrlSource::Missing));
        assert!(music.message.contains("server stream"), "{music:#?}");
        assert!(!report.unauthorized);
        assert!(report.ok(), "{}", report.summary(false));
        assert!(report.warnings(false).is_empty());
        let summary = report.summary(true);
        assert!(summary.contains("√ 服务器在线"), "{summary}");
        // §6.2 `public_url`: shown (normalised) when testing another address.
        assert_eq!(
            report.public_url.as_deref(),
            Some("https://AudioLib.Example.com:8443")
        );
        assert_eq!(
            report.other_public_url(),
            Some("https://AudioLib.Example.com:8443")
        );
        assert!(
            summary.contains("server external address: https://AudioLib.Example.com:8443"),
            "{summary}"
        );
        let mut same = report.clone();
        same.server_url = Some("https://audiolib.example.com:8443/api/v1".into());
        assert_eq!(same.other_public_url(), None);
        assert!(
            summary.contains("– WebDAV Music · no WebDAV URL configured"),
            "{summary}"
        );
        assert!(
            test_connection(&settings, &secrets, &[], None, true)
                .summary(true)
                .contains("– WebDAV Music · 未配置 WebDAV，试听走服务器流")
        );
        assert!(!summary.contains("tok") && !summary.contains("pw"));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["steps"][9]["skipped"], true);
        assert_eq!(json["steps"][9]["optional"], true);
        assert_eq!(json["steps"][0]["step"], "connect");

        let bad = RemoteSecrets {
            token: "nope".into(),
            webdav_password: "pw".into(),
            ..RemoteSecrets::default()
        };
        let cached = vec![RemoteRoot {
            name: "SFX".into(),
            server_path: "/V/SFX".into(),
            webdav_url: Some(format!("{}/dav/SFX", dav.url)),
            ..Default::default()
        }];
        let report = test_connection(&settings, &bad, &cached, None, true);
        assert!(report.unauthorized);
        assert!(report.steps[2].ok && !report.steps[3].ok && report.steps[4].skipped);
        assert!(report.steps[5].skipped && !report.steps[5].optional);
        assert!(report.steps[3].message.contains("401"));
        assert!(report.steps[7].ok, "cached roots still verify WebDAV");
        assert!(!report.ok());
    }

    #[test]
    fn connection_test_works_without_a_token_and_hints_when_one_is_required() {
        let open = TestServer::start(|request| {
            if request.header("authorization").is_some() {
                return TestResponse::json(400, r#"{"message":"unexpected Authorization"}"#);
            }
            match request.path.as_str() {
                "/api/v1/health" => crate::test_server::marker_health(),
                "/api/v1/capabilities" => {
                    crate::test_server::marker_capabilities(serde_json::json!({}))
                }
                "/api/v1/library" => TestResponse::json(
                    200,
                    r#"{"library_id":"open","schema_version":3,"revision":1}"#,
                ),
                "/api/v1/roots" => TestResponse::json(
                    200,
                    r#"[{"name":"TestSFX","server_path":"/V/TestSFX","webdav_url":null}]"#,
                ),
                _ => TestResponse::json(404, "{}"),
            }
        });
        let settings = |url: &str| DataSourceSettings {
            mode: DataSourceMode::Server,
            server_url: url.to_owned(),
            ..Default::default()
        };
        let no_token = RemoteSecrets::default();
        let report = test_connection(&settings(&open.url), &no_token, &[], None, true);
        assert!(report.ok(), "{}", report.summary(true));
        assert!(report.steps[3].ok && report.steps[4].ok);

        let secured = TestServer::start(|request| match request.path.as_str() {
            "/api/v1/health" => crate::test_server::marker_health(),
            "/api/v1/capabilities" => {
                crate::test_server::marker_capabilities(serde_json::json!({}))
            }
            _ => TestResponse::json(
                401,
                r#"{"error_code":"UNAUTHORIZED","message":"missing token"}"#,
            ),
        });
        let report = test_connection(&settings(&secured.url), &no_token, &[], None, true);
        assert!(report.unauthorized);
        assert!(!report.ok());
        assert!(
            report.steps[3].message.contains("服务器要求访问令牌"),
            "{:#?}",
            report.steps
        );
    }

    /// A synced snapshot of library `L`: record 1 is marked missing, 5 and 9 are present.
    fn synced_cache(directory: &std::path::Path) -> RemoteCache {
        let cache = RemoteCache::new(directory);
        let state = crate::sync::CacheState {
            library_id: "L".into(),
            revision: 4,
            schema_version: 3,
            format: None,
            database_file: "library-r4.db".into(),
            server_url: String::new(),
            server_version: String::new(),
            downloaded_at: 0,
            checked_at: 0,
            snapshot_bytes: 0,
            audio_files: 2,
            roots: Vec::new(),
        };
        let database = cache.database_path(&state);
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        drop(audiolib_db::SqliteProvider::open(&database).unwrap());
        let connection = rusqlite::Connection::open(&database).unwrap();
        audiolib_db::register_library_functions(&connection).unwrap();
        connection
            .execute_batch(
                "INSERT INTO audio_files(id,file_path,filename,missing) VALUES
                 (1,'/V/SFX/gone.wav','gone.wav',1),
                 (5,'/V/SFX/a.wav','a.wav',0),
                 (9,'/V/SFX/b.wav','b.wav',0);",
            )
            .unwrap();
        drop(connection);
        cache.save_state(&state).unwrap();
        cache
    }

    #[test]
    fn stream_step_samples_a_synced_record_and_webdav_failures_are_warnings() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        // A configured WebDAV URL that is broken.
        let dav = TestServer::start(|_| TestResponse::json(500, ""));
        let dead_dav = format!("{}/dav/SFX", dav.url);
        // 0: Range honoured, 1: file missing, 2: Range ignored, 3: token rejected,
        // 4: root offline.
        let mode = Arc::new(AtomicUsize::new(0));
        let api = {
            let mode = mode.clone();
            TestServer::start(move |request| {
                match request.path.as_str() {
                "/api/v1/health" => crate::test_server::marker_health(),
                _ if request.header("authorization") != Some("Bearer tok") => {
                    TestResponse::json(401, r#"{"error_code":"UNAUTHORIZED"}"#)
                }
                "/api/v1/capabilities" => crate::test_server::marker_capabilities(serde_json::json!({})),
                "/api/v1/library" => {
                    TestResponse::json(200, r#"{"library_id":"L","schema_version":3,"revision":4}"#)
                }
                "/api/v1/roots" => TestResponse::json(
                    200,
                    &format!(
                        r#"[{{"name":"SFX","server_path":"/V/SFX","webdav_url":"{dead_dav}"}},{{"name":"Music","server_path":"/V/Music","webdav_url":null}}]"#
                    ),
                ),
                preview if preview.starts_with("/api/v1/audio/5/preview?codec=pcm&sr=48000&bits=16") => {
                    assert_eq!(request.header("range"), Some("bytes=0-524287"));
                    TestResponse::bytes(206, vec![0; 8192])
                        .with_header("Content-Range", "bytes 0-8191/8192")
                        .with_header("X-Preview-Sample-Rate", "48000")
                        .with_header("X-Preview-Bits", "16")
                        .with_header("X-Preview-Channels", "2")
                }
                "/api/v1/audio/5/stream" if request.header("range") == Some("bytes=0-0") => {
                    match mode.load(Ordering::SeqCst) {
                        0 => TestResponse::bytes(206, vec![b'R'])
                            .with_header("Content-Range", "bytes 0-0/1234")
                            .with_header("ETag", "\"4d2-1\""),
                        1 => TestResponse::json(
                            404,
                            r#"{"error_code":"NOT_FOUND","message":"file missing"}"#,
                        ),
                        2 => TestResponse::bytes(200, vec![0; 64]),
                        4 => TestResponse::json(
                            503,
                            r#"{"error_code":"ROOT_OFFLINE","message":"Library root \"SFX\" is offline"}"#,
                        )
                        .with_header("Retry-After", "5"),
                        _ => TestResponse::json(401, r#"{"error_code":"UNAUTHORIZED"}"#),
                    }
                }
                _ => TestResponse::json(400, "{}"),
            }
            })
        };
        let directory = tempfile::tempdir().unwrap();
        let cache = synced_cache(directory.path());
        assert_eq!(cache.sample_audio_id("L"), Some(5), "skips missing records");
        assert_eq!(cache.sample_audio_id("other"), None);
        let settings = DataSourceSettings {
            mode: DataSourceMode::Server,
            server_url: api.url.clone(),
            ..Default::default()
        };
        let secrets = RemoteSecrets {
            token: "tok".into(),
            webdav_password: String::new(),
            ..RemoteSecrets::default()
        };
        let run = |chinese| test_connection(&settings, &secrets, &[], Some(&cache), chinese);

        let report = run(true);
        let stream = &report.steps[5];
        assert!(
            stream.ok && !stream.optional && stream.target == "#5",
            "{stream:#?}"
        );
        assert!(stream.message.contains("206") && stream.message.contains("1234"));
        let preview = &report.steps[6];
        assert!(
            preview.ok && preview.optional && preview.target == "#5",
            "{preview:#?}"
        );
        assert!(
            preview.message.contains("8192") && preview.message.contains("Mbps"),
            "{preview:#?}"
        );
        assert!(preview.message.contains("48000 Hz/16-bit/2 ch"));
        let sfx = &report.steps[7];
        assert!(!sfx.ok && !sfx.skipped && sfx.optional, "{sfx:#?}");
        assert!(sfx.message.contains("可选"));
        assert!(report.steps[8].skipped && report.steps[8].optional);
        assert!(report.ok(), "{}", report.summary(true));
        let warnings = report.warnings(true);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("WebDAV SFX"));
        let summary = report.summary(true);
        assert!(summary.contains("√ 服务器音频流 #5"), "{summary}");
        assert!(summary.contains("√ 试听预览吞吐 #5"), "{summary}");
        assert!(summary.contains("√ DNS / TCP 连接"), "{summary}");
        assert!(summary.contains("! WebDAV SFX"), "{summary}");
        assert!(summary.contains("– WebDAV Music"), "{summary}");

        mode.store(1, Ordering::SeqCst);
        let report = run(false);
        assert!(!report.steps[5].ok && report.steps[5].optional);
        assert!(report.steps[5].message.contains("missing on the server"));
        // Nothing to sample for the preview: a note, not a warning.
        assert!(report.steps[6].skipped && report.steps[6].optional);
        assert!(report.ok(), "a missing sampled file is a warning");
        assert_eq!(report.warnings(false).len(), 2);

        mode.store(2, Ordering::SeqCst);
        let report = run(false);
        assert!(report.steps[5].ok && report.steps[5].message.contains("ignored Range"));

        // §11: an offline root (503) fails the step, it is not a warning.
        mode.store(4, Ordering::SeqCst);
        let report = run(false);
        assert!(
            !report.steps[5].ok && !report.steps[5].optional,
            "{:#?}",
            report.steps[5]
        );
        assert!(!report.ok(), "a 503 stream fails the test");

        mode.store(3, Ordering::SeqCst);
        let report = run(false);
        assert!(!report.steps[5].ok && !report.steps[5].optional);
        assert!(!report.ok(), "a rejected stream fails the test");

        // A snapshot of another library is not sampled.
        let empty = tempfile::tempdir().unwrap();
        let other = RemoteCache::new(empty.path());
        let report = test_connection(&settings, &secrets, &[], Some(&other), false);
        assert!(report.steps[5].skipped && report.steps[5].optional);
        mode.store(0, Ordering::SeqCst);
        assert!(report.ok());
    }

    #[test]
    fn https_connection_test_reports_the_tls_handshake() {
        let (port, _) = crate::tls::tests::trusted_server();
        let settings = DataSourceSettings {
            mode: DataSourceMode::Server,
            // A path prefix and a pasted /api/v1 normalise to the same API base.
            server_url: format!("https://localhost:{port}/api/v1/"),
            ..Default::default()
        };
        let secrets = RemoteSecrets {
            token: "tok".into(),
            webdav_password: String::new(),
            ..RemoteSecrets::default()
        };
        let report = test_connection(&settings, &secrets, &[], None, false);
        assert_eq!(
            report.server_url.as_deref(),
            Some(format!("https://localhost:{port}").as_str())
        );
        assert!(report.steps[0].ok, "{:#?}", report.steps);
        let tls = &report.steps[1];
        assert!(tls.ok && tls.step == ProbeStep::Tls, "{tls:#?}");
        assert!(tls.message.contains("TLSv1.3") && tls.message.contains("AudioLib Test CA"));
        assert_eq!(
            report.tls.as_ref().map(|info| info.subject.as_deref()),
            Some(Some("localhost"))
        );
        assert!(report.steps[2].ok && report.steps[3].ok && report.steps[4].ok);
        assert!(report.ok(), "{}", report.summary(false));
        assert!(!report.summary(true).contains("Bearer"));
    }

    #[test]
    fn a_web_server_without_the_api_is_named_as_such() {
        let site = TestServer::start(|_| TestResponse::json(404, "<html>\n</html>"));
        let settings = DataSourceSettings {
            mode: DataSourceMode::Server,
            server_url: site.url.clone(),
            ..Default::default()
        };
        let report = test_connection(&settings, &RemoteSecrets::default(), &[], None, false);
        let health = &report.steps[2];
        assert_eq!(health.step, ProbeStep::Health);
        assert!(
            !health.ok && health.message.contains("path prefix"),
            "{health:#?}"
        );
        assert!(!health.message.contains("<html>"));
        assert!(!report.ok());
    }

    #[test]
    fn unreachable_servers_stop_after_the_network_step() {
        let settings = |url: &str| DataSourceSettings {
            mode: DataSourceMode::Server,
            server_url: url.to_owned(),
            ..Default::default()
        };
        let report = test_connection(
            &settings("https://audiolib-test.invalid:8443"),
            &RemoteSecrets::default(),
            &[],
            None,
            false,
        );
        assert!(!report.ok());
        assert_eq!(report.steps.len(), 7, "{:#?}", report.steps);
        assert!(report.steps[2..].iter().all(|step| step.skipped));
        // A fake-IP DNS proxy (Clash/TUN style) resolves every name and accepts the TCP
        // connection, so then it is the TLS step that fails; otherwise DNS does.
        if std::net::ToSocketAddrs::to_socket_addrs(&("audiolib-test.invalid", 443)).is_err() {
            assert!(!report.steps[0].ok, "{:#?}", report.steps);
            assert!(
                report.steps[0].message.contains("DNS"),
                "{:#?}",
                report.steps
            );
            assert!(report.steps[1].skipped);
        } else {
            assert!(!report.steps[0].ok || !report.steps[1].ok);
        }
        // An invalid address never reaches the network.
        let report = test_connection(
            &settings("https://host:99999"),
            &RemoteSecrets::default(),
            &[],
            None,
            true,
        );
        assert_eq!(report.steps.len(), 1);
        assert!(
            report.steps[0].message.contains("端口"),
            "{:#?}",
            report.steps
        );
    }
}
