//! The session relay: one long-lived process between a build's compiler
//! wrappers and a remote `bellowsd`.
//!
//! Cargo starts a fresh wrapper process for every rustc invocation. Talking to
//! a far server directly, each wrapper pays a TCP and TLS handshake before its
//! first request, and waits for its uploads before Cargo may start the units
//! that depend on it. The relay listens on loopback and keeps a warm
//! connection pool to the server, so a wrapper's request costs one round trip
//! to the server and nothing more. Publications are answered as soon as they
//! are spooled to local disk and uploaded in the background, in dependency
//! order: a record only after every blob accepted before it, a lease release
//! only after every record before it. Any other request that changes the
//! server waits for earlier uploads too, so it sees what the wrappers sent.
//!
//! Reads pass straight through. Wrappers still verify every blob's digest, and
//! the relay verifies each spooled blob before uploading it, so the relay is
//! never trusted with integrity. Wrappers authenticate with a per-session
//! token; the server's token stays in the relay, out of Cargo's environment.

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::serve::ListenerExt;
use futures_util::StreamExt;
use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Semaphore, watch};
use tokio_util::io::ReaderStream;
use tokio_util::task::TaskTracker;

/// The largest JSON request the relay buffers (the server's own limit).
const MAX_JSON_BODY_BYTES: usize = 16 * 1024 * 1024;

pub struct RelayOptions {
    /// The server's base URL, without a trailing slash.
    pub upstream: String,
    pub upstream_token: Option<String>,
    pub listen: SocketAddr,
    /// Where accepted blobs wait for upload. Created if missing; the relay
    /// removes each file once its upload ends.
    pub spool_dir: PathBuf,
    /// Accepted but not yet uploaded bytes above which a further blob waits
    /// (its wrapper with it) until uploads make room.
    pub spool_limit_bytes: u64,
    pub upload_concurrency: usize,
    pub connect_timeout: Duration,
}

/// What a relay sent to the server in the background.
#[derive(Debug, Default)]
pub struct RelaySummary {
    pub blobs_uploaded: u64,
    pub blob_bytes: u64,
    pub blobs_present: u64,
    pub records: u64,
    pub failures: u64,
    pub first_failure: Option<String>,
    /// Background work still running when the drain timed out.
    pub abandoned: bool,
}

impl RelaySummary {
    pub fn describe(&self) -> String {
        let mut text = format!(
            "uploaded {} blob(s) ({}), {} already on the server, {} record(s)",
            self.blobs_uploaded,
            super::human_bytes(self.blob_bytes),
            self.blobs_present,
            self.records
        );
        if self.failures > 0 {
            text.push_str(&format!(
                "; {} upload(s) failed (first: {})",
                self.failures,
                self.first_failure.as_deref().unwrap_or("unknown")
            ));
        }
        if self.abandoned {
            text.push_str("; the drain timed out and left uploads unfinished");
        }
        text
    }
}

