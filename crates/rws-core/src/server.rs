use crate::{
    config::{Config, SiteConfig, SiteMode},
    metrics::Metrics,
    scheduler::{OutboundMessage, SchedulerHandle},
    site_log::{SiteLog, quoted, timestamp_millis},
    static_cache::{CachedAsset, FastLookup, StaticCache},
};
use anyhow::{Context, Result};
use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{ConnectInfo, Request, State, WebSocketUpgrade, ws::Message},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{any, get},
};
use fastcgi_client::{Client, Params, Request as FastCgiRequest, Response as FastCgiResponse, io};
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioTimer;
use percent_encoding::percent_decode_str;
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    borrow::Cow,
    collections::HashMap,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{Arc, atomic::Ordering},
    task::{Context as TaskContext, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
};
use tokio_util::io::ReaderStream;
use tracing::{error, info, warn};

#[derive(Clone)]
struct Site {
    id: usize,
    config: SiteConfig,
    root: Option<PathBuf>,
    access_log: Option<SiteLog>,
    error_log: Option<SiteLog>,
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    sites: Arc<HashMap<String, Site>>,
    metrics: Arc<Metrics>,
    static_cache: Arc<StaticCache>,
    request_permits: Arc<Semaphore>,
    compression_permits: Arc<Semaphore>,
    scheduler: SchedulerHandle,
    proxy_client: Option<reqwest::Client>,
}

pub async fn serve(config: Config) -> Result<()> {
    crate::tls::ensure_crypto_provider();
    let sites = build_sites(&config)?;
    let config = Arc::new(config);
    let metrics = Arc::new(Metrics::default());
    let static_cache = Arc::new(StaticCache::new(
        config.static_cache.clone(),
        Arc::clone(&metrics),
    ));
    let scheduler = SchedulerHandle::start(config.websocket.clone(), Arc::clone(&metrics));
    let state = AppState {
        config: Arc::clone(&config),
        sites: Arc::new(sites),
        metrics: Arc::clone(&metrics),
        static_cache,
        request_permits: Arc::new(Semaphore::new(config.server.max_in_flight_requests)),
        compression_permits: Arc::new(Semaphore::new(config.server.max_in_flight_requests.min(16))),
        scheduler,
        proxy_client: build_proxy_client(&config)?,
    };

    crate::controller::start_if_enabled(Arc::clone(&config), Arc::clone(&metrics));

    let app = public_router(&config, state.clone());
    let metrics_app = Router::new()
        .route("/healthz", get(|| async { (StatusCode::OK, "ok\n") }))
        .route("/metrics", get(metrics_handler))
        .with_state(state.clone());

    let http_listener = bind_std_listener(config.server.listen, config.server.listen_backlog)
        .with_context(|| format!("failed to bind HTTP listener {}", config.server.listen))?;
    let metrics_listener = bind_listener(config.metrics.listen, 128)
        .with_context(|| format!("failed to bind metrics listener {}", config.metrics.listen))?;
    info!(address = %config.server.listen, "HTTP listener started");
    info!(address = %config.metrics.listen, "local metrics listener started");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let metrics_shutdown = shutdown_rx.clone();
    let http_handle = axum_server::Handle::new();
    let mut http_server = axum_server::from_tcp(http_listener)?.handle(http_handle.clone());
    http_server
        .http_builder()
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(config.request_timeout());
    let http_task = tokio::spawn(
        http_server.serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>()),
    );
    let metrics_task = tokio::spawn(async move {
        axum::serve(metrics_listener, metrics_app)
            .with_graceful_shutdown(wait_for_shutdown(metrics_shutdown))
            .await
    });

    let mut tls_handle = None;
    let mut tls_task = None;
    if let Some(tls) = &config.tls {
        let rustls =
            axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert_file, &tls.key_file)
                .await
                .context("failed to load TLS certificate/key")?;
        let handle = axum_server::Handle::new();
        let tls_listener =
            bind_std_listener(config.server.https_listen, config.server.listen_backlog)
                .with_context(|| {
                    format!(
                        "failed to bind HTTPS listener {}",
                        config.server.https_listen
                    )
                })?;
        let mut server = axum_server::from_tcp_rustls(tls_listener, rustls)?.handle(handle.clone());
        server
            .http_builder()
            .http1()
            .timer(TokioTimer::new())
            .header_read_timeout(config.request_timeout());
        let server = server.serve(
            public_router(&config, state.clone())
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        );
        info!(address = %config.server.https_listen, "HTTPS listener started");
        tls_handle = Some(handle);
        tls_task = Some(tokio::spawn(server));
    }

    wait_for_signal().await?;
    info!("shutdown signal received");
    let _ = shutdown_tx.send(true);
    http_handle.graceful_shutdown(Some(Duration::from_secs(
        config.server.shutdown_grace_period_s,
    )));
    if let Some(handle) = tls_handle {
        handle.graceful_shutdown(Some(Duration::from_secs(
            config.server.shutdown_grace_period_s,
        )));
    }
    let grace = Duration::from_secs(config.server.shutdown_grace_period_s);
    let _ = tokio::time::timeout(grace, async {
        let _ = http_task.await;
        let _ = metrics_task.await;
        if let Some(task) = tls_task {
            let _ = task.await;
        }
    })
    .await;
    info!("shutdown complete");
    Ok(())
}

fn build_proxy_client(config: &Config) -> Result<Option<reqwest::Client>> {
    if !config.sites.iter().any(|site| site.mode == SiteMode::Proxy) {
        return Ok(None);
    }
    Ok(Some(
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.request_timeout())
            .build()?,
    ))
}

fn bind_listener(address: std::net::SocketAddr, backlog: u32) -> std::io::Result<TcpListener> {
    TcpListener::from_std(bind_std_listener(address, backlog)?)
}

fn bind_std_listener(
    address: std::net::SocketAddr,
    backlog: u32,
) -> std::io::Result<std::net::TcpListener> {
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    socket.listen(backlog.min(i32::MAX as u32) as i32)?;
    Ok(socket.into())
}

fn public_router(config: &Config, state: AppState) -> Router {
    Router::new()
        .route(&config.websocket.path, get(websocket_upgrade))
        .fallback(any(dispatch_request))
        .with_state(state)
}

fn build_sites(config: &Config) -> Result<HashMap<String, Site>> {
    config
        .sites
        .iter()
        .enumerate()
        .map(|(id, site)| {
            let host = normalize_host(&site.host);
            let root = site
                .root
                .as_ref()
                .map(|path| path.canonicalize())
                .transpose()
                .with_context(|| format!("failed to canonicalize site root for {host}"))?;
            Ok((
                host,
                Site {
                    id,
                    config: site.clone(),
                    root,
                    access_log: site.access_log.as_deref().map(SiteLog::start).transpose()?,
                    error_log: site.error_log.as_deref().map(SiteLog::start).transpose()?,
                },
            ))
        })
        .collect()
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    if !*shutdown.borrow() {
        let _ = shutdown.changed().await;
    }
}

async fn wait_for_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

async fn metrics_handler(State(state): State<AppState>) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
        .into_response()
}

fn add_server_header(response: &mut Response) {
    response
        .headers_mut()
        .insert(header::SERVER, HeaderValue::from_static("RWS"));
}

struct RequestLogInfo {
    method: String,
    uri: String,
    version: String,
    remote: String,
    started: std::time::Instant,
}

impl RequestLogInfo {
    fn capture(site: &Site, request: &Request) -> Option<Self> {
        if site.access_log.is_none() && site.error_log.is_none() {
            return None;
        }
        Some(Self {
            method: request.method().to_string(),
            uri: request
                .uri()
                .path_and_query()
                .map_or("/", |value| value.as_str())
                .to_owned(),
            version: format!("{:?}", request.version()),
            remote: request
                .extensions()
                .get::<ConnectInfo<std::net::SocketAddr>>()
                .map(|ConnectInfo(address)| address.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            started: std::time::Instant::now(),
        })
    }

    fn write(self, site: &Site, response: &Response) {
        let status = response.status().as_u16();
        let bytes = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-");
        let line = format!(
            "{} remote={} host={} method={} uri={} version={} status={} bytes={} duration_us={}\n",
            timestamp_millis(),
            quoted(&self.remote),
            quoted(&site.config.host),
            quoted(&self.method),
            quoted(&self.uri),
            quoted(&self.version),
            status,
            bytes,
            self.started.elapsed().as_micros(),
        );
        if let Some(access_log) = &site.access_log {
            access_log.write(line.clone());
        }
        if status >= 400
            && let Some(error_log) = &site.error_log
        {
            error_log.write(line);
        }
    }
}

fn write_site_error(site: &Site, message: &str) {
    if let Some(error_log) = &site.error_log {
        error_log.write(format!(
            "{} host={} message={}\n",
            timestamp_millis(),
            quoted(&site.config.host),
            quoted(message),
        ));
    }
}

async fn dispatch_request(State(state): State<AppState>, request: Request) -> Response {
    state
        .metrics
        .http_requests_total
        .fetch_add(1, Ordering::Relaxed);
    let host = request_host(&request);
    let Some(site) = host
        .as_ref()
        .and_then(|host| state.sites.get(host.as_ref()))
    else {
        let mut response = counted_error(&state, StatusCode::MISDIRECTED_REQUEST, "unknown host");
        add_server_header(&mut response);
        return response;
    };
    let request_log = RequestLogInfo::capture(site, &request);
    let mut response = match site.config.mode {
        SiteMode::Static => static_or_php(&state, site, request).await,
        SiteMode::Proxy => match state.request_permits.try_acquire() {
            Ok(_permit) => proxy_request(&state, site, request).await,
            Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "server busy").into_response(),
        },
    };
    if response.status().is_client_error() || response.status().is_server_error() {
        state
            .metrics
            .http_errors_total
            .fetch_add(1, Ordering::Relaxed);
    }
    add_server_header(&mut response);
    if let Some(request_log) = request_log {
        request_log.write(site, &response);
    }
    response
}

fn request_host(request: &Request) -> Option<Cow<'_, str>> {
    request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| request.uri().authority().map(|value| value.as_str()))
        .map(normalize_request_host)
}

fn normalize_request_host(host: &str) -> Cow<'_, str> {
    let host = host.trim().trim_end_matches('.');
    let without_port = if let Some(stripped) = host.strip_prefix('[') {
        stripped.split(']').next().unwrap_or(stripped)
    } else {
        host.split(':').next().unwrap_or(host)
    };
    if without_port.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(without_port.to_ascii_lowercase())
    } else {
        Cow::Borrowed(without_port)
    }
}

