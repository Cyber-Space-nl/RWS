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
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
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
    scheduler: SchedulerHandle,
    #[cfg(feature = "comfy")]
    proxy_client: reqwest::Client,
}

pub async fn serve(config: Config) -> Result<()> {
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
        scheduler,
        #[cfg(feature = "comfy")]
        proxy_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.request_timeout())
            .build()?,
    };

    #[cfg(feature = "comfy")]
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
        #[cfg(feature = "comfy")]
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
        return cached_response(asset, is_head);
    }
    let _permit = match state.request_permits.try_acquire() {
        Ok(permit) => permit,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "server busy").into_response(),
    };

    let target = match resolve_target(root, &request_path, &site.config.index).await {
        Ok(target) => target,
        Err(StatusCode::NOT_FOUND) if site.config.front_controller.is_some() => {
            state.static_cache.invalidate(site.id, &request_path);
            let front_controller = site
                .config
                .front_controller
                .as_deref()
                .expect("front controller checked above");
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
    let (body, length) = if is_head {
        state
            .static_cache
            .validate_metadata(site.id, &request_path, &target.metadata);
        (None::<Bytes>, target.metadata.len())
    } else {
        if state.static_cache.should_stream(target.metadata.len()) {
            state.static_cache.record_bypass();
            return match streamed_file_response(target.file, &target.path, target.metadata.len())
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
            Ok(asset) => return cached_response(asset, false),
            Err(error) => {
                return static_read_error(site, &target.path, error);
            }
        }
    };
    let mime = mime_guess::from_path(&target.path).first_or_octet_stream();
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
    response
}

async fn streamed_file_response(
    file: tokio::fs::File,
    path: &Path,
    length: u64,
) -> std::io::Result<Response> {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
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
    Ok(response)
}

fn cached_response(asset: CachedAsset, is_head: bool) -> Response {
    let mut response = Response::new(if is_head {
        Body::empty()
    } else {
        Body::from(asset.body)
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
    response
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

#[cfg(feature = "comfy")]
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
    let mut outgoing = state.proxy_client.request(parts.method, url).body(body);
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

#[cfg(feature = "comfy")]
fn connection_names_header(headers: &HeaderMap, name: &axum::http::HeaderName) -> bool {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case(name.as_str()))
}

#[cfg(feature = "comfy")]
fn is_untrusted_forwarding_header(name: &axum::http::HeaderName) -> bool {
    name.as_str().starts_with("x-forwarded-") || matches!(name.as_str(), "forwarded" | "x-real-ip")
}

#[cfg(feature = "comfy")]
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

    #[cfg(feature = "comfy")]
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

    #[cfg(feature = "comfy")]
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
            scheduler: SchedulerHandle::start(config.websocket.clone(), Arc::clone(&metrics)),
            config,
            metrics,
            #[cfg(feature = "comfy")]
            proxy_client: reqwest::Client::new(),
        }
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
