use crate::{
    check::generator_info,
    model::{HistogramData, LatencyStats, RunResult, RunSettings, RunSummary, SCHEMA_VERSION},
    monitor::{MonitorConfig, MonitorHandle},
};
use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use hdrhistogram::Histogram;
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinSet,
    time::MissedTickBehavior,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::{HeaderValue, header::HOST},
    },
};
use url::Url;

#[derive(Debug, Clone)]
pub struct WebSocketConfig {
    pub label: String,
    pub url: String,
    pub host: String,
    pub connections: usize,
    pub active_percent: f64,
    pub idle_duration_secs: u64,
    pub active_duration_secs: u64,
    pub rps: u64,
    pub payload_bytes: usize,
    pub connect_rate: u64,
    pub request_timeout_ms: u64,
    pub allow_large_test: bool,
    pub monitor_ssh: Option<String>,
    pub monitor_process: Option<String>,
    pub monitor_port: u16,
}

impl WebSocketConfig {
    fn validate(&self) -> Result<()> {
        let parsed = Url::parse(&self.url).context("invalid WebSocket URL")?;
        if !matches!(parsed.scheme(), "ws" | "wss") {
            bail!("WebSocket URL must use ws:// or wss://");
        }
        if self.host.trim().is_empty() || self.host.contains(['\r', '\n']) {
            bail!("host header is empty or unsafe");
        }
        if self.connections == 0 || self.connect_rate == 0 || self.payload_bytes == 0 {
            bail!("connections, connect-rate and payload-bytes must be positive");
        }
        if !(0.0..=100.0).contains(&self.active_percent) {
            bail!("active-percent must be between 0 and 100");
        }
        if self.active_percent > 0.0 && (self.rps == 0 || self.active_duration_secs == 0) {
            bail!("an active workload requires positive rps and active-duration");
        }
        if self.connections > 10_000 && !self.allow_large_test {
            bail!("tests above 10,000 connections require --allow-large-test");
        }
        if self.connections > 1_000_000 {
            bail!("the hard safety limit is 1,000,000 connections");
        }
        Ok(())
    }
}

#[derive(Default)]
struct Worker {
    opened: bool,
    failed: bool,
    sent: u64,
    ok: u64,
    response_errors: u64,
    socket_errors: u64,
    disconnects: usize,
    connect_us: Option<u64>,
    latency: Option<Histogram<u64>>,
    error: Option<String>,
}

pub async fn execute(config: WebSocketConfig) -> Result<RunResult> {
    config.validate()?;
    let active_connections = ((config.connections as f64 * config.active_percent / 100.0).round()
        as usize)
        .min(config.connections);
    let started = Instant::now();
    let started_unix_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let (start_tx, _) = broadcast::channel(1);
    let (status_tx, mut status_rx) = mpsc::channel(1024);
    let mut tasks = JoinSet::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(10));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let batch_size = config.connect_rate.div_ceil(100).max(1) as usize;
    let url = Arc::new(config.url.clone());
    let host = Arc::new(config.host.clone());
    let payload = Arc::new("x".repeat(config.payload_bytes));
    let timeout = Duration::from_millis(config.request_timeout_ms);
    let mut spawned = 0usize;
    let mut reported = 0usize;
    while reported < config.connections {
        tokio::select! {
            _ = ticker.tick(), if spawned < config.connections => {
                let end = spawned.saturating_add(batch_size).min(config.connections);
                while spawned < end {
                    let id = spawned;
                    tasks.spawn(worker(
                        id, id < active_connections, Arc::clone(&url), Arc::clone(&payload),
                        Arc::clone(&host),
                        timeout, config.active_duration_secs, config.idle_duration_secs,
                        config.rps, active_connections, status_tx.clone(), start_tx.subscribe(),
                    ));
                    spawned += 1;
                }
            }
            value = status_rx.recv() => {
                value.context("WebSocket status channel closed early")?;
                reported += 1;
            }
        }
    }
    drop(status_tx);
    let monitor = if let Some(process) = &config.monitor_process {
        Some(MonitorHandle::start(
            MonitorConfig {
                ssh_target: config.monitor_ssh.clone(),
                process_name: process.clone(),
                server_port: config.monitor_port,
                interval: Duration::from_secs(1),
            },
            started,
        )?)
    } else {
        None
    };
    let _ = start_tx.send(());
    drop(start_tx);

    let mut summary = RunSummary {
        connections_requested: config.connections,
        ..RunSummary::default()
    };
    let mut connect_histogram = histogram()?;
    let mut latency_histogram = histogram()?;
    while let Some(joined) = tasks.join_next().await {
        let value = joined.context("WebSocket task panicked")?;
        summary.connections_opened += usize::from(value.opened);
        summary.connections_failed += usize::from(value.failed);
        summary.requests_sent += value.sent;
        summary.responses_ok += value.ok;
        summary.responses_failed += value.response_errors;
        summary.socket_errors += value.socket_errors;
        summary.unexpected_disconnects += value.disconnects;
        if let Some(micros) = value.connect_us {
            let _ = connect_histogram.record(micros.max(1));
        }
        if let Some(histogram) = value.latency {
            latency_histogram.add(&histogram)?;
        }
        if let Some(error) = value.error {
            *summary.errors.entry(error).or_default() += 1;
        }
    }
    summary.elapsed_ms = started.elapsed().as_millis();
    summary.achieved_rps = if active_connections > 0 {
        summary.responses_ok as f64 / config.active_duration_secs as f64
    } else {
        0.0
    };
    summary.connection_latency = LatencyStats::from_histogram(&connect_histogram);
    summary.request_latency = LatencyStats::from_histogram(&latency_histogram);
    let server_samples = match monitor {
        Some(handle) => handle.finish().await,
        None => Vec::new(),
    };
    let mut warnings = Vec::new();
    if summary.connections_opened < config.connections {
        warnings.push("Not all requested WebSockets opened; report achieved rather than requested concurrency.".to_owned());
    }
    if active_connections > 0 && summary.achieved_rps < config.rps as f64 * 0.9 {
        warnings.push("Echo rate was below 90% of offered rate; inspect generator saturation and socket errors.".to_owned());
    }
    Ok(RunResult {
        schema_version: SCHEMA_VERSION,
        label: config.label,
        target: config.url,
        host_header: config.host,
        started_unix_ms,
        settings: RunSettings {
            connections: config.connections,
            active_percent: config.active_percent,
            idle_duration_secs: config.idle_duration_secs,
            active_duration_secs: config.active_duration_secs,
            offered_rps: config.rps,
            connect_rate: config.connect_rate,
            request_timeout_ms: config.request_timeout_ms,
            source_ips: Vec::new(),
        },
        generator: generator_info(),
        summary,
        raw_connection_latency: HistogramData::from_histogram(&connect_histogram),
        raw_request_latency: HistogramData::from_histogram(&latency_histogram),
        server_samples,
        warnings,
    })
}