fn normalize_host(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if let Some(stripped) = host.strip_prefix('[') {
        return stripped.split(']').next().unwrap_or(stripped).to_owned();
    }
    host.split(':').next().unwrap_or(&host).to_owned()
}

async fn static_or_php(state: &AppState, site: &Site, request: Request) -> Response {
    let Some(root) = &site.root else {
        return counted_error(
            state,
            StatusCode::INTERNAL_SERVER_ERROR,
            "site root unavailable",
        );
    };
    let is_head = request.method() == Method::HEAD;
    let can_use_cache = matches!(*request.method(), Method::GET | Method::HEAD);
    let request_path = match validated_request_path(request.uri().path()) {
        Ok(path) => path,
        Err(status) => {
            return (status, status.canonical_reason().unwrap_or("error")).into_response();
        }
    };
    if can_use_cache
        && let FastLookup::Hit(asset) = state.static_cache.fast_lookup(site.id, &request_path)
    {
        let encoding = static_encoding(
            request.headers(),
            site.config.gzip,
            asset.content_type.to_str().unwrap_or(""),
            asset.body.len() as u64,
            false,
        );
        if encoding.unacceptable {
            return encoding_not_acceptable();
        }
        let body = asset.body.clone();
        let modified = asset.modified;
        let response = cached_response(asset, &request, encoding.gzip);
        return encode_static_response(
            response,
            body,
            encoding,
            is_head,
            Arc::clone(&state.compression_permits),
            request.headers(),
            modified,
        )
        .await;
    }
    let _permit = match state.request_permits.try_acquire() {
        Ok(permit) => permit,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "server busy").into_response(),
    };

    let target = match resolve_target(root, &request_path, &site.config.index).await {
        Ok(target) => target,
        Err(StatusCode::NOT_FOUND) => {
            if site.config.directory_listing
                && matches!(*request.method(), Method::GET | Method::HEAD)
            {
                let raw_path = request.uri().path().to_owned();
                let query = request.uri().query().map(str::to_owned);
                match directory_response(root, &request_path, &raw_path, query.as_deref(), is_head)
                    .await
                {
                    Ok(Some(response)) => return response,
                    Err(status) => return status.into_response(),
                    Ok(None) => {}
                }
            }
            let Some(front_controller) = site.config.front_controller.as_deref() else {
                state.static_cache.invalidate(site.id, &request_path);
                return (StatusCode::NOT_FOUND, "not found").into_response();
            };
            state.static_cache.invalidate(site.id, &request_path);
            match resolve_target(root, front_controller, &site.config.index).await {
                Ok(target) => target,
                Err(status) => {
                    return (status, status.canonical_reason().unwrap_or("error")).into_response();
                }
            }
        }
        Err(status) => {
            state.static_cache.invalidate(site.id, &request_path);
            return (status, status.canonical_reason().unwrap_or("error")).into_response();
        }
    };
    if target.path.extension().and_then(|value| value.to_str()) == Some("php") {
        state.static_cache.invalidate(site.id, &request_path);
        if !site.config.php {
            return (StatusCode::NOT_FOUND, "not found").into_response();
        }
        return php_request(state, site, root, target.path, request).await;
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response();
    }
    let mime = mime_guess::from_path(&target.path).first_or_octet_stream();
    let encoding = static_encoding(
        request.headers(),
        site.config.gzip,
        mime.as_ref(),
        target.metadata.len(),
        state.static_cache.should_stream(target.metadata.len()),
    );
    if encoding.unacceptable {
        return encoding_not_acceptable();
    }
    let streamed = state.static_cache.should_stream(target.metadata.len());
    let stream_tag = streamed.then(|| streamed_etag(&target.metadata)).flatten();
    if streamed
        && let Some(mut response) = static_precondition_response(
            request.headers(),
            target.metadata.modified().ok(),
            stream_tag.as_ref(),
        )
    {
        if encoding.varies {
            response
                .headers_mut()
                .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        }
        return response;
    }
    let (body, length) = if is_head && !encoding.gzip && streamed {
        state
            .static_cache
            .validate_metadata(site.id, &request_path, &target.metadata);
        (None::<Bytes>, target.metadata.len())
    } else {
        if streamed {
            state.static_cache.record_bypass();
            let range = requested_range(
                &request,
                target.metadata.len(),
                stream_tag.as_ref(),
                target.metadata.modified().ok(),
            );
            return match streamed_file_response(
                target.file,
                &target.path,
                &target.metadata,
                range,
                stream_tag.as_ref(),
            )
            .await
            {
                Ok(response) => response,
                Err(error) => static_read_error(site, &target.path, error),
            };
        }
        match state
            .static_cache
            .get_or_load(
                site.id,
                &request_path,
                &target.path,
                target.file,
                &target.metadata,
            )
            .await
        {
            Ok(asset) => {
                let body = asset.body.clone();
                let modified = asset.modified;
                let response = cached_response(asset, &request, encoding.gzip);
                return encode_static_response(
                    response,
                    body,
                    encoding,
                    is_head,
                    Arc::clone(&state.compression_permits),
                    request.headers(),
                    modified,
                )
                .await;
            }
            Err(error) => {
                return static_read_error(site, &target.path, error);
            }
        }
    };
    let mut response = Response::new(body.map_or_else(Body::empty, Body::from));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).expect("numeric content length"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    add_static_headers(&mut response, target.metadata.modified().ok());
    if let Some(tag) = stream_tag {
        response.headers_mut().insert(header::ETAG, tag);
    }
    if encoding.varies {
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    response
}

async fn streamed_file_response(
    mut file: tokio::fs::File,
    path: &Path,
    metadata: &std::fs::Metadata,
    range: ByteRange,
    etag: Option<&HeaderValue>,
) -> std::io::Result<Response> {
    let full_length = metadata.len();
    let modified = metadata.modified().ok();
    if matches!(range, ByteRange::Unsatisfiable) {
        let mut response = range_not_satisfiable(full_length, modified);
        if let Some(tag) = etag {
            response.headers_mut().insert(header::ETAG, tag.clone());
        }
        return Ok(response);
    }
    if let ByteRange::Multiple(ranges) = &range {
        let mut parts = Vec::with_capacity(ranges.len());
        for &(start, end) in ranges {
            file.seek(std::io::SeekFrom::Start(start)).await?;
            let mut bytes = vec![0; (end - start + 1) as usize];
            file.read_exact(&mut bytes).await?;
            parts.push(Bytes::from(bytes));
        }
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let mut response = multipart_response(ranges, &parts, mime.as_ref(), full_length, modified);
        if let Some(tag) = etag {
            response.headers_mut().insert(header::ETAG, tag.clone());
        }
        return Ok(response);
    }
    let (length, partial) = match range {
        ByteRange::Selected(start, end) => {
            file.seek(std::io::SeekFrom::Start(start)).await?;
            (end - start + 1, Some((start, end)))
        }
        _ => (full_length, None),
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file.take(length))));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).expect("numeric content length"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    add_static_headers(&mut response, modified);
    if let Some(tag) = etag {
        response.headers_mut().insert(header::ETAG, tag.clone());
    }
    if let Some((start, end)) = partial {
        set_content_range(&mut response, start, end, full_length);
    }
    Ok(response)
}

#[cfg(target_os = "linux")]
fn streamed_etag(metadata: &std::fs::Metadata) -> Option<HeaderValue> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    let fingerprint = format!(
        "{}:{}:{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec()
    );
    HeaderValue::from_str(&format!(
        "W/\"{:x}\"",
        Sha256::digest(fingerprint.as_bytes())
    ))
    .ok()
}

#[cfg(not(target_os = "linux"))]
fn streamed_etag(_metadata: &std::fs::Metadata) -> Option<HeaderValue> {
    None
}

fn cached_response(asset: CachedAsset, request: &Request, ignore_range: bool) -> Response {
    let etag = (!ignore_range).then_some(&asset.etag);
    if !ignore_range
        && let Some(response) =
            static_precondition_response(request.headers(), asset.modified, etag)
    {
        return response;
    }
    let full_length = asset.body.len() as u64;
    let range = if ignore_range {
        ByteRange::Ignore
    } else {
        requested_range(request, full_length, etag, asset.modified)
    };
    if matches!(range, ByteRange::Unsatisfiable) {
        let mut response = range_not_satisfiable(full_length, asset.modified);
        if let Some(tag) = etag {
            response.headers_mut().insert(header::ETAG, tag.clone());
        }
        return response;
    }
    if let ByteRange::Multiple(ranges) = &range {
        let parts: Vec<_> = ranges
            .iter()
            .map(|&(start, end)| asset.body.slice(start as usize..=end as usize))
            .collect();
        let mut response = multipart_response(
            ranges,
            &parts,
            asset
                .content_type
                .to_str()
                .unwrap_or("application/octet-stream"),
            full_length,
            asset.modified,
        );
        if let Some(tag) = etag {
            response.headers_mut().insert(header::ETAG, tag.clone());
        }
        return response;
    }
    let is_head = request.method() == Method::HEAD;
    let (body, partial) = match range {
        ByteRange::Selected(start, end) => (
            asset.body.slice(start as usize..=end as usize),
            Some((start, end)),
        ),
        _ => (asset.body, None),
    };
    let mut response = Response::new(if is_head {
        Body::empty()
    } else {
        Body::from(body)
    });
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, asset.content_type);
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, asset.content_length);
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    add_static_headers(&mut response, asset.modified);
    if let Some(tag) = etag {
        response.headers_mut().insert(header::ETAG, tag.clone());
    }
    if let Some((start, end)) = partial {
        response.headers_mut().insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&(end - start + 1).to_string()).expect("numeric length"),
        );
        set_content_range(&mut response, start, end, full_length);
    }
    response
}

#[derive(Clone, Copy)]
struct StaticEncoding {
    varies: bool,
    gzip: bool,
    unacceptable: bool,
}

fn encoding_not_acceptable() -> Response {
    let mut response = StatusCode::NOT_ACCEPTABLE.into_response();
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    response
}

