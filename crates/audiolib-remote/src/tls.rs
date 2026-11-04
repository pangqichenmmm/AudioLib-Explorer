// crates/audiolib-remote/src/tls.rs
//! TLS policy shared by every server-mode connection: API requests, the snapshot,
//! `/audio/{id}/stream`, `/preview`, `/waveform`, drag-out downloads and WebDAV.
//!
//! * Certificates are verified by `rustls-platform-verifier` against the operating system
//!   trust store (on Windows the certificate store, so enterprise/GPO-deployed roots and
//!   Windows' on-demand root updates apply; on macOS the keychain). Public CAs such as
//!   Let's Encrypt therefore work out of the box.
//! * There is deliberately no switch to accept self-signed or otherwise untrusted
//!   certificates, and verification is never disabled.
//! * Failures are classified into [`TlsFailure`] for readable messages.
//! * Unit tests swap in a throwaway CA ([`set_test_roots`], `cfg(test)` only); release
//!   builds have no way to add roots.
use crate::error::{RemoteError, TlsFailure};
use serde::Serialize;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Human-readable name of the trust store used for verification.
pub const TRUST_STORE: &str = if cfg!(windows) {
    "Windows certificate store (rustls-platform-verifier)"
} else if cfg!(target_vendor = "apple") {
    "macOS keychain (rustls-platform-verifier)"
} else {
    "OS trust store (rustls-platform-verifier)"
};

#[cfg(test)]
static TEST_ROOTS: std::sync::OnceLock<Vec<rustls::pki_types::CertificateDer<'static>>> =
    std::sync::OnceLock::new();

/// Trust only `roots` in this test process (the first call wins).
#[cfg(test)]
pub(crate) fn set_test_roots(roots: Vec<rustls::pki_types::CertificateDer<'static>>) {
    let _ = TEST_ROOTS.set(roots);
}

/// TLS settings for every ureq agent of this crate (see [`crate::abort::shared_agent`]).
pub(crate) fn ureq_tls_config() -> ureq::tls::TlsConfig {
    let builder = ureq::tls::TlsConfig::builder().provider(ureq::tls::TlsProvider::Rustls);
    #[cfg(test)]
    if let Some(roots) = TEST_ROOTS.get() {
        let roots: Vec<_> = roots
            .iter()
            .map(|der| ureq::tls::Certificate::from_der(der.as_ref()).to_owned())
            .collect();
        return builder
            .root_certs(ureq::tls::RootCerts::new_with_certs(&roots))
            .build();
    }
    builder
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build()
}

fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    // The same choice ureq makes: a process default if one is installed, else ring.
    rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()))
}

/// rustls client configuration equivalent to [`ureq_tls_config`], for the connection
/// test's separate handshake step.
fn client_config() -> std::result::Result<Arc<rustls::ClientConfig>, rustls::Error> {
    let provider = crypto_provider();
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?;
    #[cfg(test)]
    if let Some(roots) = TEST_ROOTS.get() {
        let mut store = rustls::RootCertStore::empty();
        for root in roots {
            store.add(root.clone())?;
        }
        return Ok(Arc::new(
            builder.with_root_certificates(store).with_no_client_auth(),
        ));
    }
    let verifier = rustls_platform_verifier::Verifier::new(provider)?;
    Ok(Arc::new(
        builder
            .dangerous() // a custom verifier, not a weaker one: the OS verifies the chain
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth(),
    ))
}

/// The rustls error carried inside an I/O error (how rustls reports handshake failures
/// through `complete_io`, and so through ureq).
pub(crate) fn rustls_error_in(error: &std::io::Error) -> Option<&rustls::Error> {
    error.get_ref()?.downcast_ref::<rustls::Error>()
}

