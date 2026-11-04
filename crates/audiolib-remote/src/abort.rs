// crates/audiolib-remote/src/abort.rs
//! HTTP requests over one shared keep-alive agent whose sockets a cancel shuts down at once.
//!
//! All requests (API, waveform, audition streams, downloads) go through [`shared_agent`],
//! so a finished request leaves its connection in ureq's pool and the next one (another
//! selection's audition, its waveform) skips the TCP + TLS handshake: over a reverse proxy
//! with ~175 ms RTT a new connection costs ~0.5 s before the first byte.
//!
//! ureq blocks in `call()` until the response head arrives (a server waking a NAS disk
//! may take up to ~100 s) and offers no cancel hook. A request made while an
//! [`Exchange`] (or [`Abort::enter`] scope) is alive on the calling thread registers the
//! connection it uses with that scope's [`Abort`]; [`Abort::abort`] shuts those sockets
//! down, so the blocked call or body read returns at once and the server sees the
//! connection close (releasing its stream slot). The registration follows the request, not
//! the connection: it is made on the connection's next I/O under a scope and dropped when
//! the scope ends, so an idle pooled connection is never shut down by a later cancel of the
//! request that last used it. A cancelled (shut down) connection is never pooled again.
//!
//! The registry shares the transport's own socket (`Arc<TcpStream>`), never a
//! `try_clone` duplicate: on Windows a handle duplicated right after a non-blocking
//! connect may still report "not connected" (WSAENOTCONN), so its shutdown sent nothing
//! and the abandoned request stayed open on the server.
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::ThreadId;
use std::time::Duration;
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, RustlsConnector, Transport,
};

/// Idle connections kept per server: an audition stream, its waveform, API requests.
const IDLE_PER_HOST: usize = 4;
/// How long an idle connection is kept (a closed one is detected before reuse).
const IDLE_AGE: Duration = Duration::from_secs(30);

/// Cancel handle for the requests made in its scopes.
#[derive(Clone, Default)]
pub(crate) struct Abort(Arc<Inner>);

#[derive(Default)]
struct Inner {
    aborted: AtomicBool,
    next_id: AtomicU64,
    sockets: Mutex<HashMap<u64, (Arc<TcpStream>, ThreadId)>>,
}

impl fmt::Debug for Abort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Abort")
            .field("aborted", &self.is_aborted())
            .finish()
    }
}

const CANCELLED: &str = "request cancelled";

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, CANCELLED)
}

impl Abort {
    fn sockets(&self) -> MutexGuard<'_, HashMap<u64, (Arc<TcpStream>, ThreadId)>> {
        self.0
            .sockets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Shut down every connection in use by this handle's requests and refuse new ones.
    pub(crate) fn abort(&self) {
        let sockets = {
            let mut sockets = self.sockets();
            self.0.aborted.store(true, Ordering::Release);
            std::mem::take(&mut *sockets)
        };
        for (socket, _) in sockets.into_values() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    pub(crate) fn is_aborted(&self) -> bool {
        self.0.aborted.load(Ordering::Acquire)
    }

    /// Requests made through [`shared_agent`] on this thread while the guard lives belong
    /// to this handle; reads wait at most `read_stall` for data (except the response head).
    pub(crate) fn enter(&self, read_stall: Option<Duration>) -> ScopeGuard {
        let previous = SCOPE.with(|scope| {
            scope.borrow_mut().replace(Scope {
                abort: self.clone(),
                read_stall,
            })
        });
        ScopeGuard {
            previous,
            abort: self.clone(),
            _thread: PhantomData,
        }
    }

    fn same(&self, other: &Abort) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Track `stream`; checked under the lock so an abort racing the request closes it.
    fn register(&self, stream: &Arc<TcpStream>) -> io::Result<u64> {
        let mut sockets = self.sockets();
        if self.is_aborted() {
            let _ = stream.shutdown(Shutdown::Both);
            return Err(cancelled());
        }
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        sockets.insert(id, (stream.clone(), std::thread::current().id()));
        Ok(id)
    }

    fn is_registered(&self, id: u64) -> bool {
        self.sockets().contains_key(&id)
    }

    fn unregister(&self, id: u64) {
        self.sockets().remove(&id);
    }

    /// Forget (without closing) the connections registered from `thread`.
    fn release(&self, thread: ThreadId) {
        self.sockets().retain(|_, (_, owner)| *owner != thread);
    }

    #[cfg(test)]
    pub(crate) fn open_connections(&self) -> usize {
        self.sockets().len()
    }
}

#[derive(Clone)]
struct Scope {
    abort: Abort,
    read_stall: Option<Duration>,
}

thread_local! {
    static SCOPE: RefCell<Option<Scope>> = const { RefCell::new(None) };
}

fn current_scope() -> Option<Scope> {
    SCOPE.with(|scope| scope.borrow().clone())
}

/// Ends an [`Abort::enter`] scope: restores the enclosing one and releases (keeps open)
/// the connections its requests used, so completed ones stay reusable.
pub(crate) struct ScopeGuard {
    previous: Option<Scope>,
    abort: Abort,
    /// Thread-bound (the scope lives in a thread local).
    _thread: PhantomData<*const ()>,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        SCOPE.with(|scope| *scope.borrow_mut() = self.previous.take());
        self.abort.release(std::thread::current().id());
    }
}