fn static_encoding(
    headers: &HeaderMap,
    enabled: bool,
    mime: &str,
    length: u64,
    streaming: bool,
) -> StaticEncoding {
    if !enabled {
        return StaticEncoding {
            varies: false,
            gzip: false,
            unacceptable: false,
        };
    }
    let compressible = mime.starts_with("text/")
        || matches!(
            mime,
            "application/javascript" | "application/json" | "application/xml" | "image/svg+xml"
        );
    let varies = compressible && !streaming && length <= 1024 * 1024;
    let mut gzip_quality = None;
    let mut identity_quality = None;
    let mut wildcard_quality = None;
    for value in headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
    {
        for item in value.split(',') {
            let mut parts = item.trim().split(';');
            let name = parts.next().unwrap_or_default().trim();
            let mut quality = 1.0_f32;
            for parameter in parts {
                if let Some((key, value)) = parameter.trim().split_once('=')
                    && key.trim().eq_ignore_ascii_case("q")
                {
                    quality = value
                        .trim()
                        .parse::<f32>()
                        .ok()
                        .filter(|q| (0.0..=1.0).contains(q))
                        .unwrap_or(0.0);
                }
            }
            if name.eq_ignore_ascii_case("gzip") {
                gzip_quality = Some(quality);
            } else if name.eq_ignore_ascii_case("identity") {
                identity_quality = Some(quality);
            } else if name == "*" {
                wildcard_quality = Some(quality);
            }
        }
    }
    let identity = identity_quality.unwrap_or(if wildcard_quality == Some(0.0) {
        0.0
    } else {
        1.0
    }) > 0.0;
    let gzip_weight = gzip_quality.or(wildcard_quality).unwrap_or(0.0);
    let gzip = varies
        && gzip_weight > 0.0
        && identity_quality.is_none_or(|identity_weight| gzip_weight >= identity_weight);
    // Prefer identity for byte-range requests. If identity was explicitly forbidden, gzip the
    // complete representation instead of applying a byte range to the wrong representation.
    let gzip = gzip && (!headers.contains_key(header::RANGE) || !identity);
    StaticEncoding {
        varies,
        gzip,
        unacceptable: !identity && !gzip,
    }
}

async fn encode_static_response(
    mut response: Response,
    body: Bytes,
    encoding: StaticEncoding,
    is_head: bool,
    permits: Arc<Semaphore>,
    request_headers: &HeaderMap,
    modified: Option<SystemTime>,
) -> Response {
    if encoding.varies {
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    if !encoding.gzip || response.status() != StatusCode::OK {
        return response;
    }
    let Ok(permit) = permits.try_acquire_owned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "compression busy").into_response();
    };
    let compressed = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let _permit = permit;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&body)?;
        encoder.finish()
    })
    .await;
    let Ok(Ok(compressed)) = compressed else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    use sha2::{Digest, Sha256};
    let etag = HeaderValue::from_str(&format!("\"{:x}\"", Sha256::digest(&compressed)))
        .expect("hex digest");
    if let Some(mut precondition) =
        static_precondition_response(request_headers, modified, Some(&etag))
    {
        precondition
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        return precondition;
    }
    response.headers_mut().insert(header::ETAG, etag);
    response
        .headers_mut()
        .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&compressed.len().to_string()).expect("numeric length"),
    );
    *response.body_mut() = if is_head {
        Body::empty()
    } else {
        Body::from(compressed)
    };
    response
}

#[derive(Debug, PartialEq, Eq)]
enum ByteRange {
    Ignore,
    Unsatisfiable,
    Selected(u64, u64),
    Multiple(Vec<(u64, u64)>),
}

// Bound multipart buffering and reject excessive range counts by ignoring the Range header.
fn requested_range(
    request: &Request,
    length: u64,
    etag: Option<&HeaderValue>,
    modified: Option<SystemTime>,
) -> ByteRange {
    if request.method() != Method::GET {
        return ByteRange::Ignore;
    }
    if request.headers().contains_key(header::IF_RANGE) {
        let mut values = request.headers().get_all(header::IF_RANGE).iter();
        let valid = match (values.next(), values.next()) {
            (Some(value), None) => {
                etag.is_some_and(|tag| !tag.as_bytes().starts_with(b"W/") && value == tag)
                    || value
                        .to_str()
                        .ok()
                        .and_then(|value| httpdate::parse_http_date(value).ok())
                        .zip(modified)
                        .is_some_and(|(date, modified)| {
                            date_at_or_before(modified, date)
                                && date_at_or_before(date, modified)
                                && SystemTime::now()
                                    .duration_since(modified)
                                    .is_ok_and(|age| age >= Duration::from_secs(60))
                        })
            }
            _ => false,
        };
        if !valid {
            return ByteRange::Ignore;
        }
    }
    let mut headers = request.headers().get_all(header::RANGE).iter();
    let Some(value) = headers.next().and_then(|value| value.to_str().ok()) else {
        return ByteRange::Ignore;
    };
    if headers.next().is_some() {
        return ByteRange::Ignore;
    }
    let Some((unit, spec)) = value.split_once('=') else {
        return ByteRange::Ignore;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return ByteRange::Ignore;
    }
    let parts: Vec<_> = spec.split(',').collect();
    if parts.len() > 16 || parts.iter().any(|part| part.trim().is_empty()) {
        return ByteRange::Ignore;
    }
    let mut ranges = Vec::new();
    for part in parts {
        match parse_byte_range(part.trim(), length) {
            ByteRange::Selected(start, end) => ranges.push((start, end)),
            ByteRange::Unsatisfiable => {}
            _ => return ByteRange::Ignore,
        }
    }
    if ranges.is_empty() {
        return ByteRange::Unsatisfiable;
    }
    if ranges.len() == 1 {
        return ByteRange::Selected(ranges[0].0, ranges[0].1);
    }
    if ranges
        .iter()
        .try_fold(0_u64, |sum, (start, end)| sum.checked_add(end - start + 1))
        .is_none_or(|size| size > 1024 * 1024)
    {
        return ByteRange::Ignore;
    }
    ByteRange::Multiple(ranges)
}

fn parse_byte_range(spec: &str, length: u64) -> ByteRange {
    fn decimal(value: &str) -> Option<u64> {
        (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| value.parse().ok())
            .flatten()
    }
    let Some((start, end)) = spec.trim().split_once('-') else {
        return ByteRange::Ignore;
    };
    if start.is_empty() {
        let Some(suffix) = decimal(end) else {
            return ByteRange::Ignore;
        };
        if suffix == 0 || length == 0 {
            return ByteRange::Unsatisfiable;
        }
        return ByteRange::Selected(length.saturating_sub(suffix), length - 1);
    }
    let Some(start) = decimal(start) else {
        return ByteRange::Ignore;
    };
    let end = if end.is_empty() {
        u64::MAX
    } else {
        let Some(end) = decimal(end) else {
            return ByteRange::Ignore;
        };
        if end < start {
            return ByteRange::Ignore;
        }
        end
    };
    if start >= length {
        return ByteRange::Unsatisfiable;
    }
    ByteRange::Selected(start, end.min(length - 1))
}

fn add_static_headers(response: &mut Response, modified: Option<SystemTime>) {
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(modified) = modified.filter(|time| {
        *time <= SystemTime::now()
            && time
                .duration_since(UNIX_EPOCH)
                .is_ok_and(|value| value.as_secs() < 253_402_300_800)
    }) && let Ok(value) = HeaderValue::from_str(&httpdate::fmt_http_date(modified))
    {
        response.headers_mut().insert(header::LAST_MODIFIED, value);
    }
}

fn date_at_or_before(modified: SystemTime, date: SystemTime) -> bool {
    match (
        modified.duration_since(UNIX_EPOCH),
        date.duration_since(UNIX_EPOCH),
    ) {
        (Ok(modified), Ok(date)) => modified.as_secs() <= date.as_secs(),
        _ => false,
    }
}

fn static_precondition_response(
    headers: &HeaderMap,
    modified: Option<SystemTime>,
    etag: Option<&HeaderValue>,
) -> Option<Response> {
    let modified = modified.filter(|time| *time <= SystemTime::now());
    if headers.contains_key(header::IF_MATCH) {
        let matches = headers
            .get_all(header::IF_MATCH)
            .iter()
            .any(|value| entity_tag_matches(value, etag, false));
        if !matches {
            let mut response = StatusCode::PRECONDITION_FAILED.into_response();
            add_static_headers(&mut response, modified);
            if let Some(tag) = etag {
                response.headers_mut().insert(header::ETAG, tag.clone());
            }
            return Some(response);
        }
    } else if let (Some(modified), Some(date)) = (
        modified,
        headers
            .get(header::IF_UNMODIFIED_SINCE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| httpdate::parse_http_date(v).ok()),
    ) && !date_at_or_before(modified, date)
    {
        let mut response = StatusCode::PRECONDITION_FAILED.into_response();
        add_static_headers(&mut response, Some(modified));
        if let Some(tag) = etag {
            response.headers_mut().insert(header::ETAG, tag.clone());
        }
        return Some(response);
    }
    let unchanged = if headers.contains_key(header::IF_NONE_MATCH) {
        headers
            .get_all(header::IF_NONE_MATCH)
            .iter()
            .any(|value| entity_tag_matches(value, etag, true))
    } else {
        modified
            .zip(
                headers
                    .get(header::IF_MODIFIED_SINCE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| httpdate::parse_http_date(v).ok()),
            )
            .is_some_and(|(modified, date)| date_at_or_before(modified, date))
    };
    if unchanged {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        add_static_headers(&mut response, modified);
        if let Some(tag) = etag {
            response.headers_mut().insert(header::ETAG, tag.clone());
        }
        Some(response)
    } else {
        None
    }
}

fn entity_tag_matches(value: &HeaderValue, etag: Option<&HeaderValue>, weak: bool) -> bool {
    let Ok(value) = value.to_str() else {
        return false;
    };
    if value.trim() == "*" {
        return true;
    }
    let Some(tag) = etag.and_then(|tag| tag.to_str().ok()) else {
        return false;
    };
    // Quoted opaque tags may contain commas; split only outside quotes.
    let mut quoted = false;
    let mut start = 0;
    for (index, byte) in value
        .bytes()
        .enumerate()
        .chain(std::iter::once((value.len(), b',')))
    {
        if byte == b'"' {
            quoted = !quoted;
        }
        if byte == b',' && !quoted {
            let item = value[start..index].trim();
            let candidate = item.strip_prefix("W/").unwrap_or(item);
            let current = tag.strip_prefix("W/").unwrap_or(tag);
            if (weak && candidate == current)
                || (!weak && !item.starts_with("W/") && !tag.starts_with("W/") && item == tag)
            {
                return true;
            }
            start = index + 1;
        }
    }
    false
}