/// Map a rustls error to a [`TlsFailure`].
pub(crate) fn classify(error: &rustls::Error) -> TlsFailure {
    use rustls::CertificateError as Cert;
    match error {
        rustls::Error::InvalidCertificate(certificate) => match certificate {
            Cert::NotValidForName | Cert::NotValidForNameContext { .. } => {
                TlsFailure::HostnameMismatch
            }
            Cert::Expired | Cert::ExpiredContext { .. } => TlsFailure::Expired,
            Cert::NotValidYet | Cert::NotValidYetContext { .. } => TlsFailure::NotYetValid,
            Cert::Revoked => TlsFailure::Revoked,
            // UnknownIssuer, BadSignature, InvalidPurpose, ApplicationVerificationFailure,
            // a platform-specific `Other`, … : the chain is not trusted.
            _ => TlsFailure::Untrusted,
        },
        rustls::Error::NoCertificatesPresented => TlsFailure::Untrusted,
        _ => TlsFailure::Handshake,
    }
}

/// [`RemoteError::Tls`] for a rustls error on `url`.
pub(crate) fn tls_error(error: &rustls::Error, url: &str) -> RemoteError {
    RemoteError::Tls {
        kind: classify(error),
        detail: format!("{url}: {error}"),
    }
}

/// True when an I/O error is a failed host name lookup (Windows WSA codes, or the
/// getaddrinfo message on other systems).
pub(crate) fn is_dns_failure(error: &std::io::Error) -> bool {
    // WSAHOST_NOT_FOUND, WSATRY_AGAIN, WSANO_RECOVERY, WSANO_DATA.
    if cfg!(windows) && matches!(error.raw_os_error(), Some(11001..=11004)) {
        return true;
    }
    let text = error.to_string().to_ascii_lowercase();
    text.contains("failed to lookup address")
        || text.contains("name or service not known")
        || text.contains("no such host")
        || text.contains("nodename nor servname")
}

/// What the connection test learned about a TLS session.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct TlsInfo {
    /// e.g. `TLSv1.3`.
    pub protocol: String,
    /// e.g. `TLS13_AES_256_GCM_SHA384`.
    pub cipher_suite: String,
    /// Name the certificate was verified for (SNI).
    pub server_name: String,
    /// Leaf certificate subject CN (or O).
    pub subject: Option<String>,
    /// Leaf certificate issuer CN/O.
    pub issuer: Option<String>,
    /// Leaf certificate expiry, `YYYY-MM-DD HH:MM:SS UTC`.
    pub not_after: Option<String>,
    /// Certificates the server sent (leaf + intermediates).
    pub chain_length: usize,
    /// Where the chain was verified ([`TRUST_STORE`]).
    pub trust_store: String,
}

/// Resolve `host` and open a TCP connection to the first address that answers. Returns
/// the resolved addresses, the connected socket, DNS time and connect time. Failures use
/// the same classification as API requests (DNS, refused, timeout).
pub(crate) fn connect_tcp(
    host: &str,
    port: u16,
    timeout: Duration,
) -> crate::Result<(Vec<SocketAddr>, TcpStream, Duration, Duration)> {
    let label = format!("{host}:{port}");
    let started = Instant::now();
    let addresses: Vec<SocketAddr> = match (host, port).to_socket_addrs() {
        Ok(addresses) => addresses.collect(),
        Err(error) => {
            return Err(RemoteError::Unreachable(format!(
                "{label}: {}: {error}",
                crate::error::DNS_FAILED
            )));
        }
    };
    let dns = started.elapsed();
    if addresses.is_empty() {
        return Err(RemoteError::Unreachable(format!(
            "{label}: {}: no addresses",
            crate::error::DNS_FAILED
        )));
    }
    let started = Instant::now();
    let mut last = None;
    for address in &addresses {
        match TcpStream::connect_timeout(address, timeout) {
            Ok(stream) => return Ok((addresses.clone(), stream, dns, started.elapsed())),
            Err(error) => last = Some(error),
        }
    }
    let error = last.unwrap_or_else(|| std::io::Error::other("no address to connect"));
    Err(RemoteError::from_io(&error, &label))
}