pub struct Relay {
    /// The loopback base URL wrappers use as `BELLOWS_SERVER`.
    pub url: String,
    /// The session token wrappers use as `BELLOWS_AUTH_TOKEN`.
    pub token: String,
    shared: Arc<Shared>,
    runtime: tokio::runtime::Runtime,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

impl Relay {
    pub fn start(options: RelayOptions) -> Result<Self> {
        std::fs::create_dir_all(&options.spool_dir)
            .with_context(|| format!("create relay spool {}", options.spool_dir.display()))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("bellows-relay")
            .enable_all()
            .build()
            .context("start relay runtime")?;
        let listener = std::net::TcpListener::bind(options.listen)
            .with_context(|| format!("bind relay listener {}", options.listen))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|error| anyhow::anyhow!("relay token: {error}"))?;
        let token = secret.iter().map(|byte| format!("{byte:02x}")).collect();
        let client = reqwest::Client::builder()
            // Several warm HTTP/1.1 connections rather than one multiplexed
            // HTTP/2 connection: large blobs then download on parallel TCP
            // streams instead of sharing one congestion window.
            .http1_only()
            .pool_max_idle_per_host(64)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .tcp_nodelay(true)
            .connect_timeout(options.connect_timeout)
            .build()
            .context("build relay client")?;
        let (changed, _) = watch::channel(0u64);
        let (record_changed, _) = watch::channel(0u64);
        let shared = Arc::new(Shared {
            client,
            upstream: options.upstream,
            upstream_token: options.upstream_token,
            local_token: token,
            spool_dir: options.spool_dir,
            spool_limit: options.spool_limit_bytes,
            spooled: AtomicU64::new(0),
            spool_files: AtomicU64::new(0),
            uploads: Semaphore::new(options.upload_concurrency.max(1)),
            blobs: Mutex::new(HashMap::new()),
            blob_order: Order::new(changed),
            record_order: Order::new(record_changed),
            tracker: TaskTracker::new(),
            stats: Stats::default(),
        });
        let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();
        let app = Router::new()
            .fallback(handle)
            .layer(DefaultBodyLimit::disable())
            .with_state(shared.clone());
        let server = {
            let _entered = runtime.enter();
            // Without TCP_NODELAY, a response streamed in small pieces waits on
            // delayed acknowledgements (tens of milliseconds a request).
            let listener = tokio::net::TcpListener::from_std(listener)?.tap_io(|stream| {
                let _ = stream.set_nodelay(true);
            });
            runtime.spawn(async move {
                let served = axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = stopped.await;
                    })
                    .await;
                if let Err(error) = served {
                    eprintln!("bellows relay: listener failed: {error}");
                }
            })
        };
        Ok(Self {
            url: format!("http://{address}"),
            token: shared.local_token.clone(),
            shared,
            runtime,
            shutdown: Some(shutdown),
            server: Some(server),
        })
    }

    /// Stops listening, then waits up to `timeout` for background uploads.
    pub fn finish(mut self, timeout: Duration) -> RelaySummary {
        let shared = self.shared.clone();
        let shutdown = self.shutdown.take();
        let server = self.server.take();
        let drained = self.runtime.block_on(async move {
            if let Some(shutdown) = shutdown {
                let _ = shutdown.send(());
            }
            if let Some(server) = server {
                // Connections still open (a wrapper Cargo left behind) do not
                // hold the drain past its bound.
                let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
            }
            shared.tracker.close();
            tokio::time::timeout(timeout, shared.tracker.wait())
                .await
                .is_ok()
        });
        let stats = &self.shared.stats;
        let summary = RelaySummary {
            blobs_uploaded: stats.blobs_uploaded.load(Ordering::Relaxed),
            blob_bytes: stats.blob_bytes.load(Ordering::Relaxed),
            blobs_present: stats.blobs_present.load(Ordering::Relaxed),
            records: stats.records.load(Ordering::Relaxed),
            failures: stats.failures.load(Ordering::Relaxed),
            first_failure: stats
                .first_failure
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            abandoned: !drained,
        };
        // Uploads remove their own files; after a timed-out drain the rest
        // are abandoned (a later build republishes them), so the spool goes.
        let _ = std::fs::remove_dir_all(&self.shared.spool_dir);
        summary
    }

    /// Runs `future` on the relay's runtime (a standalone relay waits for its
    /// stop signal here).
    pub fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

struct Shared {
    client: reqwest::Client,
    upstream: String,
    upstream_token: Option<String>,
    local_token: String,
    spool_dir: PathBuf,
    spool_limit: u64,
    spooled: AtomicU64,
    spool_files: AtomicU64,
    uploads: Semaphore,
    blobs: Mutex<HashMap<String, BlobState>>,
    blob_order: Order,
    record_order: Order,
    tracker: TaskTracker,
    stats: Stats,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlobState {
    Pending,
    Done,
    Failed,
}

#[derive(Default)]
struct Stats {
    blobs_uploaded: AtomicU64,
    blob_bytes: AtomicU64,
    blobs_present: AtomicU64,
    records: AtomicU64,
    failures: AtomicU64,
    first_failure: Mutex<Option<String>>,
}

impl Stats {
    fn fail(&self, message: String) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        let mut first = self.first_failure.lock().unwrap_or_else(|e| e.into_inner());
        if first.is_none() {
            *first = Some(message.clone());
        }
        drop(first);
        super::record_event("fallback", "bellows-relay", None, None, &message);
    }
}

/// Background work in acceptance order: `wait_before(h)` returns once every
/// item begun before the horizon `h` has finished, however they interleave.
struct Order {
    state: Mutex<(u64, BTreeSet<u64>)>,
    changed: watch::Sender<u64>,
}

impl Order {
    fn new(changed: watch::Sender<u64>) -> Self {
        Self {
            state: Mutex::new((0, BTreeSet::new())),
            changed,
        }
    }