#[allow(clippy::too_many_arguments)]
async fn worker(
    id: usize,
    active: bool,
    url: Arc<String>,
    payload: Arc<String>,
    host: Arc<String>,
    timeout: Duration,
    active_duration: u64,
    idle_duration: u64,
    rps: u64,
    active_connections: usize,
    status: mpsc::Sender<()>,
    mut start: broadcast::Receiver<()>,
) -> Worker {
    let connecting = Instant::now();
    let mut request = match url.as_str().into_client_request() {
        Ok(value) => value,
        Err(error) => {
            let _ = status.send(()).await;
            return failed(error.to_string());
        }
    };
    let host = match HeaderValue::from_str(&host) {
        Ok(value) => value,
        Err(error) => {
            let _ = status.send(()).await;
            return failed(error.to_string());
        }
    };
    request.headers_mut().insert(HOST, host);
    let (mut socket, _) = match tokio::time::timeout(timeout, connect_async(request)).await {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            let _ = status.send(()).await;
            return failed(error.to_string());
        }
        Err(_) => {
            let _ = status.send(()).await;
            return failed("WebSocket connect timed out".to_owned());
        }
    };
    let connect_us = micros(connecting.elapsed());
    let _ = status.send(()).await;
    if start.recv().await.is_err() {
        return failed("start signal dropped".to_owned());
    }
    let mut result = Worker {
        opened: true,
        connect_us: Some(connect_us),
        latency: histogram().ok(),
        ..Worker::default()
    };
    if !active {
        tokio::time::sleep(Duration::from_secs(idle_duration.max(active_duration))).await;
        let _ = socket.close(None).await;
        return result;
    }
    let period = Duration::from_secs_f64(active_connections as f64 / rps as f64);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(active_duration);
    let mut due =
        tokio::time::Instant::now() + period.mul_f64(id as f64 / active_connections.max(1) as f64);
    while due < deadline {
        tokio::time::sleep_until(due).await;
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let sent_at = Instant::now();
        result.sent += 1;
        let exchange = async {
            socket.send(Message::Text(payload.as_str().into())).await?;
            loop {
                match socket.next().await {
                    Some(Ok(Message::Text(_) | Message::Binary(_))) => {
                        return Ok::<(), tokio_tungstenite::tungstenite::Error>(());
                    }
                    Some(Ok(Message::Ping(value))) => socket.send(Message::Pong(value)).await?,
                    Some(Ok(Message::Close(_))) | None => {
                        return Err(tokio_tungstenite::tungstenite::Error::ConnectionClosed);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error),
                }
            }
        };
        match tokio::time::timeout(timeout, exchange).await {
            Ok(Ok(())) => {
                result.ok += 1;
                if let Some(histogram) = result.latency.as_mut() {
                    let _ = histogram.record(micros(sent_at.elapsed()).max(1));
                }
            }
            Ok(Err(error)) => {
                result.response_errors += 1;
                result.socket_errors += 1;
                result.disconnects += 1;
                result.error = Some(error.to_string());
                break;
            }
            Err(_) => {
                result.response_errors += 1;
                result.socket_errors += 1;
                result.error = Some("WebSocket echo timed out".to_owned());
                break;
            }
        }
        let next = due + period;
        due = if next < tokio::time::Instant::now() {
            tokio::time::Instant::now() + period
        } else {
            next
        };
    }
    let _ = socket.close(None).await;
    result
}

fn failed(error: String) -> Worker {
    Worker {
        failed: true,
        socket_errors: 1,
        error: Some(error.chars().take(160).collect()),
        ..Worker::default()
    }
}
fn histogram() -> Result<Histogram<u64>> {
    Ok(Histogram::<u64>::new_with_max(3_600_000_000, 3)?)
}
fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

pub fn write_json(path: &Path, result: &RunResult) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(result)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn guards_large_websocket_runs() {
        let config = WebSocketConfig {
            label: "x".into(),
            url: "ws://127.0.0.1/".into(),
            host: "bench.local".into(),
            connections: 10_001,
            active_percent: 1.0,
            idle_duration_secs: 1,
            active_duration_secs: 1,
            rps: 1,
            payload_bytes: 1,
            connect_rate: 1,
            request_timeout_ms: 1,
            allow_large_test: false,
            monitor_ssh: None,
            monitor_process: None,
            monitor_port: 80,
        };
        assert!(config.validate().is_err());
    }
}