/// The process-wide keep-alive agent (API, snapshot, streams, previews, waveforms,
/// downloads, WebDAV, management requests and the connection test). `https://`
/// certificates are verified against the OS trust store ([`crate::tls`]). Proxies are not
/// supported (the server is reached directly or through a reverse proxy).
pub(crate) fn shared_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            .timeout_connect(Some(crate::client::CONNECT_TIMEOUT))
            .max_idle_connections(4 * IDLE_PER_HOST)
            .max_idle_connections_per_host(IDLE_PER_HOST)
            .max_idle_age(IDLE_AGE)
            .user_agent(concat!("AudioLibExplorer/", env!("CARGO_PKG_VERSION")))
            .tls_config(crate::tls::ureq_tls_config())
            .build();
        ureq::Agent::with_parts(
            config,
            ScopedTcpConnector.chain(RustlsConnector::default()),
            DefaultResolver::default(),
        )
    })
}

static WARM_SPARES: AtomicBool = AtomicBool::new(false);

/// Enable [`warm_spare`] (the desktop and the bench do; off by default so tests see only
/// the requests they make).
pub fn set_connection_warming(enabled: bool) {
    WARM_SPARES.store(enabled, Ordering::Release);
}

/// Spare connections being opened at once.
const MAX_WARMING: usize = 2;

/// Spare requests in flight (decremented once the response was read and its connection
/// returned to the pool).
static WARMING: AtomicUsize = AtomicUsize::new(0);

/// A cancel (another selection) or a seek abandons its response, which kills that
/// connection; the next request would pay a new TCP + TLS handshake (~0.5 s over a remote
/// reverse proxy). Open a replacement in the background, `GET <origin>/api/v1/health`
/// (public, tiny), so it waits in the pool: when an audition starts (for its first seek or
/// the next selection), after each re-request and after a cancel. AudioLib server URLs
/// only; at most [`MAX_WARMING`] at a time, the pool keeps at most 4 idle per server.
pub(crate) fn warm_spare(url: &str) {
    if WARM_SPARES.load(Ordering::Acquire) {
        warm(url);
    }
}

fn warm(url: &str) {
    let Some(index) = url.find("/api/v1/") else {
        return;
    };
    if WARMING
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_WARMING).then_some(n + 1)
        })
        .is_err()
    {
        return;
    }
    let health = format!("{}/api/v1/health", &url[..index]);
    let spawned = std::thread::Builder::new()
        .name("audiolib-http-warm".into())
        .spawn(move || {
            let timeouts = Timeouts::new(Duration::from_secs(10), Some(Duration::from_secs(10)));
            if let Ok(mut response) = timeouts.get(&health).call() {
                let _ = response
                    .body_mut()
                    .with_config()
                    .limit(64 * 1024)
                    .read_to_vec();
            }
            WARMING.fetch_sub(1, Ordering::AcqRel);
        });
    if spawned.is_err() {
        WARMING.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Whether `error` is a reused keep-alive connection the peer had already closed (the
/// request never got an answer): reset, broken pipe or end of stream before the response,
/// never our own cancel.
pub(crate) fn stale_connection(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Io(error) => {
            matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::UnexpectedEof
            ) && error.to_string() != CANCELLED
        }
        _ => false,
    }
}