    fn begin(&self) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let sequence = state.0;
        state.0 += 1;
        state.1.insert(sequence);
        sequence
    }

    fn horizon(&self) -> u64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0
    }

    fn finish(&self, sequence: u64) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .remove(&sequence);
        self.changed.send_modify(|version| *version += 1);
    }

    fn passed(&self, horizon: u64) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .first()
            .is_none_or(|open| *open >= horizon)
    }

    async fn wait_before(&self, horizon: u64) {
        let mut changed = self.changed.subscribe();
        while !self.passed(horizon) {
            if changed.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Shared {
    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return false;
        };
        let (left, right) = (value.as_bytes(), self.local_token.as_bytes());
        left.len() == right.len()
            && left
                .iter()
                .zip(right)
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }

    fn upstream(&self, method: Method, path_and_query: &str) -> reqwest::RequestBuilder {
        let request = self
            .client
            .request(method, format!("{}{path_and_query}", self.upstream));
        match &self.upstream_token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    fn blob_state(&self, digest: &str) -> Option<BlobState> {
        self.blobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(digest)
            .copied()
    }
}

async fn handle(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    if path == "/live" {
        return StatusCode::NO_CONTENT.into_response();
    }
    if !shared.authorized(request.headers()) {
        return (StatusCode::UNAUTHORIZED, "relay session token required").into_response();
    }
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_owned())
        .unwrap_or_else(|| path.clone());
    let segments = path.trim_start_matches('/').split('/').collect::<Vec<_>>();
    let method = request.method().clone();
    match segments.as_slice() {
        ["v1", "blobs", digest] if method == Method::HEAD => {
            // Known means accepted this session; anything else is uploaded
            // through the spool, which asks the server itself.
            match shared.blob_state(digest) {
                Some(BlobState::Pending | BlobState::Done) => StatusCode::OK.into_response(),
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }
        ["v1", "blobs", digest] if method == Method::PUT => {
            let digest = (*digest).to_owned();
            put_blob(shared, digest, request.into_body()).await
        }
        ["v1", "actions", _, _] if method == Method::PUT => {
            defer(shared, request, path_and_query, Deferred::Record).await
        }
        ["v1", "actions", _, _, "used"] if method == Method::POST => {
            defer(shared, request, path_and_query, Deferred::Use).await
        }
        ["v1", "leases", _, _] if method == Method::DELETE => {
            defer(shared, request, path_and_query, Deferred::Release).await
        }
        // Reads, and lease requests (which precede a compile and name no
        // blob), pass straight through.
        ["v1", "leases", _] if method == Method::POST => {
            forward(&shared, request, &path_and_query).await
        }
        _ if method == Method::GET || method == Method::HEAD => {
            forward(&shared, request, &path_and_query).await
        }
        // Every other change (archives, declared records, remote execution,
        // collection) sees every blob accepted before it.
        _ => {
            shared
                .blob_order
                .wait_before(shared.blob_order.horizon())
                .await;
            forward(&shared, request, &path_and_query).await
        }
    }
}

async fn forward(shared: &Shared, request: Request, path_and_query: &str) -> Response {
    let method = request.method().clone();
    let content_type = request.headers().get(header::CONTENT_TYPE).cloned();
    let body = match axum::body::to_bytes(request.into_body(), MAX_JSON_BODY_BYTES).await {
        Ok(body) => body,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let mut upstream = shared.upstream(method, path_and_query);
    if let Some(content_type) = content_type {
        upstream = upstream.header(header::CONTENT_TYPE, content_type);
    }
    if !body.is_empty() {
        upstream = upstream.body(body);
    }
    match upstream.send().await {
        Ok(response) => relay_response(response),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            format!("relay could not reach the server: {error}"),
        )
            .into_response(),
    }
}

fn relay_response(response: reqwest::Response) -> Response {
    let mut builder = Response::builder().status(response.status());
    for name in [header::CONTENT_TYPE, header::CONTENT_LENGTH] {
        if let Some(value) = response.headers().get(&name) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|error| (StatusCode::BAD_GATEWAY, error.to_string()).into_response())
}

async fn put_blob(shared: Arc<Shared>, digest: String, body: Body) -> Response {
    if bellows_core::validate_content_key(&digest).is_err() {
        return (StatusCode::BAD_REQUEST, "invalid blob digest").into_response();
    }
    if matches!(
        shared.blob_state(&digest),
        Some(BlobState::Pending | BlobState::Done)
    ) {
        return StatusCode::NO_CONTENT.into_response();
    }
    // Back-pressure: past the spool limit, wait for uploads to make room. An
    // empty spool always takes the next blob, however large.
    let mut changed = shared.blob_order.changed.subscribe();
    while shared.spooled.load(Ordering::Acquire) >= shared.spool_limit
        && shared.spool_files.load(Ordering::Acquire) > 0
    {
        if changed.changed().await.is_err() {
            break;
        }
    }
    let path = shared
        .spool_dir
        .join(format!("{digest}.{}", spool_counter()));
    let spooled = async {
        let mut file = tokio::fs::File::create(&path).await?;
        let mut hasher = blake3::Hasher::new();
        let mut length = 0u64;
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(std::io::Error::other)?;
            hasher.update(&chunk);
            length += chunk.len() as u64;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok::<_, std::io::Error>((hasher.finalize().to_hex().to_string(), length))
    }
    .await;
    let (actual, length) = match spooled {
        Ok(value) => value,
        Err(error) => {
            let _ = tokio::fs::remove_file(&path).await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("relay spool: {error}"),
            )
                .into_response();
        }
    };
    if actual != digest {
        let _ = tokio::fs::remove_file(&path).await;
        return (StatusCode::BAD_REQUEST, "blob digest mismatch").into_response();
    }
    let sequence = {
        let mut blobs = shared.blobs.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(
            blobs.get(&digest),
            Some(BlobState::Pending | BlobState::Done)
        ) {
            None
        } else {
            blobs.insert(digest.clone(), BlobState::Pending);
            shared.spooled.fetch_add(length, Ordering::AcqRel);
            shared.spool_files.fetch_add(1, Ordering::AcqRel);
            Some(shared.blob_order.begin())
        }
    };
    let Some(sequence) = sequence else {
        let _ = tokio::fs::remove_file(&path).await;
        return StatusCode::NO_CONTENT.into_response();
    };
    let task = shared.clone();
    shared.tracker.spawn(async move {
        let outcome = upload(&task, &digest, &path, length).await;
        let _ = tokio::fs::remove_file(&path).await;
        let state = match outcome {
            Ok(true) => {
                task.stats.blobs_uploaded.fetch_add(1, Ordering::Relaxed);
                task.stats.blob_bytes.fetch_add(length, Ordering::Relaxed);
                BlobState::Done
            }
            Ok(false) => {
                task.stats.blobs_present.fetch_add(1, Ordering::Relaxed);
                BlobState::Done
            }
            Err(error) => {
                task.stats.fail(format!("relay upload failed: {error:#}"));
                BlobState::Failed
            }
        };
        task.blobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(digest, state);
        task.spooled.fetch_sub(length, Ordering::AcqRel);
        task.spool_files.fetch_sub(1, Ordering::AcqRel);
        task.blob_order.finish(sequence);
    });
    StatusCode::CREATED.into_response()
}