fn set_content_range(response: &mut Response, start: u64, end: u64, length: u64) {
    *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    response.headers_mut().insert(
        header::CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes {start}-{end}/{length}")).expect("numeric range"),
    );
}

fn multipart_response(
    ranges: &[(u64, u64)],
    parts: &[Bytes],
    mime: &str,
    length: u64,
    modified: Option<SystemTime>,
) -> Response {
    // Ensure the chosen delimiter does not appear in any selected bytes.
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part);
    }
    let digest = format!("{:x}", hash.finalize());
    let mut n = 0_u64;
    let boundary = loop {
        let candidate = format!("rws-{}-{n:x}", &digest[..32]);
        if parts.iter().all(|part| {
            !part
                .windows(candidate.len())
                .any(|window| window == candidate.as_bytes())
        }) {
            break candidate;
        }
        n += 1;
    };
    let mut body = Vec::new();
    for ((start, end), part) in ranges.iter().zip(parts) {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Type: {mime}\r\nContent-Range: bytes {start}-{end}/{length}\r\n\r\n").as_bytes());
        body.extend_from_slice(part);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let body_length = body.len();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&format!("multipart/byteranges; boundary={boundary}"))
            .expect("valid boundary"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&body_length.to_string()).expect("numeric length"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    add_static_headers(&mut response, modified);
    response
}

fn range_not_satisfiable(length: u64, modified: Option<SystemTime>) -> Response {
    let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    response.headers_mut().insert(
        header::CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes */{length}")).expect("numeric length"),
    );
    add_static_headers(&mut response, modified);
    response
}

#[cfg(target_os = "linux")]
async fn directory_response(
    root: &Path,
    decoded_path: &str,
    raw_path: &str,
    query: Option<&str>,
    is_head: bool,
) -> Result<Option<Response>, StatusCode> {
    use std::os::fd::AsRawFd;

    let relative = decoded_path.trim_start_matches('/');
    let relative = if relative.is_empty() { "." } else { relative };
    let Ok(directory) = open_contained(root, Path::new(relative), true).await else {
        return Ok(None);
    };
    if !directory
        .metadata()
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?
        .is_dir()
    {
        return Ok(None);
    }
    if !raw_path.ends_with('/') {
        // A Location beginning with two slashes is a network-path reference, not a local path.
        if !raw_path.starts_with('/') || raw_path.starts_with("//") {
            return Err(StatusCode::BAD_REQUEST);
        }
        let mut location = format!("{raw_path}/");
        if let Some(query) = query {
            location.push('?');
            location.push_str(query);
        }
        let mut response = StatusCode::PERMANENT_REDIRECT.into_response();
        response.headers_mut().insert(
            header::LOCATION,
            HeaderValue::from_str(&location).map_err(|_| StatusCode::BAD_REQUEST)?,
        );
        return Ok(Some(response));
    }

    let directory = directory.into_std().await;
    let entries = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<(String, bool)>> {
        // openat2 already pinned the directory under the trusted root; keep that descriptor open
        // while enumerating it instead of reopening the original, raceable request path.
        let fd_path = format!("/proc/self/fd/{}", directory.as_raw_fd());
        let mut entries = Vec::new();
        let mut names_bytes = 0usize;
        let mut examined = 0usize;
        for entry in std::fs::read_dir(fd_path)? {
            examined += 1;
            if examined > 4096 {
                return Err(std::io::Error::other("directory scan exceeds limits"));
            }
            let entry = entry?;
            let kind = entry.file_type()?;
            if !kind.is_file() && !kind.is_dir() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            names_bytes = names_bytes.saturating_add(name.len());
            if entries.len() >= 1024 || names_bytes > 256 * 1024 {
                return Err(std::io::Error::other("directory listing exceeds limits"));
            }
            entries.push((name, kind.is_dir()));
        }
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::FORBIDDEN)?;

    fn escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;")
    }
    let title = escape(decoded_path);
    let mut html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Index of {title}</title></head><body><h1>Index of {title}</h1><ul>"
    );
    for (name, is_dir) in entries {
        let encoded =
            percent_encoding::utf8_percent_encode(&name, percent_encoding::NON_ALPHANUMERIC);
        let suffix = if is_dir { "/" } else { "" };
        html.push_str(&format!(
            "<li><a href=\"{encoded}{suffix}\">{}{suffix}</a></li>",
            escape(&name)
        ));
    }
    html.push_str("</ul></body></html>");
    let length = html.len();
    let mut response = Response::new(if is_head {
        Body::empty()
    } else {
        Body::from(html)
    });
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).expect("numeric length"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; base-uri 'none'; form-action 'none'"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(Some(response))
}

#[cfg(not(target_os = "linux"))]
async fn directory_response(
    _root: &Path,
    _decoded_path: &str,
    _raw_path: &str,
    _query: Option<&str>,
    _is_head: bool,
) -> Result<Option<Response>, StatusCode> {
    // Race-resistant directory enumeration is currently Linux-only.
    Ok(None)
}

struct ResolvedTarget {
    path: PathBuf,
    metadata: std::fs::Metadata,
    file: tokio::fs::File,
}

fn static_read_error(site: &Site, target: &Path, error: std::io::Error) -> Response {
    warn!(path = %target.display(), %error, "failed to read static file");
    write_site_error(
        site,
        &format!("failed to read static file {}: {error}", target.display()),
    );
    (StatusCode::NOT_FOUND, "not found").into_response()
}

async fn resolve_target(
    root: &Path,
    request_path: &str,
    index: &str,
) -> Result<ResolvedTarget, StatusCode> {
    let mut relative = PathBuf::new();
    for component in Path::new(request_path.trim_start_matches('/')).components() {
        match component {
            Component::Normal(segment) => relative.push(segment),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(StatusCode::FORBIDDEN);
            }
        }
    }
    if relative.as_os_str().is_empty() {
        relative.push(index);
    }
    let target = root.join(relative);
    let mut canonical = tokio::fs::canonicalize(&target)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let mut metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    if metadata.is_dir() {
        canonical = tokio::fs::canonicalize(canonical.join(index))
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
        metadata = tokio::fs::metadata(&canonical)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
    }
    if !canonical.starts_with(root) || !metadata.is_file() {
        return Err(StatusCode::FORBIDDEN);
    }
    let relative = canonical
        .strip_prefix(root)
        .map_err(|_| StatusCode::FORBIDDEN)?;
    let is_php = canonical.extension().and_then(|value| value.to_str()) == Some("php");
    let file = open_contained(root, relative, !is_php)
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?;
    let metadata = file.metadata().await.map_err(|_| StatusCode::NOT_FOUND)?;
    if !metadata.is_file() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(ResolvedTarget {
        path: canonical,
        metadata,
        file,
    })
}

#[cfg(target_os = "linux")]
async fn open_contained(
    root: &Path,
    relative: &Path,
    readable: bool,
) -> std::io::Result<tokio::fs::File> {
    let root = root.to_owned();
    let relative = relative.to_owned();
    let file = tokio::task::spawn_blocking(move || {
        use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
        let directory = std::fs::File::open(root)?;
        let flags = if readable {
            OFlags::RDONLY | OFlags::NONBLOCK
        } else {
            OFlags::PATH
        };
        let fd = openat2(
            &directory,
            relative.as_path(),
            flags | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
        )?;
        Ok::<std::fs::File, std::io::Error>(fd.into())
    })
    .await
    .map_err(std::io::Error::other)??;
    Ok(tokio::fs::File::from_std(file))
}

#[cfg(not(target_os = "linux"))]
async fn open_contained(
    root: &Path,
    relative: &Path,
    _readable: bool,
) -> std::io::Result<tokio::fs::File> {
    tokio::fs::File::open(root.join(relative)).await
}

fn validated_request_path(request_path: &str) -> Result<std::borrow::Cow<'_, str>, StatusCode> {
    let decoded = percent_decode_str(request_path)
        .decode_utf8()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    if decoded.contains('\0') || decoded.contains('\\') {
        return Err(StatusCode::BAD_REQUEST);
    }
    for component in Path::new(decoded.trim_start_matches('/')).components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    Ok(decoded)
}

async fn php_request(
    state: &AppState,
    site: &Site,
    root: &Path,
    script: PathBuf,
    request: Request,
) -> Response {
    let endpoint = site.config.php_fpm.as_deref().unwrap_or_default();
    let timeout = state.config.request_timeout();
    let remote = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|ConnectInfo(address)| *address);
    let (parts, body) = request.into_parts();
    let body =
        match tokio::time::timeout(timeout, to_bytes(body, state.config.server.max_body_bytes))
            .await
        {
            Ok(Ok(body)) => body,
            Ok(Err(_)) => {
                return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response();
            }
            Err(_) => return (StatusCode::REQUEST_TIMEOUT, "request body timeout").into_response(),
        };
    let future = async {
        let content_type = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let script_name = script
            .strip_prefix(root)
            .map(|path| format!("/{}", path.to_string_lossy().replace('\\', "/")))
            .map_err(|_| "PHP script is outside the site root".to_owned())?;
        let mut extras = Params::default()
            .document_root(root.to_string_lossy().into_owned())
            .script_filename(script.to_string_lossy().into_owned())
            .script_name(script_name.clone())
            .document_uri(script_name)
            .server_addr(state.config.server.listen.ip().to_string())
            .server_port(state.config.server.listen.port())
            .server_name(site.config.host.clone())
            .content_type(content_type)
            .content_length(body.len());
        if let Some(remote) = remote {
            extras = extras
                .remote_addr(remote.ip().to_string())
                .remote_port(remote.port());
        }
        let http_request = http::Request::from_parts(parts, io::Cursor::new(body.to_vec()));
        let fastcgi = FastCgiRequest::try_from_http_with(http_request, extras)
            .map_err(|error| format!("FastCGI request conversion failed: {error}"))?;
        if let Some(address) = endpoint.strip_prefix("tcp:") {
            let stream = TcpStream::connect(address)
                .await
                .map_err(|error| format!("PHP-FPM connection failed: {error}"))?;
            execute_fastcgi(
                stream,
                fastcgi,
                state.config.server.max_fastcgi_response_bytes,
            )
            .await
        } else if endpoint.starts_with("unix:") {
            #[cfg(unix)]
            {
                let path = endpoint.trim_start_matches("unix:");
                let stream = tokio::net::UnixStream::connect(path)
                    .await
                    .map_err(|error| format!("PHP-FPM connection failed: {error}"))?;
                execute_fastcgi(
                    stream,
                    fastcgi,
                    state.config.server.max_fastcgi_response_bytes,
                )
                .await
            }
            #[cfg(not(unix))]
            Err("Unix PHP-FPM sockets require Linux/Unix".to_owned())
        } else {
            Err("php_fpm must start with tcp: or unix:".to_owned())
        }
    };
    match tokio::time::timeout(timeout, future).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            error!(%error, "PHP-FPM request failed");
            write_site_error(site, &format!("PHP-FPM request failed: {error}"));
            (StatusCode::BAD_GATEWAY, "PHP-FPM request failed").into_response()
        }
        Err(_) => {
            write_site_error(site, "PHP-FPM request timed out");
            (StatusCode::GATEWAY_TIMEOUT, "PHP-FPM timeout").into_response()
        }
    }
}