/// Send an idempotent request built by `build`; when it failed on a stale pooled
/// connection ([`stale_connection`]), send it once more (on a fresh connection).
pub(crate) fn call_fresh(
    build: impl Fn() -> ureq::RequestBuilder<ureq::typestate::WithoutBody>,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    match build().call() {
        Err(error) if stale_connection(&error) => {
            tracing::debug!(%error, "stale keep-alive connection; request sent again");
            build().call()
        }
        result => result,
    }
}

/// Per-request timeouts for [`shared_agent`]: the response head within `response`, the
/// whole body within `body` (`None`: no limit; long transfers bound each read instead).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timeouts {
    pub(crate) response: Duration,
    pub(crate) body: Option<Duration>,
}

impl Timeouts {
    pub(crate) fn new(response: Duration, body: Option<Duration>) -> Self {
        Self { response, body }
    }

    fn apply<B>(&self, request: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        let config = request
            .config()
            .timeout_recv_response(Some(self.response))
            .timeout_recv_body(self.body);
        // Tests may install their throwaway CA after the shared agent was built.
        #[cfg(test)]
        let config = config.tls_config(crate::tls::ureq_tls_config());
        config.build()
    }

    pub(crate) fn get(&self, url: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
        self.apply(shared_agent().get(url))
    }

    pub(crate) fn head(&self, url: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
        self.apply(shared_agent().head(url))
    }

    pub(crate) fn post(&self, url: &str) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
        self.apply(shared_agent().post(url))
    }

    pub(crate) fn patch(&self, url: &str) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
        self.apply(shared_agent().patch(url))
    }

    pub(crate) fn delete(&self, url: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
        self.apply(shared_agent().delete(url))
    }

    /// Run a prepared request (e.g. a WebDAV `PROPFIND`) with these timeouts.
    pub(crate) fn run<S: ureq::AsSendBody>(
        &self,
        request: ureq::http::Request<S>,
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let config = shared_agent()
            .configure_request(request)
            .timeout_recv_response(Some(self.response))
            .timeout_recv_body(self.body);
        #[cfg(test)]
        let config = config.tls_config(crate::tls::ureq_tls_config());
        shared_agent().run(config.build())
    }
}

/// Requests through [`shared_agent`] with fixed timeouts whose connections the given
/// [`Abort`] can shut down while this value lives (on this thread).
pub(crate) struct Exchange {
    timeouts: Timeouts,
    _scope: ScopeGuard,
}

impl std::ops::Deref for Exchange {
    type Target = Timeouts;

    fn deref(&self) -> &Timeouts {
        &self.timeouts
    }
}

/// Short exchanges (head and body each bounded) cancellable through `abort`.
pub(crate) fn abortable_agent(response: Duration, body: Duration, abort: &Abort) -> Exchange {
    Exchange {
        timeouts: Timeouts::new(response, Some(body)),
        _scope: abort.enter(None),
    }
}

/// [`abortable_agent`] for long transfers (audition streams, downloads): no limit on the
/// whole body, but any single read that waits longer than `read_stall` for data fails
/// (a dead link is noticed, a slow but moving one never is). Waiting for the response
/// head keeps the `response` limit (a waking NAS disk).
pub(crate) fn abortable_stream_agent(
    response: Duration,
    read_stall: Duration,
    abort: &Abort,
) -> Exchange {
    Exchange {
        timeouts: Timeouts::new(response, None),
        _scope: abort.enter(Some(read_stall)),
    }
}

#[derive(Debug)]
struct ScopedTcpConnector;

impl Connector<()> for ScopedTcpConnector {
    type Out = ScopedTransport;