fn spool_counter() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Uploads one spooled blob; `Ok(false)` when the server already had it.
async fn upload(shared: &Shared, digest: &str, path: &PathBuf, length: u64) -> Result<bool> {
    let _permit = shared.uploads.acquire().await?;
    let blob_path = format!("/v1/blobs/{digest}");
    if length > super::UPLOAD_WITHOUT_ASKING_BYTES as u64 {
        let present = shared
            .upstream(Method::HEAD, &blob_path)
            .timeout(Duration::from_secs(60))
            .send()
            .await;
        if present.is_ok_and(|response| response.status().is_success()) {
            return Ok(false);
        }
    }
    let mut attempt = 0;
    loop {
        attempt += 1;
        let file = tokio::fs::File::open(path)
            .await
            .context("reopen spooled blob")?;
        let sent = shared
            .upstream(Method::PUT, &blob_path)
            .header(header::CONTENT_LENGTH, length)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .timeout(Duration::from_secs(30 * 60))
            .body(reqwest::Body::wrap_stream(ReaderStream::new(file)))
            .send()
            .await;
        match sent {
            Ok(response) if response.status().is_success() => return Ok(true),
            Ok(response) if response.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                anyhow::bail!(
                    "blob {digest} ({}) exceeds the server's blob size limit; it stays local",
                    super::human_bytes(length)
                )
            }
            Ok(response) => {
                anyhow::bail!("blob {digest}: server answered {}", response.status())
            }
            Err(error) if attempt < 3 && (error.is_connect() || error.is_request()) => {
                tokio::time::sleep(Duration::from_millis(250 * attempt)).await;
            }
            Err(error) => return Err(error).with_context(|| format!("upload blob {digest}")),
        }
    }
}

