use crate::{
    check::{generator_info, inspect},
    http::{HttpConnection, HttpEndpoint},
    model::{HistogramData, LatencyStats, RunResult, RunSettings, RunSummary, SCHEMA_VERSION},
    monitor::{MonitorConfig, MonitorHandle},
};
use anyhow::{Context, Result, bail};
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::IpAddr,
    path::Path,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinSet,
    time::MissedTickBehavior,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunConfig {
    pub label: String,
    pub target: String,
    pub host: String,
    pub connections: usize,
    pub active_percent: f64,
    pub idle_duration_secs: u64,
    pub active_duration_secs: u64,
    pub rps: u64,
    pub connect_rate: u64,
    pub request_timeout_ms: u64,
    pub source_ips: Vec<IpAddr>,
    pub allow_large_test: bool,
    pub monitor_ssh: Option<String>,
    pub monitor_process: Option<String>,
    pub monitor_port: u16,
    pub monitor_interval_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            label: "unlabelled".to_owned(),
            target: "http://127.0.0.1:8080/robots.txt".to_owned(),
            host: "bench.local".to_owned(),
            connections: 100,
            active_percent: 100.0,
            idle_duration_secs: 10,
            active_duration_secs: 10,
            rps: 100,
            connect_rate: 1_000,
            request_timeout_ms: 10_000,
            source_ips: Vec::new(),
            allow_large_test: false,
            monitor_ssh: None,
            monitor_process: None,
            monitor_port: 80,
            monitor_interval_ms: 1_000,
        }
    }
}

impl RunConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid configuration {}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        if self.connections == 0 || self.connect_rate == 0 || self.request_timeout_ms == 0 {
            bail!("connections, connect-rate and request-timeout-ms must be greater than zero");
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
        if self.host.trim().is_empty() || self.host.contains(['\r', '\n']) {
            bail!("host header is empty or unsafe");
        }
        Ok(())
    }
}

#[derive(Default)]
struct WorkerResult {
    opened: bool,
    failed: bool,
    unexpected_disconnects: usize,
    requests_sent: u64,
    responses_ok: u64,
    responses_failed: u64,
    socket_errors: u64,
    connection_latency: Option<u64>,
    request_latency: Option<Histogram<u64>>,
    error: Option<String>,
}

enum ConnectionStatus {
    Ready,
    Failed,
}