    fn connect(
        &self,
        details: &ConnectionDetails,
        _chained: Option<()>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        if current_scope().is_some_and(|scope| scope.abort.is_aborted()) {
            return Err(cancelled().into());
        }
        let timeout = details.timeout.not_zero().map(|timeout| *timeout);
        let mut last = None;
        for address in details.addrs.iter() {
            let attempt = match timeout {
                Some(timeout) => TcpStream::connect_timeout(address, timeout),
                None => TcpStream::connect(address),
            };
            let stream = match attempt {
                Ok(stream) => stream,
                Err(error) => {
                    last = Some(error);
                    continue;
                }
            };
            if details.config.no_delay() {
                stream.set_nodelay(true)?;
            }
            let mut transport = ScopedTransport {
                stream: Arc::new(stream),
                buffers: LazyBuffers::new(
                    details.config.input_buffer_size(),
                    details.config.output_buffer_size(),
                ),
                read_timeout: None,
                write_timeout: None,
                registration: None,
                read_stall: None,
            };
            transport.follow_scope()?;
            return Ok(Some(transport));
        }
        Err(match last {
            Some(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                ureq::Error::Timeout(details.timeout.reason)
            }
            Some(error) => error.into(),
            None => {
                io::Error::new(io::ErrorKind::ConnectionRefused, "no address to connect").into()
            }
        })
    }
}

/// Plain TCP transport (as ureq's own) registered with the current request's [`Abort`].
struct ScopedTransport {
    /// Shared with the [`Abort`] registry (same socket, not a duplicate handle).
    stream: Arc<TcpStream>,
    buffers: LazyBuffers,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    /// The scope of the request using the connection, and its registry id.
    registration: Option<(Abort, u64)>,
    /// Longest wait for data in one read, except for the response head.
    read_stall: Option<Duration>,
}

impl fmt::Debug for ScopedTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScopedTransport")
            .field("peer", &self.stream.peer_addr().ok())
            .finish()
    }
}

impl Drop for ScopedTransport {
    fn drop(&mut self) {
        if let Some((abort, id)) = self.registration.take() {
            abort.unregister(id);
        }
    }
}

impl ScopedTransport {
    /// Register with the calling thread's scope (if any) unless already registered with it.
    fn follow_scope(&mut self) -> io::Result<()> {
        let scope = current_scope();
        let current = match (&self.registration, &scope) {
            (Some((abort, id)), Some(scope)) => {
                abort.same(&scope.abort) && abort.is_registered(*id)
            }
            _ => false,
        };
        if current {
            return Ok(());
        }
        if let Some((abort, id)) = self.registration.take() {
            abort.unregister(id);
        }
        self.read_stall = scope.as_ref().and_then(|scope| scope.read_stall);
        if let Some(scope) = scope {
            let id = scope.abort.register(&self.stream)?;
            self.registration = Some((scope.abort, id));
        }
        Ok(())
    }

    fn aborted(&self) -> bool {
        self.registration
            .as_ref()
            .is_some_and(|(abort, _)| abort.is_aborted())
    }

    /// Map an I/O failure: a timeout keeps ureq's reason, an aborted request says so.
    fn failure(&self, error: io::Error, timeout: NextTimeout) -> ureq::Error {
        if self.aborted() {
            return cancelled().into();
        }
        match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
                ureq::Error::Timeout(timeout.reason)
            }
            _ => error.into(),
        }
    }

    /// Keep one deadline across interrupted/polled reads; injectable operations let
    /// tests exercise cancellation and deadline exhaustion without signals or sleeps.
    fn await_input_with(
        &mut self,
        timeout: NextTimeout,
        mut read: impl FnMut(&mut Self) -> io::Result<usize>,
        mut now: impl FnMut() -> std::time::Instant,
    ) -> Result<bool, ureq::Error> {
        self.follow_scope()?;
        let mut wanted = timeout.not_zero().map(|timeout| *timeout);
        if let Some(stall) = self.read_stall
            && timeout.reason != ureq::Timeout::RecvResponse
        {
            wanted = Some(wanted.map_or(stall, |wanted| wanted.min(stall)));
        }
        // Windows shutdown can leave a pending blocking recv asleep until the peer
        // writes/closes. Poll scoped reads so cancellation also bounds the worker's
        // lifetime when the peer retains its socket; retain the original deadline.
        let deadline = wanted.map(|wait| now() + wait);
        let scoped = self.registration.is_some();
        let amount = loop {
            if self.aborted() {
                return Err(cancelled().into());
            }
            let remaining = deadline.map(|deadline| deadline.saturating_duration_since(now()));
            if remaining.is_some_and(|remaining| remaining.is_zero()) {
                return Err(ureq::Error::Timeout(timeout.reason));
            }
            let read_wait = if scoped {
                Some(remaining.map_or(crate::client::WAIT_TICK, |remaining| {
                    remaining.min(crate::client::WAIT_TICK)
                }))
            } else {
                remaining
            };
            if read_wait != self.read_timeout {
                self.stream.set_read_timeout(read_wait)?;
                self.read_timeout = read_wait;
            }
            match read(self) {
                Ok(amount) => break amount,
                // A signal can interrupt recv before either cancellation or the
                // timeout. Retry through the same abort/deadline checks above.
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error)
                    if scoped
                        && matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) => {}
                Err(error) => return Err(self.failure(error, timeout)),
            }
        };
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }
}