#[derive(Clone, Copy)]
enum Deferred {
    /// A compiler or build-script record: after the blobs it names.
    Record,
    /// A best-effort use mark: no ordering.
    Use,
    /// A single-flight lease release: after the records published under it,
    /// so a waiting runner finds the result instead of compiling again.
    Release,
}

async fn defer(
    shared: Arc<Shared>,
    request: Request,
    path_and_query: String,
    kind: Deferred,
) -> Response {
    let method = request.method().clone();
    let content_type = request.headers().get(header::CONTENT_TYPE).cloned();
    let body = match axum::body::to_bytes(request.into_body(), MAX_JSON_BODY_BYTES).await {
        Ok(body) => body,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let blob_horizon = shared.blob_order.horizon();
    let record_horizon = shared.record_order.horizon();
    let sequence = matches!(kind, Deferred::Record).then(|| shared.record_order.begin());
    let task = shared.clone();
    shared.tracker.spawn(async move {
        match kind {
            Deferred::Record => task.blob_order.wait_before(blob_horizon).await,
            Deferred::Use => {}
            Deferred::Release => {
                task.blob_order.wait_before(blob_horizon).await;
                task.record_order.wait_before(record_horizon).await;
            }
        }
        let mut attempt = 0;
        let outcome = loop {
            attempt += 1;
            let mut upstream = task
                .upstream(method.clone(), &path_and_query)
                .timeout(Duration::from_secs(120));
            if let Some(content_type) = &content_type {
                upstream = upstream.header(header::CONTENT_TYPE, content_type.clone());
            }
            if !body.is_empty() {
                upstream = upstream.body(body.clone());
            }
            match upstream.send().await {
                Ok(response) => break Ok(response.status()),
                Err(error) if attempt < 3 && (error.is_connect() || error.is_request()) => {
                    tokio::time::sleep(Duration::from_millis(250 * attempt)).await;
                }
                Err(error) => break Err(error),
            }
        };
        if let Deferred::Record = kind {
            match outcome {
                Ok(status) if status.is_success() => {
                    task.stats.records.fetch_add(1, Ordering::Relaxed);
                }
                Ok(status) => task.stats.fail(format!(
                    "relay: the server refused a record ({status}); one of its blobs failed to upload"
                )),
                Err(error) => task
                    .stats
                    .fail(format!("relay: record publication failed: {error}")),
            }
        }
        if let Some(sequence) = sequence {
            task.record_order.finish(sequence);
        }
    });
    match kind {
        Deferred::Record => StatusCode::CREATED.into_response(),
        Deferred::Use | Deferred::Release => StatusCode::NO_CONTENT.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_waits_only_for_work_begun_before_the_horizon() {
        let (changed, _) = watch::channel(0);
        let order = Order::new(changed);
        let first = order.begin();
        let horizon = order.horizon();
        let second = order.begin();
        assert!(!order.passed(horizon));
        order.finish(second);
        assert!(!order.passed(horizon), "an earlier item is still open");
        order.finish(first);
        assert!(order.passed(horizon));
        assert!(order.passed(order.horizon()));
    }

    #[test]
    fn relay_rejects_a_wrong_session_token() {
        let shared = Shared {
            client: reqwest::Client::new(),
            upstream: "http://127.0.0.1:1".into(),
            upstream_token: None,
            local_token: "abc123".into(),
            spool_dir: PathBuf::new(),
            spool_limit: 0,
            spooled: AtomicU64::new(0),
            spool_files: AtomicU64::new(0),
            uploads: Semaphore::new(1),
            blobs: Mutex::new(HashMap::new()),
            blob_order: Order::new(watch::channel(0).0),
            record_order: Order::new(watch::channel(0).0),
            tracker: TaskTracker::new(),
            stats: Stats::default(),
        };
        let mut headers = HeaderMap::new();
        assert!(!shared.authorized(&headers));
        headers.insert(header::AUTHORIZATION, "Bearer abc124".parse().unwrap());
        assert!(!shared.authorized(&headers));
        headers.insert(header::AUTHORIZATION, "Bearer abc12".parse().unwrap());
        assert!(!shared.authorized(&headers));
        headers.insert(header::AUTHORIZATION, "Bearer abc123".parse().unwrap());
        assert!(shared.authorized(&headers));
    }
}