pub async fn execute(config: RunConfig) -> Result<RunResult> {
    config.validate()?;
    let endpoint = Arc::new(HttpEndpoint::parse(&config.target)?);
    let addresses = endpoint.resolve().await?;
    let preflight = inspect(
        Some(&config.target),
        config.connections,
        config.source_ips.len(),
    )
    .await?;
    for warning in &preflight.warnings {
        eprintln!("preflight warning: {warning}");
    }

    let active_connections = ((config.connections as f64 * config.active_percent / 100.0).round()
        as usize)
        .min(config.connections);
    let timeout = Duration::from_millis(config.request_timeout_ms);
    let started = Instant::now();
    let started_unix_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let (start_tx, _) = broadcast::channel::<()>(1);
    let (status_tx, mut status_rx) = mpsc::channel::<ConnectionStatus>(1024);
    let mut tasks = JoinSet::new();
    let batch_interval = Duration::from_millis(10);
    let batch_size = config.connect_rate.div_ceil(100).max(1) as usize;
    let mut ticker = tokio::time::interval(batch_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut spawned = 0usize;
    let mut reported = 0usize;

    while reported < config.connections {
        tokio::select! {
            _ = ticker.tick(), if spawned < config.connections => {
                let end = spawned.saturating_add(batch_size).min(config.connections);
                while spawned < end {
                    let id = spawned;
                    let endpoint = Arc::clone(&endpoint);
                    let address = addresses[id % addresses.len()];
                    let source_ip = if config.source_ips.is_empty() { None } else { Some(config.source_ips[id % config.source_ips.len()]) };
                    let host = config.host.clone();
                    let status = status_tx.clone();
                    let start = start_tx.subscribe();
                    tasks.spawn(connection_worker(
                        id, id < active_connections, endpoint, address, source_ip, host,
                        timeout, config.active_duration_secs, config.idle_duration_secs,
                        config.rps, active_connections, status, start,
                    ));
                    spawned += 1;
                }
            }
            value = status_rx.recv() => {
                match value.context("connection status channel closed early")? {
                    ConnectionStatus::Ready | ConnectionStatus::Failed => reported += 1,
                }
            }
        }
    }
    drop(status_tx);

    let monitor = if let Some(process_name) = &config.monitor_process {
        Some(MonitorHandle::start(
            MonitorConfig {
                ssh_target: config.monitor_ssh.clone(),
                process_name: process_name.clone(),
                server_port: config.monitor_port,
                interval: Duration::from_millis(config.monitor_interval_ms.max(100)),
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
    let mut connection_histogram = histogram()?;
    let mut request_histogram = histogram()?;
    while let Some(joined) = tasks.join_next().await {
        let worker = joined.context("connection task panicked")?;
        summary.connections_opened += usize::from(worker.opened);
        summary.connections_failed += usize::from(worker.failed);
        summary.unexpected_disconnects += worker.unexpected_disconnects;
        summary.requests_sent += worker.requests_sent;
        summary.responses_ok += worker.responses_ok;
        summary.responses_failed += worker.responses_failed;
        summary.socket_errors += worker.socket_errors;
        if let Some(value) = worker.connection_latency {
            let _ = connection_histogram.record(value.max(1));
        }
        if let Some(value) = worker.request_latency {
            request_histogram.add(&value)?;
        }
        if let Some(error) = worker.error {
            *summary.errors.entry(error).or_default() += 1;
        }
    }
    summary.elapsed_ms = started.elapsed().as_millis();
    summary.achieved_rps = if active_connections > 0 && config.active_duration_secs > 0 {
        summary.responses_ok as f64 / config.active_duration_secs as f64
    } else {
        0.0
    };
    summary.connection_latency = LatencyStats::from_histogram(&connection_histogram);
    summary.request_latency = LatencyStats::from_histogram(&request_histogram);
    let server_samples = match monitor {
        Some(handle) => handle.finish().await,
        None => Vec::new(),
    };
    let mut warnings = preflight.warnings;
    if active_connections > 0 && summary.achieved_rps < config.rps as f64 * 0.9 {
        warnings.push("Achieved response rate was below 90% of the configured offered rate; inspect load-generator saturation and errors before attributing the result to the server.".to_owned());
    }
    if summary.connections_opened < config.connections {
        warnings.push("Not all requested connections became usable; do not report the requested connection count as achieved.".to_owned());
    }
    Ok(RunResult {
        schema_version: SCHEMA_VERSION,
        label: config.label.clone(),
        target: config.target.clone(),
        host_header: config.host.clone(),
        started_unix_ms,
        settings: RunSettings {
            connections: config.connections,
            active_percent: config.active_percent,
            idle_duration_secs: config.idle_duration_secs,
            active_duration_secs: config.active_duration_secs,
            offered_rps: config.rps,
            connect_rate: config.connect_rate,
            request_timeout_ms: config.request_timeout_ms,
            source_ips: config.source_ips.iter().map(ToString::to_string).collect(),
        },
        generator: generator_info(),
        summary,
        raw_connection_latency: HistogramData::from_histogram(&connection_histogram),
        raw_request_latency: HistogramData::from_histogram(&request_histogram),
        server_samples,
        warnings,
    })
}

#[allow(clippy::too_many_arguments)]
async fn connection_worker(
    id: usize,
    active: bool,
    endpoint: Arc<HttpEndpoint>,
    address: std::net::SocketAddr,
    source_ip: Option<IpAddr>,
    host: String,
    timeout: Duration,
    active_duration_secs: u64,
    idle_duration_secs: u64,
    rps: u64,
    active_connections: usize,
    status: mpsc::Sender<ConnectionStatus>,
    mut start: broadcast::Receiver<()>,
) -> WorkerResult {
    let connected = Instant::now();
    let mut connection =
        match HttpConnection::connect(address, source_ip, &endpoint, &host, timeout).await {
            Ok(value) => value,
            Err(error) => {
                let _ = status.send(ConnectionStatus::Failed).await;
                return failed(error);
            }
        };
    let connection_latency = micros(connected.elapsed());
    match connection.request(timeout).await {
        Ok(200..=399) => {}
        Ok(status_code) => {
            let _ = status.send(ConnectionStatus::Failed).await;
            return failed(anyhow::anyhow!("warm-up returned HTTP {status_code}"));
        }
        Err(error) => {
            let _ = status.send(ConnectionStatus::Failed).await;
            return failed(error.context("warm-up request failed"));
        }
    }
    let _ = status.send(ConnectionStatus::Ready).await;
    if start.recv().await.is_err() {
        return failed(anyhow::anyhow!("start signal was dropped"));
    }
    let mut result = WorkerResult {
        opened: true,
        connection_latency: Some(connection_latency),
        request_latency: histogram().ok(),
        ..WorkerResult::default()
    };
    if !active {
        tokio::time::sleep(Duration::from_secs(
            idle_duration_secs.max(active_duration_secs),
        ))
        .await;
        return result;
    }

    let period = Duration::from_secs_f64(active_connections as f64 / rps as f64);
    let stagger = period.mul_f64(id as f64 / active_connections.max(1) as f64);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(active_duration_secs);
    let mut due = tokio::time::Instant::now() + stagger;
    while due < deadline {
        tokio::time::sleep_until(due).await;
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let request_started = Instant::now();
        result.requests_sent += 1;
        match connection.request(timeout).await {
            Ok(200..=399) => {
                result.responses_ok += 1;
                if let Some(histogram) = result.request_latency.as_mut() {
                    let _ = histogram.record(micros(request_started.elapsed()).max(1));
                }
            }
            Ok(status_code) => {
                result.responses_failed += 1;
                result.error = Some(format!("HTTP status {status_code}"));
            }
            Err(error) => {
                result.responses_failed += 1;
                result.socket_errors += 1;
                result.unexpected_disconnects += 1;
                result.error = Some(normalize_error(&error));
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
    result
}

fn failed(error: anyhow::Error) -> WorkerResult {
    WorkerResult {
        failed: true,
        socket_errors: 1,
        error: Some(normalize_error(&error)),
        ..WorkerResult::default()
    }
}

fn normalize_error(error: &anyhow::Error) -> String {
    error
        .to_string()
        .lines()
        .next()
        .unwrap_or("unknown error")
        .chars()
        .take(160)
        .collect()
}

fn histogram() -> Result<Histogram<u64>> {
    Ok(Histogram::<u64>::new_with_max(3_600_000_000, 3)?)
}

fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

pub fn write_json(path: &Path, result: &RunResult) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(result)?;
    std::fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

pub fn merge_error_maps(target: &mut BTreeMap<String, u64>, source: &BTreeMap<String, u64>) {
    for (key, value) in source {
        *target.entry(key.clone()).or_default() += value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_test_requires_explicit_opt_in() {
        let config = RunConfig {
            connections: 10_001,
            ..RunConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_active_run_without_rps() {
        let config = RunConfig {
            rps: 0,
            active_percent: 1.0,
            ..RunConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