async fn execute_fastcgi<S>(
    stream: S,
    request: FastCgiRequest<'static, io::Cursor<Vec<u8>>>,
    max_response_bytes: usize,
) -> Result<Response, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let client = Client::new_tokio(LimitedFastcgiStream {
        inner: stream,
        remaining: max_response_bytes,
    });
    let response = client
        .execute_once(request)
        .await
        .map_err(|error| format!("FastCGI execution failed: {error}"))?;
    if let Some(stderr) = response.stderr.as_ref().filter(|value| !value.is_empty()) {
        warn!(stderr = %String::from_utf8_lossy(stderr), "PHP-FPM stderr");
    }
    fastcgi_response(response)
}

// Limit all FastCGI response bytes, including record framing, headers and stderr.
struct LimitedFastcgiStream<S> {
    inner: S,
    remaining: usize,
}

impl<S: AsyncRead + Unpin> AsyncRead for LimitedFastcgiStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if self.remaining == 0 {
            return Poll::Ready(Err(std::io::Error::other(
                "FastCGI response exceeds configured limit",
            )));
        }
        let mut storage = [0u8; 8192];
        let limit = storage.len().min(buf.remaining()).min(self.remaining);
        let mut limited = ReadBuf::new(&mut storage[..limit]);
        match Pin::new(&mut self.inner).poll_read(cx, &mut limited) {
            Poll::Ready(Ok(())) => {
                let read = limited.filled().len();
                self.remaining -= read;
                buf.put_slice(limited.filled());
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for LimitedFastcgiStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn fastcgi_response(response: FastCgiResponse) -> Result<Response, String> {
    let response: http::Response<Vec<u8>> = response
        .try_into()
        .map_err(|error| format!("invalid FastCGI response: {error}"))?;
    let (parts, body) = response.into_parts();
    Ok(Response::from_parts(parts, Body::from(body)))
}

async fn websocket_upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(normalize_request_host);
    if host
        .as_ref()
        .is_none_or(|host| !state.sites.contains_key(host.as_ref()))
    {
        return counted_error(&state, StatusCode::MISDIRECTED_REQUEST, "unknown host");
    }
    let mut response = upgrade.on_upgrade(move |socket| websocket_session(state, socket));
    add_server_header(&mut response);
    response
}

async fn websocket_session(state: AppState, socket: axum::extract::ws::WebSocket) {
    state
        .metrics
        .open_websockets
        .fetch_add(1, Ordering::Relaxed);
    let (writer_tx, mut writer_rx) = state.scheduler.writer_channel();
    let id = match state.scheduler.register(writer_tx).await {
        Ok(id) => id,
        Err(_) => {
            state
                .metrics
                .open_websockets
                .fetch_sub(1, Ordering::Relaxed);
            return;
        }
    };
    let (mut sink, mut source) = socket.split();
    let writer = tokio::spawn(async move {
        while let Some(message) = writer_rx.recv().await {
            if sink.send(message.into_axum()).await.is_err() {
                break;
            }
        }
    });
    while let Some(result) = source.next().await {
        let message = match result {
            Ok(message) => message,
            Err(_) => break,
        };
        state
            .metrics
            .websocket_messages_in_total
            .fetch_add(1, Ordering::Relaxed);
        let outbound = match message {
            Message::Text(text) => OutboundMessage::Text(Bytes::copy_from_slice(text.as_bytes())),
            Message::Binary(bytes) => OutboundMessage::Binary(bytes),
            Message::Ping(bytes) => OutboundMessage::Pong(bytes),
            Message::Pong(_) => continue,
            Message::Close(_) => break,
        };
        if state.scheduler.enqueue(id, outbound).await.is_err() {
            break;
        }
    }
    state.scheduler.unregister(id).await;
    writer.abort();
    let _ = writer.await;
    state
        .metrics
        .open_websockets
        .fetch_sub(1, Ordering::Relaxed);
}

async fn proxy_request(state: &AppState, site: &Site, request: Request) -> Response {
    let Some(upstream) = site.config.upstream.as_deref() else {
        return counted_error(state, StatusCode::BAD_GATEWAY, "upstream missing");
    };
    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let url = format!("{}{}", upstream.trim_end_matches('/'), path);
    let (parts, body) = request.into_parts();
    let body = match tokio::time::timeout(
        state.config.request_timeout(),
        to_bytes(body, state.config.server.max_body_bytes),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(_)) => {
            return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response();
        }
        Err(_) => return (StatusCode::REQUEST_TIMEOUT, "request body timeout").into_response(),
    };
    let Some(client) = &state.proxy_client else {
        return counted_error(state, StatusCode::BAD_GATEWAY, "proxy client unavailable");
    };
    let mut outgoing = client.request(parts.method, url).body(body);
    for (name, value) in &parts.headers {
        if !is_hop_by_hop(name)
            && !connection_names_header(&parts.headers, name)
            && *name != header::HOST
            && !is_untrusted_forwarding_header(name)
        {
            outgoing = outgoing.header(name, value);
        }
    }
    let upstream_response = match outgoing.send().await {
        Ok(response) => response,
        Err(error) => {
            warn!(%error, "upstream request failed");
            write_site_error(site, &format!("upstream request failed: {error}"));
            return (StatusCode::BAD_GATEWAY, "upstream request failed").into_response();
        }
    };
    let status = upstream_response.status();
    let headers = upstream_response.headers().clone();
    let limit = state.config.server.max_proxy_response_bytes;
    if upstream_response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return (StatusCode::BAD_GATEWAY, "upstream response too large").into_response();
    }
    let mut stream = upstream_response.bytes_stream();
    let mut body = bytes::BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                return (StatusCode::BAD_GATEWAY, "invalid upstream response").into_response();
            }
        };
        if chunk.len() > limit.saturating_sub(body.len()) {
            return (StatusCode::BAD_GATEWAY, "upstream response too large").into_response();
        }
        body.extend_from_slice(&chunk);
    }
    let mut response = Response::new(Body::from(body.freeze()));
    *response.status_mut() = status;
    for (name, value) in &headers {
        if !is_hop_by_hop(name) && !connection_names_header(&headers, name) {
            response.headers_mut().append(name, value.clone());
        }
    }
    response
}

fn connection_names_header(headers: &HeaderMap, name: &axum::http::HeaderName) -> bool {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case(name.as_str()))
}

fn is_untrusted_forwarding_header(name: &axum::http::HeaderName) -> bool {
    name.as_str().starts_with("x-forwarded-") || matches!(name.as_str(), "forwarded" | "x-real-ip")
}