impl Transport for ScopedTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.follow_scope()?;
        let wanted = timeout.not_zero().map(|timeout| *timeout);
        if wanted != self.write_timeout {
            self.stream.set_write_timeout(wanted)?;
            self.write_timeout = wanted;
        }
        let output = &self.buffers.output()[..amount];
        (&*self.stream)
            .write_all(output)
            .map_err(|error| self.failure(error, timeout))
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.await_input_with(
            timeout,
            |transport| (&*transport.stream).read(transport.buffers.input_append_buf()),
            std::time::Instant::now,
        )
    }

    fn is_open(&mut self) -> bool {
        // A cancelled connection was shut down: the probe sees it closed. (Not the stale
        // registration: a later cancel of the request that last used it is irrelevant.)
        if self.stream.set_nonblocking(true).is_err() {
            return false;
        }
        let mut probe = [0u8];
        let open = matches!((&*self.stream).read(&mut probe), Err(error) if error.kind() == io::ErrorKind::WouldBlock);
        open && self.stream.set_nonblocking(false).is_ok()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::Cell;
    use std::net::TcpListener;
    use std::time::Instant;

    fn transport_for_read_test() -> (ScopedTransport, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream =
            TcpStream::connect_timeout(&listener.local_addr().unwrap(), Duration::from_secs(5))
                .unwrap();
        let (peer, _) = listener.accept().unwrap();
        (
            ScopedTransport {
                stream: Arc::new(stream),
                buffers: LazyBuffers::new(16, 16),
                read_timeout: None,
                write_timeout: None,
                registration: None,
                read_stall: None,
            },
            peer,
        )
    }

    fn response_timeout(wait: Duration) -> NextTimeout {
        NextTimeout {
            after: ureq::unversioned::transport::time::Duration::Exact(wait),
            reason: ureq::Timeout::RecvResponse,
        }
    }

    #[test]
    fn interrupted_read_retries_and_appends_the_received_bytes() {
        let (mut transport, _peer) = transport_for_read_test();
        let mut reads = 0;
        let now = Instant::now();
        let received = transport
            .await_input_with(
                response_timeout(Duration::from_secs(1)),
                |transport| {
                    reads += 1;
                    if reads == 1 {
                        return Err(io::ErrorKind::Interrupted.into());
                    }
                    transport.buffers.input_append_buf()[..2].copy_from_slice(b"ok");
                    Ok(2)
                },
                || now,
            )
            .unwrap();
        assert!(received);
        assert_eq!(reads, 2);
        assert_eq!(transport.buffers.input(), b"ok");
    }

    #[test]
    fn interrupted_read_rechecks_cancellation_before_another_read() {
        let (mut transport, _peer) = transport_for_read_test();
        let abort = Abort::default();
        let _scope = abort.enter(None);
        let mut reads = 0;
        let now = Instant::now();
        let result = transport.await_input_with(
            response_timeout(Duration::from_secs(1)),
            |_| {
                reads += 1;
                abort.abort();
                Err(io::ErrorKind::Interrupted.into())
            },
            || now,
        );
        assert!(
            matches!(result, Err(ureq::Error::Io(error)) if error.kind() == io::ErrorKind::ConnectionAborted)
        );
        assert_eq!(reads, 1);
    }

    #[test]
    fn interrupted_reads_keep_the_original_deadline() {
        let (mut transport, _peer) = transport_for_read_test();
        let now = Cell::new(Instant::now());
        let mut waits = Vec::new();
        let result = transport.await_input_with(
            response_timeout(Duration::from_secs(3)),
            |transport| {
                assert!(waits.len() < 3, "interrupted reads extended the deadline");
                waits.push(transport.read_timeout.unwrap());
                now.set(now.get() + Duration::from_secs(1));
                Err(io::ErrorKind::Interrupted.into())
            },
            || now.get(),
        );
        assert!(matches!(
            result,
            Err(ureq::Error::Timeout(ureq::Timeout::RecvResponse))
        ));
        assert_eq!(
            waits,
            [
                Duration::from_secs(3),
                Duration::from_secs(2),
                Duration::from_secs(1)
            ]
        );
    }

    /// A server that accepts, reads the request head and never answers; sends the instant
    /// it saw the client close the connection.
    pub(crate) fn silent_server() -> (String, std::sync::mpsc::Receiver<Instant>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/api/v1/audio/1/stream",
            listener.local_addr().unwrap()
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            let mut buffer = [0u8; 4096];
            // The request head, then wait for EOF (or a reset) from the client.
            loop {
                match socket.read(&mut buffer) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let _ = sender.send(Instant::now());
        });
        (url, receiver)
    }

    #[test]
    fn abort_closes_a_request_waiting_for_its_response() {
        let (url, closed) = silent_server();
        let abort = Abort::default();
        let handle = abort.clone();
        let call = std::thread::spawn(move || {
            abortable_agent(Duration::from_secs(60), Duration::from_secs(60), &handle)
                .get(&url)
                .call()
                .map(|_| ())
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while abort.open_connections() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(abort.open_connections(), 1);
        let cancelled_at = Instant::now();
        abort.abort();
        let seen = closed
            .recv_timeout(Duration::from_secs(5))
            .expect("the server must see the connection closed");
        assert!(seen.duration_since(cancelled_at) < Duration::from_secs(2));
        assert!(call.join().unwrap().is_err());
        // New requests through an aborted handle fail without connecting.
        let refused = abortable_agent(Duration::from_secs(1), Duration::from_secs(1), &abort)
            .get("http://127.0.0.1:9/")
            .call();
        assert!(refused.is_err());
    }

    /// A keep-alive server: answers every request on a connection with `body`; reports
    /// each accepted connection and each connection end.
    fn keep_alive_server(body: usize) -> (String, Arc<AtomicU64>, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/k", listener.local_addr().unwrap());
        let (accepted, ended) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (accepted_count, ended_count) = (accepted.clone(), ended.clone());
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { break };
                accepted_count.fetch_add(1, Ordering::SeqCst);
                let ended = ended_count.clone();
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 8192];
                    let mut pending = Vec::new();
                    loop {
                        match socket.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(read) => pending.extend_from_slice(&buffer[..read]),
                        }
                        while let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            pending.drain(..end + 4);
                            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {body}\r\n\r\n");
                            if socket.write_all(head.as_bytes()).is_err()
                                || socket.write_all(&vec![7u8; body]).is_err()
                            {
                                break;
                            }
                        }
                    }
                    ended.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        (url, accepted, ended)
    }

    #[test]
    fn completed_requests_reuse_their_connection_and_a_later_cancel_keeps_it_open() {
        let (url, accepted, ended) = keep_alive_server(100);
        let read = |abort: &Abort| {
            let exchange =
                abortable_stream_agent(Duration::from_secs(5), Duration::from_secs(5), abort);
            let mut response = exchange.get(&url).call().unwrap();
            response.body_mut().read_to_vec().unwrap().len()
        };
        let first = Abort::default();
        assert_eq!(read(&first), 100);
        // The finished request's scope ended: cancelling it now must not close the idle
        // pooled connection (the next selection reuses it).
        first.abort();
        assert_eq!(read(&Abort::default()), 100);
        assert_eq!(timeouts_get(&url), 100);
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "one connection for all three"
        );
        assert_eq!(ended.load(Ordering::SeqCst), 0);
    }

    fn timeouts_get(url: &str) -> usize {
        Timeouts::new(Duration::from_secs(5), Some(Duration::from_secs(5)))
            .get(url)
            .call()
            .unwrap()
            .body_mut()
            .read_to_vec()
            .unwrap()
            .len()
    }

    /// Answers one request per connection (keep-alive), then closes the connection when
    /// the next request arrives on it: a server timing out an idle connection just as it
    /// is reused.
    fn one_answer_per_connection_server() -> (String, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/once", listener.local_addr().unwrap());
        let accepted = Arc::new(AtomicU64::new(0));
        let count = accepted.clone();
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { break };
                count.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 8192];
                    let mut answered = false;
                    while let Ok(read) = socket.read(&mut buffer) {
                        if read == 0 {
                            break;
                        }
                        if answered {
                            // The reused connection: close instead of answering.
                            let _ = socket.shutdown(Shutdown::Both);
                            break;
                        }
                        answered = true;
                        let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                    }
                });
            }
        });
        (url, accepted)
    }

    #[test]
    fn a_request_on_a_connection_closed_by_the_server_is_sent_again() {
        let (url, accepted) = one_answer_per_connection_server();
        let timeouts = Timeouts::new(Duration::from_secs(5), Some(Duration::from_secs(5)));
        let body = |response: Result<ureq::http::Response<ureq::Body>, ureq::Error>| {
            response.unwrap().body_mut().read_to_string().unwrap()
        };
        assert_eq!(body(call_fresh(|| timeouts.get(&url))), "ok");
        // The pooled connection looks open (the server closes it only on the next request).
        assert_eq!(body(call_fresh(|| timeouts.get(&url))), "ok");
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            2,
            "the retry used a fresh connection"
        );
        assert!(!stale_connection(&ureq::Error::Io(cancelled())));
    }

    #[test]
    fn a_warm_spare_is_reused_by_the_next_request() {
        let (url, accepted, ended) = keep_alive_server(10);
        let source = url.replace("/k", "/api/v1/audio/1/preview");
        warm(&source);
        let deadline = Instant::now() + Duration::from_secs(5);
        while accepted.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // Let the spare's request finish and return its connection to the pool.
        while WARMING.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(timeouts_get(&source), 10);
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "the request used the spare"
        );
        assert_eq!(ended.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_cancelled_transfer_closes_its_connection_and_is_not_reused() {
        let (url, accepted, ended) = keep_alive_server(8 << 20);
        let abort = Abort::default();
        {
            let exchange =
                abortable_stream_agent(Duration::from_secs(5), Duration::from_secs(5), &abort);
            let mut response = exchange.get(&url).call().unwrap();
            let mut reader = response.body_mut().as_reader();
            let mut chunk = [0u8; 1024];
            reader.read_exact(&mut chunk).unwrap();
            abort.abort();
            let mut rest = Vec::new();
            assert!(
                reader.read_to_end(&mut rest).is_err(),
                "the read fails once cancelled"
            );
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while ended.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(ended.load(Ordering::SeqCst), 1, "the server saw the cancel");
        let _ = timeouts_get(&url);
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            2,
            "a fresh connection after a cancel"
        );
    }

    /// Answers after `head_delay`, sends 1000 of the promised 100 000 body bytes, then
    /// goes silent with the connection open.
    fn stalling_server(head_delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/s", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer);
            std::thread::sleep(head_delay);
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n");
            let _ = socket.write_all(&[0u8; 1000]);
            std::thread::sleep(Duration::from_secs(20));
        });
        url
    }

    #[test]
    fn stream_agents_bound_each_read_but_not_the_response_head() {
        // The head may take longer than the read limit (a waking disk) ...
        let agent = abortable_stream_agent(
            Duration::from_secs(10),
            Duration::from_millis(300),
            &Abort::default(),
        );
        let mut response = agent
            .get(&stalling_server(Duration::from_millis(800)))
            .call()
            .expect("the head is waited for beyond the read limit");
        // ... but a body that stops moving fails after the read limit, not 30 minutes.
        let started = Instant::now();
        let mut body = Vec::new();
        let result = response.body_mut().as_reader().read_to_end(&mut body);
        assert!(result.is_err(), "{result:?}");
        assert!(body.len() <= 1000);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }
}