/// Run a TLS handshake for `server_name` on `stream` with the same verification as all
/// requests, then close the session. Used by the connection test and the CLI.
pub(crate) fn handshake(
    mut stream: TcpStream,
    server_name: &str,
    timeout: Duration,
) -> crate::Result<TlsInfo> {
    let label = format!("https://{server_name}");
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let name = rustls::pki_types::ServerName::try_from(server_name.to_owned()).map_err(|_| {
        RemoteError::Tls {
            kind: TlsFailure::HostnameMismatch,
            detail: format!("{label}: not a valid TLS server name"),
        }
    })?;
    let config = client_config().map_err(|error| tls_error(&error, &label))?;
    let mut connection =
        rustls::ClientConnection::new(config, name).map_err(|error| tls_error(&error, &label))?;
    while connection.is_handshaking() {
        match connection.complete_io(&mut stream) {
            Ok((0, 0)) => {
                return Err(RemoteError::Tls {
                    kind: TlsFailure::Handshake,
                    detail: format!(
                        "{label}: the server closed the connection during the handshake"
                    ),
                });
            }
            Ok(_) => {}
            Err(error) => return Err(handshake_io_error(&error, &label)),
        }
    }
    let certificates = connection.peer_certificates().unwrap_or_default();
    let leaf = certificates
        .first()
        .map(|der| summarize_certificate(der.as_ref()));
    let info = TlsInfo {
        protocol: connection
            .protocol_version()
            .map(|version| match version {
                rustls::ProtocolVersion::TLSv1_3 => "TLSv1.3".to_owned(),
                rustls::ProtocolVersion::TLSv1_2 => "TLSv1.2".to_owned(),
                other => format!("{other:?}"),
            })
            .unwrap_or_default(),
        cipher_suite: connection
            .negotiated_cipher_suite()
            .map(|suite| format!("{:?}", suite.suite()))
            .unwrap_or_default(),
        server_name: server_name.to_owned(),
        subject: leaf.as_ref().and_then(|leaf| leaf.subject.clone()),
        issuer: leaf.as_ref().and_then(|leaf| leaf.issuer.clone()),
        not_after: leaf.and_then(|leaf| leaf.not_after),
        chain_length: certificates.len(),
        trust_store: TRUST_STORE.to_owned(),
    };
    connection.send_close_notify();
    let _ = connection.complete_io(&mut stream);
    let _ = stream.flush();
    // Drain a little so the close is orderly; errors here do not matter.
    let mut sink = [0u8; 256];
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let _ = stream.read(&mut sink);
    Ok(info)
}

fn handshake_io_error(error: &std::io::Error, label: &str) -> RemoteError {
    if let Some(tls) = rustls_error_in(error) {
        return tls_error(tls, label);
    }
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => RemoteError::Tls {
            kind: TlsFailure::Handshake,
            detail: format!("{label}: no TLS answer within the timeout"),
        },
        _ => RemoteError::Tls {
            kind: TlsFailure::Handshake,
            detail: format!("{label}: {error}"),
        },
    }
}

/// Subject, issuer and expiry of a DER certificate (best effort; `None` fields when the
/// structure is unexpected).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CertificateSummary {
    pub subject: Option<String>,
    pub issuer: Option<String>,
    pub not_after: Option<String>,
}

pub(crate) fn summarize_certificate(der: &[u8]) -> CertificateSummary {
    (|| {
        let (certificate, _) = der::element(der, 0x30)?;
        let (tbs, _) = der::element(certificate, 0x30)?;
        let mut rest = tbs;
        if rest.first() == Some(&0xA0) {
            rest = der::skip(rest)?; // [0] version
        }
        rest = der::skip(rest)?; // serialNumber
        rest = der::skip(rest)?; // signature algorithm
        let (issuer, after) = der::element(rest, 0x30)?;
        let (validity, after) = der::element(after, 0x30)?;
        let (subject, _) = der::element(after, 0x30)?;
        let not_after = der::skip(validity).and_then(der::time);
        Some(CertificateSummary {
            subject: der::name(subject),
            issuer: der::name(issuer),
            not_after,
        })
    })()
    .unwrap_or_default()
}

/// Just enough DER to read X.509 names and validity.
mod der {
    /// Content of the element at the start of `input` (which must have `tag`) and the
    /// bytes after it.
    pub fn element(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
        let (found, content, rest) = any(input)?;
        (found == tag).then_some((content, rest))
    }

    fn any(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let tag = *input.first()?;
        let first = *input.get(1)? as usize;
        let (length, header) = if first < 0x80 {
            (first, 2)
        } else {
            let count = first & 0x7F;
            if count == 0 || count > 4 {
                return None;
            }
            let mut length = 0usize;
            for index in 0..count {
                length = (length << 8) | *input.get(2 + index)? as usize;
            }
            (length, 2 + count)
        };
        let end = header.checked_add(length)?;
        Some((tag, input.get(header..end)?, input.get(end..)?))
    }