fn is_hop_by_hop(name: &axum::http::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn counted_error(state: &AppState, status: StatusCode, message: &'static str) -> Response {
    state
        .metrics
        .http_errors_total
        .fetch_add(1, Ordering::Relaxed);
    (status, message).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn proxy_limits_upstream_responses_and_strips_untrusted_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().fallback(any(|headers: HeaderMap| async move {
            if headers.contains_key("x-forwarded-for") || headers.contains_key("x-private") {
                return (StatusCode::INTERNAL_SERVER_ERROR, "forwarded").into_response();
            }
            let body = Body::from_stream(futures_util::stream::iter([
                Ok::<_, std::io::Error>(Bytes::from_static(b"123")),
                Ok(Bytes::from_static(b"456")),
            ]));
            let mut response = Response::new(body);
            response
                .headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static("x-secret"));
            response
                .headers_mut()
                .insert("x-secret", HeaderValue::from_static("hidden"));
            response
        }));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config: Config = toml::from_str(&format!(
            "[server]\nmax_proxy_response_bytes=5\nmax_body_bytes=4\n[[sites]]\nhost='example.test'\nmode='proxy'\nupstream='http://{addr}'\n"
        )).unwrap();
        config.validate().unwrap();
        let state = test_state(config);
        let site = state.sites.get("example.test").unwrap();
        let oversized = Request::builder()
            .method("POST")
            .uri("/")
            .body(Body::from("12345"))
            .unwrap();
        assert_eq!(
            proxy_request(&state, site, oversized).await.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let request = || {
            Request::builder()
                .uri("/")
                .header("x-forwarded-for", "spoofed")
                .header(header::CONNECTION, "x-private")
                .header("x-private", "secret")
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            proxy_request(&state, site, request()).await.status(),
            StatusCode::BAD_GATEWAY
        );

        let mut config = (*state.config).clone();
        config.server.max_proxy_response_bytes = 6;
        let state = test_state(config);
        let site = state.sites.get("example.test").unwrap();
        let response = proxy_request(&state, site, request()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key("x-secret"));
        assert_eq!(to_bytes(response.into_body(), 6).await.unwrap(), "123456");
        task.abort();
    }

    #[tokio::test]
    async fn proxy_times_out_slow_request_body() {
        let config: Config = toml::from_str(
            "[server]\nrequest_timeout_ms=20\n[[sites]]\nhost='example.test'\nmode='proxy'\nupstream='http://127.0.0.1:9'\n",
        ).unwrap();
        let state = test_state(config);
        let site = state.sites.get("example.test").unwrap();
        let body =
            Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .body(body)
            .unwrap();
        assert_eq!(
            proxy_request(&state, site, request).await.status(),
            StatusCode::REQUEST_TIMEOUT
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_pointing_outside_site_root() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "secret").unwrap();
        symlink(outside.path().join("secret"), root.path().join("link")).unwrap();
        assert!(matches!(
            resolve_target(&root.path().canonicalize().unwrap(), "/link", "index.html").await,
            Err(StatusCode::FORBIDDEN)
        ));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn contained_open_rejects_replacement_symlink_and_keeps_open_file_stable() {
        use std::os::unix::fs::symlink;
        use tokio::io::AsyncReadExt;
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let file_path = root.path().join("asset.txt");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&file_path, "public").unwrap();
        std::fs::write(&secret, "secret").unwrap();
        let mut file = open_contained(root.path(), Path::new("asset.txt"), true)
            .await
            .unwrap();
        std::fs::remove_file(&file_path).unwrap();
        symlink(&secret, &file_path).unwrap();
        assert!(
            open_contained(root.path(), Path::new("asset.txt"), true)
                .await
                .is_err()
        );
        let mut body = String::new();
        file.read_to_string(&mut body).await.unwrap();
        assert_eq!(body, "public");
    }

    fn test_state(config: Config) -> AppState {
        let sites = build_sites(&config).unwrap();
        let config = Arc::new(config);
        let metrics = Arc::new(Metrics::default());
        AppState {
            sites: Arc::new(sites),
            static_cache: Arc::new(StaticCache::new(
                config.static_cache.clone(),
                Arc::clone(&metrics),
            )),
            request_permits: Arc::new(Semaphore::new(config.server.max_in_flight_requests)),
            compression_permits: Arc::new(Semaphore::new(
                config.server.max_in_flight_requests.min(16),
            )),
            scheduler: SchedulerHandle::start(config.websocket.clone(), Arc::clone(&metrics)),
            proxy_client: build_proxy_client(&config).unwrap(),
            config,
            metrics,
        }
    }

    #[tokio::test]
    async fn proxy_client_is_only_created_for_proxy_sites() {
        let directory = tempdir().unwrap();
        let static_config: Config = toml::from_str(&format!(
            "[[sites]]\nhost='example.test'\nroot='{}'\n",
            directory.path().display()
        ))
        .unwrap();
        assert!(build_proxy_client(&static_config).unwrap().is_none());
        let proxy_config: Config = toml::from_str(
            "[[sites]]\nhost='example.test'\nmode='proxy'\nupstream='http://127.0.0.1:8080'\n",
        )
        .unwrap();
        crate::tls::ensure_crypto_provider();
        assert!(build_proxy_client(&proxy_config).unwrap().is_some());
    }

    #[tokio::test]
    async fn fastcgi_response_is_bounded_before_buffering() {
        use tokio::io::AsyncWriteExt;
        async fn run(limit: usize) -> Result<Response, String> {
            let (client, mut backend) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let body = b"Content-Type: text/plain\r\n\r\nhello";
                let header = [1, 6, 0, 1, 0, body.len() as u8, 0, 0];
                backend.write_all(&header).await.unwrap();
                backend.write_all(body).await.unwrap();
                backend
                    .write_all(&[1, 3, 0, 1, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
            });
            let http = http::Request::builder()
                .uri("/")
                .body(io::Cursor::new(Vec::new()))
                .unwrap();
            let request = FastCgiRequest::try_from_http_with(http, Params::default()).unwrap();
            execute_fastcgi(client, request, limit).await
        }
        let response = run(1024).await.unwrap();
        assert_eq!(to_bytes(response.into_body(), 20).await.unwrap(), "hello");
        assert!(run(16).await.is_err());
    }

    #[tokio::test]
    async fn php_rejects_oversized_request_before_connecting_to_fpm() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("index.php");
        std::fs::write(&script, "<?php").unwrap();
        let config: Config = toml::from_str(&format!("[server]\nmax_body_bytes=4\n[[sites]]\nhost='example.test'\nroot='{}'\nphp=true\nphp_fpm='tcp:127.0.0.1:9'\n", dir.path().display())).unwrap();
        let state = test_state(config);
        let site = state.sites.get("example.test").unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .body(Body::from("12345"))
            .unwrap();
        let response = php_request(&state, site, dir.path(), script, request).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn rejects_malformed_http_headers_on_the_wire() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempdir().unwrap();
        let config: Config = toml::from_str(&format!(
            "[[sites]]\nhost='example.test'\nroot='{}'\n",
            dir.path().display()
        ))
        .unwrap();
        let app = public_router(&config, test_state(config.clone()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\nInvalid Header: x\r\n\r\n")
            .await
            .unwrap();
        let mut response = vec![0; 1024];
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response[..n].starts_with(b"HTTP/1.1 400"));
        server.abort();
    }

    #[tokio::test]
    async fn http_header_deadline_closes_a_slow_connection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = bind_std_listener("127.0.0.1:0".parse().unwrap(), 16).unwrap();
        let addr = listener.local_addr().unwrap();
        let mut server = axum_server::from_tcp(listener).unwrap();
        server
            .http_builder()
            .http1()
            .timer(TokioTimer::new())
            .header_read_timeout(Duration::from_millis(100));
        let app = Router::new().fallback(any(|| async { "ok" }));
        let task = tokio::spawn(server.serve(app.into_make_service()));
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"GET / HTTP/1.1\r\nHost:").await.unwrap();
        tokio::time::sleep(Duration::from_millis(350)).await;
        let mut response = [0; 128];
        let read = tokio::time::timeout(Duration::from_secs(2), client.read(&mut response)).await;
        assert!(
            matches!(read, Ok(Ok(0)) | Ok(Err(_))),
            "connection stayed open: {read:?}"
        );
        task.abort();
    }

    #[tokio::test]
    async fn blocks_traversal_and_serves_index() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "ok").unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert_eq!(
            resolve_target(&root, "/", "index.html").await.unwrap().path,
            root.join("index.html")
        );
        assert!(matches!(
            validated_request_path("/../secret"),
            Err(StatusCode::FORBIDDEN)
        ));
        assert!(matches!(
            validated_request_path("/%2e%2e/secret"),
            Err(StatusCode::FORBIDDEN)
        ));
    }

    #[tokio::test]
    async fn resolves_configured_front_controller_for_missing_path() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("index.php"), "<?php").unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert_eq!(
            resolve_target(&root, "index.php", "index.html")
                .await
                .unwrap()
                .path,
            root.join("index.php")
        );
        assert!(matches!(
            resolve_target(&root, "/missing", "index.html").await,
            Err(StatusCode::NOT_FOUND)
        ));
    }

    #[test]
    fn strips_port_from_host() {
        assert_eq!(normalize_host("Example.COM:8080"), "example.com");
    }

    #[test]
    fn reads_http2_authority_when_host_header_is_absent() {
        let request = Request::builder()
            .uri("https://Example.COM:443/path")
            .body(Body::empty())
            .unwrap();
        assert_eq!(request_host(&request).as_deref(), Some("example.com"));
    }

    #[test]
    fn adds_rws_server_header() {
        let mut response = StatusCode::NO_CONTENT.into_response();
        add_server_header(&mut response);
        assert_eq!(response.headers().get(header::SERVER).unwrap(), "RWS");
    }

    #[test]
    fn multipart_ranges_are_bounded() {
        let request = Request::builder()
            .header("range", "bytes=0-1048575,1048576-1048576")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            requested_range(&request, 2_000_000, None, None),
            ByteRange::Ignore
        );
        let request = Request::builder()
            .header("range", "bytes=0-2,broken")
            .body(Body::empty())
            .unwrap();
        assert_eq!(requested_range(&request, 10, None, None), ByteRange::Ignore);
    }

    #[test]
    fn if_range_date_requires_an_old_and_exact_last_modified() {
        let modified = SystemTime::now() - Duration::from_secs(120);
        let date = httpdate::fmt_http_date(modified);
        let request = Request::builder()
            .header("range", "bytes=0-1")
            .header("if-range", &date)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            requested_range(&request, 10, None, Some(modified)),
            ByteRange::Selected(0, 1)
        );
        assert_eq!(
            requested_range(&request, 10, None, Some(modified + Duration::from_secs(1))),
            ByteRange::Ignore
        );
        let recent = SystemTime::now() - Duration::from_secs(5);
        let request = Request::builder()
            .header("range", "bytes=0-1")
            .header("if-range", httpdate::fmt_http_date(recent))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            requested_range(&request, 10, None, Some(recent)),
            ByteRange::Ignore
        );
    }

    #[test]
    fn entity_tags_respect_weak_comparison_and_quoted_commas() {
        let tag = HeaderValue::from_static("\"abc\"");
        assert!(entity_tag_matches(
            &HeaderValue::from_static("\"a,b\", W/\"abc\""),
            Some(&tag),
            true
        ));
        assert!(!entity_tag_matches(
            &HeaderValue::from_static("\"a,b\", W/\"abc\""),
            Some(&tag),
            false
        ));
        assert!(!entity_tag_matches(
            &HeaderValue::from_static("\"a, \"abc\""),
            Some(&tag),
            true
        ));
    }

    #[test]
    fn static_headers_omit_dates_outside_http_date_range() {
        let mut response = StatusCode::OK.into_response();
        add_static_headers(&mut response, Some(UNIX_EPOCH - Duration::from_secs(1)));
        assert!(!response.headers().contains_key(header::LAST_MODIFIED));
        add_static_headers(
            &mut response,
            Some(SystemTime::now() + Duration::from_secs(3600)),
        );
        assert!(!response.headers().contains_key(header::LAST_MODIFIED));
    }

    #[tokio::test]
    async fn static_get_and_head_have_matching_length_and_mime() {
        let directory = tempdir().unwrap();
        std::fs::write(directory.path().join("asset.txt"), "hello").unwrap();
        let text = format!(
            "[static_cache]\nenabled=true\nmax_total_bytes=1024\nmax_file_bytes=1024\nmax_entries=16\n[[sites]]\nhost='example.test'\nroot='{}'\n",
            directory.path().display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        config.validate().unwrap();
        let state = test_state(config);
        let site = state.sites.get("example.test").unwrap().clone();

        for _ in 0..2 {
            let request = Request::builder()
                .method(Method::GET)
                .uri("/asset.txt")
                .body(Body::empty())
                .unwrap();
            let response = static_or_php(&state, &site, request).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "text/plain");
            assert_eq!(response.headers()[header::CONTENT_LENGTH], "5");
            assert_eq!(to_bytes(response.into_body(), 16).await.unwrap(), "hello");
        }

        let request = Request::builder()
            .method(Method::HEAD)
            .uri("/asset.txt")
            .body(Body::empty())
            .unwrap();
        let response = static_or_php(&state, &site, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/plain");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "5");
        assert!(to_bytes(response.into_body(), 16).await.unwrap().is_empty());
        assert_eq!(
            state
                .metrics
                .static_cache_hits_total
                .load(Ordering::Relaxed),
            2
        );
    }

    #[tokio::test]
    async fn static_ranges_and_date_conditions_work_cached_and_streamed() {
        let directory = tempdir().unwrap();
        std::fs::write(directory.path().join("asset.txt"), "abcdefghij").unwrap();
        for max_file in [4, 1024] {
            let config: Config = toml::from_str(&format!(
                "[static_cache]\nenabled=true\nmax_total_bytes=1024\nmax_file_bytes={max_file}\n[[sites]]\nhost='example.test'\nroot='{}'\n", directory.path().display()
            )).unwrap();
            let state = test_state(config);
            let site = state.sites.get("example.test").unwrap();
            let get = |method: Method, key: &'static str, value: &str| {
                Request::builder()
                    .method(method)
                    .uri("/asset.txt")
                    .header(key, value)
                    .body(Body::empty())
                    .unwrap()
            };
            // Repeat so the same assertions cover a warm cached hit as well as a cold read.
            for _ in 0..2 {
                let response =
                    static_or_php(&state, site, get(Method::GET, "range", "bytes=2-5")).await;
                assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
                assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
                assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
                assert_eq!(to_bytes(response.into_body(), 32).await.unwrap(), "cdef");
            }
            let response = static_or_php(&state, site, get(Method::GET, "range", "bytes=-3")).await;
            assert_eq!(to_bytes(response.into_body(), 32).await.unwrap(), "hij");
            let response = static_or_php(&state, site, get(Method::GET, "range", "bytes=7-")).await;
            assert_eq!(to_bytes(response.into_body(), 32).await.unwrap(), "hij");
            let response =
                static_or_php(&state, site, get(Method::GET, "range", "bytes=+2-4")).await;
            assert_eq!(response.status(), StatusCode::OK);
            let response =
                static_or_php(&state, site, get(Method::GET, "range", "bytes=50-")).await;
            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
            let response =
                static_or_php(&state, site, get(Method::GET, "range", "bytes=0-1,3-4")).await;
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            let content_type = response.headers()[header::CONTENT_TYPE].to_str().unwrap();
            let boundary = content_type
                .strip_prefix("multipart/byteranges; boundary=")
                .unwrap()
                .to_owned();
            let expected = format!(
                "--{boundary}\r\nContent-Type: text/plain\r\nContent-Range: bytes 0-1/10\r\n\r\nab\r\n--{boundary}\r\nContent-Type: text/plain\r\nContent-Range: bytes 3-4/10\r\n\r\nde\r\n--{boundary}--\r\n"
            );
            assert_eq!(
                response.headers()[header::CONTENT_LENGTH],
                expected.len().to_string()
            );
            assert_eq!(
                to_bytes(response.into_body(), 1024).await.unwrap(),
                expected
            );
            let response =
                static_or_php(&state, site, get(Method::GET, "range", "bytes=50-,2-3")).await;
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-3/10");
            let response =
                static_or_php(&state, site, get(Method::GET, "range", "bytes=50-,60-")).await;
            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            let too_many = format!("bytes={}", vec!["0-0"; 17].join(","));
            let response = static_or_php(&state, site, get(Method::GET, "range", &too_many)).await;
            assert_eq!(response.status(), StatusCode::OK);
            let response =
                static_or_php(&state, site, get(Method::HEAD, "range", "bytes=0-1")).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
            assert!(to_bytes(response.into_body(), 32).await.unwrap().is_empty());
            let response = static_or_php(
                &state,
                site,
                Request::builder()
                    .uri("/asset.txt")
                    .header("range", "bytes=0-1")
                    .header("if-range", "\"old\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let last_modified = response.headers()[header::LAST_MODIFIED]
                .to_str()
                .unwrap()
                .to_owned();
            let response = static_or_php(
                &state,
                site,
                get(Method::GET, "if-modified-since", &last_modified),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            assert!(to_bytes(response.into_body(), 32).await.unwrap().is_empty());
            let response = static_or_php(
                &state,
                site,
                Request::builder()
                    .uri("/asset.txt")
                    .header("if-modified-since", &last_modified)
                    .header("range", "bytes=0-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            let response =
                static_or_php(&state, site, get(Method::GET, "if-none-match", "*")).await;
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            let response = static_or_php(
                &state,
                site,
                get(
                    Method::GET,
                    "if-unmodified-since",
                    "Thu, 01 Jan 1970 00:00:00 GMT",
                ),
            )
            .await;
            assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
            let response =
                static_or_php(&state, site, get(Method::GET, "if-match", "\"unknown\"")).await;
            assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
            let response = static_or_php(
                &state,
                site,
                Request::builder()
                    .uri("/asset.txt")
                    .header("if-none-match", "\"unknown\"")
                    .header("if-modified-since", &last_modified)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let response =
                static_or_php(&state, site, get(Method::GET, "accept-encoding", "gzip")).await;
            assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
            std::fs::write(directory.path().join("empty.txt"), "").unwrap();
            let response = static_or_php(
                &state,
                site,
                Request::builder()
                    .uri("/empty.txt")
                    .header("range", "bytes=0-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */0");
        }
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn streamed_weak_etags_validate_and_preserve_range_rules() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("asset.txt");
        std::fs::write(&path, "abcdefghij").unwrap();
        let config: Config = toml::from_str(&format!("[static_cache]\nenabled=true\nmax_total_bytes=1024\nmax_file_bytes=4\n[[sites]]\nhost='example.test'\nroot='{}'\n", directory.path().display())).unwrap();
        let state = test_state(config);
        let site = state.sites.get("example.test").unwrap();
        let req = |method: Method, headers: &[(&str, &str)]| {
            let mut builder = Request::builder().method(method).uri("/asset.txt");
            for &(key, value) in headers {
                builder = builder.header(key, value);
            }
            builder.body(Body::empty()).unwrap()
        };
        let response = static_or_php(&state, site, req(Method::GET, &[])).await;
        let tag = response.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(tag.starts_with("W/\""));
        assert_eq!(
            to_bytes(response.into_body(), 32).await.unwrap(),
            "abcdefghij"
        );
        let response = static_or_php(&state, site, req(Method::HEAD, &[])).await;
        assert_eq!(response.headers()[header::ETAG], tag);
        let response =
            static_or_php(&state, site, req(Method::GET, &[("if-none-match", &tag)])).await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::ETAG], tag);
        let response = static_or_php(
            &state,
            site,
            req(
                Method::GET,
                &[("if-none-match", tag.strip_prefix("W/").unwrap())],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        let response = static_or_php(&state, site, req(Method::GET, &[("if-match", &tag)])).await;
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        let response = static_or_php(
            &state,
            site,
            req(
                Method::GET,
                &[("if-match", tag.strip_prefix("W/").unwrap())],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        let response = static_or_php(
            &state,
            site,
            req(Method::GET, &[("range", "bytes=0-1"), ("if-range", &tag)]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let response =
            static_or_php(&state, site, req(Method::GET, &[("range", "bytes=0-1")])).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::ETAG], tag);
        let response =
            static_or_php(&state, site, req(Method::GET, &[("range", "bytes=50-")])).await;
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::ETAG], tag);
        let replacement = directory.path().join("replacement.txt");
        std::fs::write(&replacement, "0123456789").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let response =
            static_or_php(&state, site, req(Method::GET, &[("if-none-match", &tag)])).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_ne!(response.headers()[header::ETAG], tag);
        assert_eq!(
            to_bytes(response.into_body(), 32).await.unwrap(),
            "0123456789"
        );
    }

    #[tokio::test]
    async fn strong_etags_follow_the_selected_buffered_representation() {
        let directory = tempdir().unwrap();
        std::fs::write(directory.path().join("asset.txt"), "abcdefghij").unwrap();
        let config: Config = toml::from_str(&format!("[static_cache]\nenabled=true\nrevalidate_after_ms=0\n[[sites]]\nhost='example.test'\nroot='{}'\ngzip=true\n", directory.path().display())).unwrap();
        let state = test_state(config);
        let site = state.sites.get("example.test").unwrap();
        let request = |method: Method, headers: &[(&str, &str)]| {
            let mut builder = Request::builder().method(method).uri("/asset.txt");
            for &(key, value) in headers {
                builder = builder.header(key, value);
            }
            builder.body(Body::empty()).unwrap()
        };
        let response = static_or_php(&state, site, request(Method::HEAD, &[])).await;
        let tag = response.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(tag.starts_with('"') && tag.ends_with('"'));
        let response = static_or_php(
            &state,
            site,
            request(Method::GET, &[("if-none-match", &format!("W/{tag}"))]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::ETAG], tag);
        let response = static_or_php(
            &state,
            site,
            request(Method::GET, &[("if-match", &format!("W/{tag}"))]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        let response = static_or_php(
            &state,
            site,
            request(Method::GET, &[("range", "bytes=2-5"), ("if-range", &tag)]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(to_bytes(response.into_body(), 16).await.unwrap(), "cdef");
        let response = static_or_php(
            &state,
            site,
            request(
                Method::GET,
                &[("range", "bytes=2-5"), ("if-range", &format!("W/{tag}"))],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = static_or_php(
            &state,
            site,
            request(
                Method::GET,
                &[("accept-encoding", "gzip"), ("if-none-match", &tag)],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        let gzip_tag = response.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        assert_ne!(gzip_tag, tag);
        let response = static_or_php(
            &state,
            site,
            request(
                Method::GET,
                &[("accept-encoding", "gzip"), ("if-none-match", &gzip_tag)],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::ETAG], gzip_tag);
        assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
        let response = static_or_php(
            &state,
            site,
            request(
                Method::GET,
                &[("accept-encoding", "gzip"), ("if-match", &tag)],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        let response = static_or_php(
            &state,
            site,
            request(
                Method::GET,
                &[("accept-encoding", "gzip"), ("if-match", &gzip_tag)],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = static_or_php(
            &state,
            site,
            request(Method::HEAD, &[("accept-encoding", "gzip")]),
        )
        .await;
        assert_eq!(response.headers()[header::ETAG], gzip_tag);
        std::fs::write(directory.path().join("asset.txt"), "0123456789").unwrap();
        let response = static_or_php(
            &state,
            site,
            request(Method::GET, &[("if-none-match", &tag)]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_ne!(response.headers()[header::ETAG], tag);
    }

    #[tokio::test]
    async fn opt_in_gzip_respects_negotiation_range_and_head() {
        use std::io::Read;
        let directory = tempdir().unwrap();
        let contents = "hello static compression ".repeat(30);
        std::fs::write(directory.path().join("asset.txt"), &contents).unwrap();
        let config: Config = toml::from_str(&format!(
            "[static_cache]\nenabled=true\n[[sites]]\nhost='example.test'\nroot='{}'\ngzip=true\n",
            directory.path().display()
        ))
        .unwrap();
        let state = test_state(config);
        let site = state.sites.get("example.test").unwrap();
        let request = |method: Method, encoding: &str| {
            Request::builder()
                .method(method)
                .uri("/asset.txt")
                .header("accept-encoding", encoding)
                .body(Body::empty())
                .unwrap()
        };
        let response = static_or_php(&state, site, request(Method::GET, "gzip")).await;
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
        let length = response.headers()[header::CONTENT_LENGTH]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let compressed = to_bytes(response.into_body(), 2048).await.unwrap();
        assert_eq!(compressed.len(), length);
        let mut decoded = String::new();
        flate2::read::GzDecoder::new(compressed.as_ref())
            .read_to_string(&mut decoded)
            .unwrap();
        assert_eq!(decoded, contents);
        let response = static_or_php(&state, site, request(Method::HEAD, "gzip")).await;
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH]
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            length
        );
        assert!(
            to_bytes(response.into_body(), 2048)
                .await
                .unwrap()
                .is_empty()
        );
        let response = static_or_php(&state, site, request(Method::GET, "gzip;q=0")).await;
        assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
        assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
        let response = static_or_php(&state, site, request(Method::GET, "*, gzip;q=0")).await;
        assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
        let response = static_or_php(
            &state,
            site,
            request(Method::GET, "gzip;q=0.2, identity;q=0.9"),
        )
        .await;
        assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
        let response =
            static_or_php(&state, site, request(Method::GET, "identity;q=0, gzip;q=0")).await;
        assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);
        let response = static_or_php(
            &state,
            site,
            Request::builder()
                .uri("/asset.txt")
                .header("accept-encoding", "gzip")
                .header("range", "bytes=0-4")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
        assert_eq!(to_bytes(response.into_body(), 100).await.unwrap(), "hello");
        let response = static_or_php(
            &state,
            site,
            Request::builder()
                .uri("/asset.txt")
                .header("accept-encoding", "gzip, identity;q=0")
                .header("range", "bytes=0-4")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn directory_listing_is_opt_in_contained_and_escaped() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::fs::create_dir(root.path().join("files")).unwrap();
        std::fs::write(root.path().join("files/<script>.txt"), "public").unwrap();
        std::fs::write(root.path().join("files/.secret"), "hidden").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "outside").unwrap();
        symlink(outside.path(), root.path().join("files/escape")).unwrap();
        let make_state = |enabled| {
            let config: Config = toml::from_str(&format!(
                "[[sites]]\nhost='example.test'\nroot='{}'\ndirectory_listing={enabled}\n",
                root.path().display()
            ))
            .unwrap();
            test_state(config)
        };
        let request = |uri: &str| Request::builder().uri(uri).body(Body::empty()).unwrap();
        let disabled = make_state(false);
        let site = disabled.sites.get("example.test").unwrap();
        assert_eq!(
            static_or_php(&disabled, site, request("/files/"))
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        let enabled = make_state(true);
        let site = enabled.sites.get("example.test").unwrap();
        let response = static_or_php(&enabled, site, request("/files?sort=name")).await;
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(response.headers()[header::LOCATION], "/files/?sort=name");
        let response = static_or_php(&enabled, site, request("/files/")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
        let html = String::from_utf8(
            to_bytes(response.into_body(), 16_384)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(html.contains("&lt;script&gt;.txt"));
        assert!(html.contains("%3Cscript%3E%2Etxt"));
        assert!(
            !html.contains(".secret") && !html.contains("escape") && !html.contains("<script>")
        );
        assert_ne!(
            static_or_php(&enabled, site, request("/files/escape/"))
                .await
                .status(),
            StatusCode::OK
        );
        let response = static_or_php(
            &enabled,
            site,
            Request::builder()
                .method(Method::HEAD)
                .uri("/files/")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH]
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            html.len()
        );
        assert!(
            to_bytes(response.into_body(), 16_384)
                .await
                .unwrap()
                .is_empty()
        );
        std::fs::write(root.path().join("files/index.html"), "index wins").unwrap();
        let response = static_or_php(&enabled, site, request("/files/")).await;
        assert_eq!(
            to_bytes(response.into_body(), 16_384).await.unwrap(),
            "index wins"
        );
    }

    #[tokio::test]
    async fn changed_static_file_is_refreshed_after_revalidation_interval() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("asset.txt");
        std::fs::write(&path, "old").unwrap();
        let text = format!(
            "[static_cache]\nenabled=true\nmax_total_bytes=1024\nmax_file_bytes=1024\nmax_entries=16\nrevalidate_after_ms=20\n[[sites]]\nhost='example.test'\nroot='{}'\n",
            directory.path().display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        config.validate().unwrap();
        let state = test_state(config);
        let site = state.sites.get("example.test").unwrap().clone();

        let request = || {
            Request::builder()
                .method(Method::GET)
                .uri("/asset.txt")
                .body(Body::empty())
                .unwrap()
        };
        let response = static_or_php(&state, &site, request()).await;
        assert_eq!(to_bytes(response.into_body(), 16).await.unwrap(), "old");
        std::fs::write(&path, "new contents").unwrap();

        let response = static_or_php(&state, &site, request()).await;
        assert_eq!(to_bytes(response.into_body(), 16).await.unwrap(), "old");
        tokio::time::sleep(Duration::from_millis(25)).await;
        let response = static_or_php(&state, &site, request()).await;
        assert_eq!(
            to_bytes(response.into_body(), 16).await.unwrap(),
            "new contents"
        );
        assert_eq!(
            state
                .metrics
                .static_cache_revalidations_total
                .load(Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn oversized_static_file_is_streamed_without_caching() {
        let directory = tempdir().unwrap();
        std::fs::write(directory.path().join("large.txt"), "streamed").unwrap();
        let text = format!(
            "[static_cache]\nenabled=true\nmax_total_bytes=1024\nmax_file_bytes=4\nmax_entries=16\n[[sites]]\nhost='example.test'\nroot='{}'\n",
            directory.path().display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        config.validate().unwrap();
        let state = test_state(config);
        let site = state.sites.get("example.test").unwrap();
        let request = Request::builder()
            .method(Method::GET)
            .uri("/large.txt")
            .body(Body::empty())
            .unwrap();

        let response = static_or_php(&state, site, request).await;
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "8");
        assert_eq!(
            to_bytes(response.into_body(), 16).await.unwrap(),
            "streamed"
        );
        assert_eq!(
            state.metrics.static_cache_entries.load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            state
                .metrics
                .static_cache_bypasses_total
                .load(Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn cached_hit_bypasses_closed_slow_request_limiter() {
        let directory = tempdir().unwrap();
        std::fs::write(directory.path().join("asset.txt"), "cached").unwrap();
        let text = format!(
            "[static_cache]\nenabled=true\nmax_total_bytes=1024\nmax_file_bytes=1024\nmax_entries=16\n[[sites]]\nhost='example.test'\nroot='{}'\n",
            directory.path().display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        config.validate().unwrap();
        let state = test_state(config);
        let site = state.sites.get("example.test").unwrap();
        let request = || {
            Request::builder()
                .method(Method::GET)
                .uri("/asset.txt")
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            static_or_php(&state, site, request()).await.status(),
            StatusCode::OK
        );
        state.request_permits.close();
        let response = static_or_php(&state, site, request()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(to_bytes(response.into_body(), 16).await.unwrap(), "cached");
    }

    #[tokio::test]
    async fn configured_listener_uses_requested_backlog() {
        let listener = bind_listener("127.0.0.1:0".parse().unwrap(), 1024).unwrap();
        assert_ne!(listener.local_addr().unwrap().port(), 0);
    }

    #[tokio::test]
    async fn php_front_controller_never_uses_static_cache() {
        let directory = tempdir().unwrap();
        std::fs::write(directory.path().join("index.php"), "<?php echo 'ok';").unwrap();
        let text = format!(
            "[server]\nrequest_timeout_ms=100\n[static_cache]\nenabled=true\nmax_total_bytes=1024\nmax_file_bytes=1024\nmax_entries=16\n[[sites]]\nhost='example.test'\nroot='{}'\nphp=true\nphp_fpm='tcp:127.0.0.1:0'\nfront_controller='index.php'\n",
            directory.path().display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        config.validate().unwrap();
        let state = test_state(config);
        let site = state.sites.get("example.test").unwrap().clone();
        let request = Request::builder()
            .method(Method::GET)
            .uri("/application-route")
            .body(Body::empty())
            .unwrap();

        let response = static_or_php(&state, &site, request).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            state
                .metrics
                .static_cache_misses_total
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            state
                .metrics
                .static_cache_bypasses_total
                .load(Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn writes_access_log_for_matching_site() {
        let directory = tempdir().unwrap();
        std::fs::write(directory.path().join("index.html"), "ok").unwrap();
        let log_path = directory.path().join("access.log");
        let text = format!(
            "[[sites]]\nhost='example.test'\nroot='{}'\naccess_log='{}'\n",
            directory.path().display(),
            log_path.display(),
        );
        let config: Config = toml::from_str(&text).unwrap();
        config.validate().unwrap();
        let state = test_state(config.clone());
        let app = public_router(&config, state);
        let mut request = Request::builder()
            .uri("/")
            .header(header::HOST, "example.test")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "192.0.2.10:12345".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let response = tower::ServiceExt::oneshot(app, request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let contents = std::fs::read_to_string(&log_path).unwrap_or_default();
                if contents.contains("remote=\"192.0.2.10:12345\"")
                    && contents.contains("status=200")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