    pub fn skip(input: &[u8]) -> Option<&[u8]> {
        any(input).map(|(_, _, rest)| rest)
    }

    /// `UTCTime` / `GeneralizedTime` at the start of `input` as `YYYY-MM-DD HH:MM:SS UTC`.
    pub fn time(input: &[u8]) -> Option<String> {
        let (tag, content, _) = any(input)?;
        let text = std::str::from_utf8(content).ok()?;
        let digits = text.strip_suffix('Z')?;
        let full = match (tag, digits.len()) {
            (0x17, 12) => {
                let year: u32 = digits[..2].parse().ok()?;
                format!("{}{digits}", if year >= 50 { "19" } else { "20" })
            }
            (0x18, 14) => digits.to_owned(),
            _ => return None,
        };
        if !full.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Some(format!(
            "{}-{}-{} {}:{}:{} UTC",
            &full[..4],
            &full[4..6],
            &full[6..8],
            &full[8..10],
            &full[10..12],
            &full[12..14]
        ))
    }

    /// `CN`, else `O`, of an X.501 Name (SEQUENCE OF SET OF AttributeTypeAndValue).
    pub fn name(mut input: &[u8]) -> Option<String> {
        const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
        const ORGANIZATION: &[u8] = &[0x55, 0x04, 0x0A];
        let (mut common, mut organization) = (None, None);
        while !input.is_empty() {
            let (set, rest) = element(input, 0x31)?;
            input = rest;
            let mut attributes = set;
            while !attributes.is_empty() {
                let (attribute, rest) = element(attributes, 0x30)?;
                attributes = rest;
                let (oid, value) = element(attribute, 0x06)?;
                let (_, text, _) = any(value)?;
                let text = String::from_utf8_lossy(text).into_owned();
                if oid == COMMON_NAME {
                    common = Some(text);
                } else if oid == ORGANIZATION {
                    organization = Some(text);
                }
            }
        }
        common.or(organization)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::client::RemoteClient;
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose,
        IsCa, KeyPair, KeyUsagePurpose,
    };
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::sync::{Mutex, OnceLock};

    /// The test CA (trusted by this process through [`set_test_roots`]) and its issuer.
    struct TestCa {
        issuer: CertifiedIssuer<'static, KeyPair>,
    }

    fn test_ca() -> &'static TestCa {
        static CA: OnceLock<TestCa> = OnceLock::new();
        CA.get_or_init(|| {
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params
                .distinguished_name
                .push(DnType::CommonName, "AudioLib Test CA");
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params.key_usages = vec![
                KeyUsagePurpose::KeyCertSign,
                KeyUsagePurpose::CrlSign,
                KeyUsagePurpose::DigitalSignature,
            ];
            let issuer =
                CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
            set_test_roots(vec![issuer.der().clone()]);
            TestCa { issuer }
        })
    }

    /// A leaf certificate for `names`, signed by the test CA (or self-signed).
    fn leaf(
        names: &[&str],
        expired: bool,
        self_signed: bool,
    ) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let ca = test_ca();
        let mut params = CertificateParams::new(
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        params.distinguished_name.push(DnType::CommonName, names[0]);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        if expired {
            params.not_before = rcgen::date_time_ymd(2020, 1, 1);
            params.not_after = rcgen::date_time_ymd(2021, 1, 1);
        }
        let key = KeyPair::generate().unwrap();
        let certificate = if self_signed {
            params.self_signed(&key).unwrap()
        } else {
            params.signed_by(&key, &*ca.issuer).unwrap()
        };
        (
            vec![certificate.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
    }

    /// Requests seen by a [`tls_server`]: `(path, Authorization header)`.
    pub(crate) type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

    /// HTTPS test server on 127.0.0.1 for the AudioLib API: `/health` is open, everything
    /// else needs `Bearer tok` (`401 UNAUTHORIZED` with the §2.2 messages for a missing
    /// or wrong token, like the server behind its reverse proxy).
    fn tls_server(chain: Vec<CertificateDer<'static>>, key: PrivateKeyDer<'static>) -> (u16, Seen) {
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Seen = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for socket in listener.incoming().flatten() {
                let config = config.clone();
                let log = log.clone();
                std::thread::spawn(move || {
                    let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));
                    let connection = rustls::ServerConnection::new(config).unwrap();
                    let mut stream = rustls::StreamOwned::new(connection, socket);
                    let mut reader = std::io::BufReader::new(&mut stream);
                    let mut head = Vec::new();
                    loop {
                        let mut line = String::new();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => return, // e.g. the client rejected our certificate
                            Ok(_) if line == "\r\n" => break,
                            Ok(_) => head.push(line.trim_end().to_owned()),
                        }
                    }
                    drop(reader);
                    let path = head
                        .first()
                        .and_then(|line| line.split(' ').nth(1))
                        .unwrap_or_default()
                        .to_owned();
                    let authorization = head.iter().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("authorization")
                            .then(|| value.trim().to_owned())
                    });
                    log.lock()
                        .unwrap()
                        .push((path.clone(), authorization.clone()));
                    let (status, extra, body) = match (path.as_str(), authorization.as_deref()) {
                        ("/api/v1/health", _) => {
                            (200, "", serde_json::json!({"status":"ok","version":env!("CARGO_PKG_VERSION")}).to_string())
                        }
                        (_, None) => (
                            401,
                            "WWW-Authenticate: Bearer realm=\"audiolib\"\r\n",
                            r#"{"error_code":"UNAUTHORIZED","message":"A bearer token is required: this request is not from a trusted direct LAN peer"}"#
                                .to_owned(),
                        ),
                        (_, Some(value)) if value != "Bearer tok" => (
                            401,
                            "WWW-Authenticate: Bearer realm=\"audiolib\"\r\n",
                            r#"{"error_code":"UNAUTHORIZED","message":"The bearer token is invalid or has been revoked"}"#
                                .to_owned(),
                        ),
                        ("/api/v1/capabilities", _) => (
                            200,
                            "",
                            serde_json::json!({"compatibility": crate::test_server::marker_catalog()})
                                .to_string(),
                        ),
                        ("/api/v1/library", _) => (
                            200,
                            "",
                            r#"{"library_id":"tls-lib","schema_version":3,"revision":2}"#.to_owned(),
                        ),
                        ("/api/v1/roots", _) => (200, "", "[]".to_owned()),
                        (stream, _) if stream.starts_with("/api/v1/audio/") => {
                            (206, "Content-Range: bytes 0-0/10\r\n", "R".to_owned())
                        }
                        _ => (404, "", r#"{"error_code":"NOT_FOUND"}"#.to_owned()),
                    };
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes());
                    stream.conn.send_close_notify();
                    let _ = stream.flush();
                });
            }
        });
        (port, seen)
    }

    pub(crate) fn trusted_server() -> (u16, Seen) {
        let (chain, key) = leaf(&["localhost", "127.0.0.1"], false, false);
        tls_server(chain, key)
    }

    #[test]
    fn https_requests_verify_the_chain_and_carry_the_bearer_token() {
        let (port, seen) = trusted_server();
        for host in ["localhost", "127.0.0.1"] {
            let client = RemoteClient::new(&format!("https://{host}:{port}"), "tok").unwrap();
            assert_eq!(client.health().unwrap().version, env!("CARGO_PKG_VERSION"));
            assert_eq!(client.library().unwrap().library_id, "tls-lib");
            // The stream probe and the abortable agent (playback, downloads) use the same
            // TLS configuration.
            let probe = crate::stream::probe_stream(&client.stream_source(7)).unwrap();
            assert_eq!((probe.status, probe.total), (206, Some(10)));
            let abort = crate::abort::Abort::default();
            let response = crate::abort::abortable_agent(
                Duration::from_secs(5),
                Duration::from_secs(5),
                &abort,
            )
            .get(&format!("https://{host}:{port}/api/v1/audio/7/stream"))
            .header("Authorization", "Bearer tok")
            .call()
            .unwrap();
            assert_eq!(response.status().as_u16(), 206);
        }
        let seen = seen.lock().unwrap();
        let library: Vec<_> = seen
            .iter()
            .filter(|(path, _)| path != "/api/v1/health")
            .collect();
        assert!(!library.is_empty());
        assert!(
            library
                .iter()
                .all(|(_, auth)| auth.as_deref() == Some("Bearer tok")),
            "{library:?}"
        );
    }

    #[test]
    fn missing_and_wrong_tokens_point_to_the_admin_page() {
        let (port, _) = trusted_server();
        let url = format!("https://localhost:{port}");
        let missing = RemoteClient::new(&url, "").unwrap().library().unwrap_err();
        assert!(
            matches!(missing, RemoteError::TokenRequired(_)),
            "{missing:?}"
        );
        for chinese in [true, false] {
            assert!(missing.localized(chinese).contains(":8790/admin"));
        }
        let wrong = RemoteClient::new(&url, "nope")
            .unwrap()
            .library()
            .unwrap_err();
        assert!(matches!(wrong, RemoteError::Unauthorized(_)), "{wrong:?}");
        assert!(wrong.localized(false).contains("revoked"));
        assert!(wrong.localized(true).contains("/admin"));
    }

    #[test]
    fn certificate_problems_are_classified_and_never_bypassed() {
        test_ca();
        let cases = [
            (leaf(&["localhost"], false, true), TlsFailure::Untrusted),
            (leaf(&["localhost"], true, false), TlsFailure::Expired),
            (
                leaf(&["other.example"], false, false),
                TlsFailure::HostnameMismatch,
            ),
        ];
        for ((chain, key), expected) in cases {
            let (port, seen) = tls_server(chain, key);
            let client = RemoteClient::new(&format!("https://localhost:{port}"), "tok").unwrap();
            let error = client.health().unwrap_err();
            assert!(
                matches!(&error, RemoteError::Tls { kind, .. } if *kind == expected),
                "{expected:?}: {error:?}"
            );
            assert!(!error.is_offline() && !error.is_unauthorized());
            // The connection test's own handshake agrees.
            let (_, stream, _, _) = connect_tcp("localhost", port, Duration::from_secs(3)).unwrap();
            let probe = handshake(stream, "localhost", Duration::from_secs(5)).unwrap_err();
            assert!(
                matches!(&probe, RemoteError::Tls { kind, .. } if *kind == expected),
                "{expected:?}: {probe:?}"
            );
            // No request ever got through.
            assert!(seen.lock().unwrap().is_empty());
            if expected == TlsFailure::Untrusted {
                assert!(
                    error
                        .localized(false)
                        .contains("publicly trusted certificate")
                );
                assert!(error.localized(true).contains("公共受信任的证书"));
            }
        }
    }

    #[test]
    fn plain_http_dns_and_refused_failures_are_readable() {
        // An http-only server addressed with https://: like nginx or the AudioLib server it
        // answers the ClientHello with a plain-text 400 and closes.
        let plain = TcpListener::bind("127.0.0.1:0").unwrap();
        let plain_port = plain.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in plain.incoming().take(1) {
                let Ok(mut stream) = stream else { continue };
                let mut buffer = [0_u8; 1024];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        let https = format!("https://127.0.0.1:{plain_port}");
        let error = RemoteClient::new(&https, "").unwrap().health().unwrap_err();
        assert!(
            matches!(
                error,
                RemoteError::Tls {
                    kind: TlsFailure::Handshake,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.localized(false).contains("http://"));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let refused = RemoteClient::new(&format!("https://127.0.0.1:{port}"), "")
            .unwrap()
            .health()
            .unwrap_err();
        assert_eq!(
            refused.network_failure(),
            Some(crate::error::NetworkFailure::Refused),
            "{refused:?}"
        );
        assert!(refused.is_offline());
        assert!(refused.localized(true).contains("拒绝连接"));
        // (Windows retries a refused SYN for about 2 s before giving up.)
        let tcp = connect_tcp("127.0.0.1", port, Duration::from_secs(10)).unwrap_err();
        assert_eq!(
            tcp.network_failure(),
            Some(crate::error::NetworkFailure::Refused),
            "{tcp:?}"
        );

        // `.invalid` never resolves (RFC 6761), unless a fake-IP DNS proxy (Clash/TUN style,
        // 198.18.0.0/15) answers every name: then there is no DNS failure to observe.
        if ("audiolib-test.invalid", 443).to_socket_addrs().is_ok() {
            eprintln!("skipping the DNS checks: this network resolves every name");
            return;
        }
        let dns = RemoteClient::new("https://audiolib-test.invalid:8443", "")
            .unwrap()
            .health()
            .unwrap_err();
        assert!(
            matches!(&dns, RemoteError::Timeout(_))
                || dns.network_failure() == Some(crate::error::NetworkFailure::Dns),
            "{dns:?}"
        );
        let lookup = connect_tcp("audiolib-test.invalid", 443, Duration::from_secs(2)).unwrap_err();
        assert_eq!(
            lookup.network_failure(),
            Some(crate::error::NetworkFailure::Dns),
            "{lookup:?}"
        );
        assert!(lookup.localized(false).contains("DNS"));
        assert!(lookup.localized(true).contains("DNS"));
    }

    #[test]
    fn handshake_reports_protocol_and_certificate_details() {
        let (port, _) = trusted_server();
        let (addresses, stream, _, _) =
            connect_tcp("localhost", port, Duration::from_secs(3)).unwrap();
        assert!(!addresses.is_empty());
        let info = handshake(stream, "localhost", Duration::from_secs(5)).unwrap();
        assert_eq!(info.protocol, "TLSv1.3");
        assert!(info.cipher_suite.starts_with("TLS13_"), "{info:?}");
        assert_eq!(info.subject.as_deref(), Some("localhost"));
        assert_eq!(info.issuer.as_deref(), Some("AudioLib Test CA"));
        assert!(
            info.not_after
                .as_deref()
                .is_some_and(|time| time.ends_with(" UTC")),
            "{info:?}"
        );
        assert_eq!(info.chain_length, 1);
    }

    #[test]
    fn certificate_summary_reads_expiry_of_old_and_new_time_formats() {
        let (chain, _) = leaf(&["expired.example"], true, false);
        let summary = summarize_certificate(chain[0].as_ref());
        assert_eq!(summary.subject.as_deref(), Some("expired.example"));
        assert_eq!(
            summary.not_after.as_deref(),
            Some("2021-01-01 00:00:00 UTC")
        );
        assert_eq!(
            summarize_certificate(b"\x30\x03garbage"),
            CertificateSummary::default()
        );
    }

    #[test]
    fn rustls_errors_map_to_readable_kinds() {
        use rustls::CertificateError as Cert;
        for (error, kind) in [
            (Cert::UnknownIssuer, TlsFailure::Untrusted),
            (Cert::BadSignature, TlsFailure::Untrusted),
            (Cert::NotValidForName, TlsFailure::HostnameMismatch),
            (Cert::Expired, TlsFailure::Expired),
            (Cert::NotValidYet, TlsFailure::NotYetValid),
            (Cert::Revoked, TlsFailure::Revoked),
        ] {
            assert_eq!(classify(&rustls::Error::InvalidCertificate(error)), kind);
        }
        assert_eq!(
            classify(&rustls::Error::AlertReceived(
                rustls::AlertDescription::HandshakeFailure
            )),
            TlsFailure::Handshake
        );
        // Wrapped in an I/O error (how ureq surfaces handshake failures).
        let io = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(Cert::UnknownIssuer),
        );
        assert!(matches!(
            RemoteError::from_io(&io, "https://h"),
            RemoteError::Tls {
                kind: TlsFailure::Untrusted,
                ..
            }
        ));
        for kind in [
            TlsFailure::Untrusted,
            TlsFailure::HostnameMismatch,
            TlsFailure::Expired,
            TlsFailure::NotYetValid,
            TlsFailure::Revoked,
            TlsFailure::Handshake,
        ] {
            let error = RemoteError::Tls {
                kind,
                detail: "https://h:1".into(),
            };
            assert!(error.localized(true).contains("https://h:1"));
            assert_ne!(error.localized(true), error.localized(false));
            assert!(matches!(error.duplicate(), RemoteError::Tls { kind: k, .. } if k == kind));
        }
    }
}
