use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub schema_version: u32,
    pub label: String,
    pub target: String,
    pub host_header: String,
    pub started_unix_ms: u128,
    pub settings: RunSettings,
    pub generator: GeneratorInfo,
    pub summary: RunSummary,
    #[serde(default)]
    pub raw_connection_latency: HistogramData,
    #[serde(default)]
    pub raw_request_latency: HistogramData,
    #[serde(default)]
    pub server_samples: Vec<ServerSample>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistogramData {
    pub buckets: Vec<HistogramBucket>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HistogramBucket {
    pub value_us: u64,
    pub count: u64,
}

impl HistogramData {
    pub fn from_histogram(histogram: &Histogram<u64>) -> Self {
        Self {
            buckets: histogram
                .iter_recorded()
                .map(|item| HistogramBucket {
                    value_us: item.value_iterated_to(),
                    count: item.count_since_last_iteration(),
                })
                .collect(),
        }
    }

    pub fn to_histogram(&self) -> Result<Histogram<u64>, hdrhistogram::CreationError> {
        let mut histogram = Histogram::<u64>::new_with_max(3_600_000_000, 3)?;
        for bucket in &self.buckets {
            let _ = histogram.record_n(bucket.value_us.max(1), bucket.count);
        }
        Ok(histogram)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSettings {
    pub connections: usize,
    pub active_percent: f64,
    pub idle_duration_secs: u64,
    pub active_duration_secs: u64,
    pub offered_rps: u64,
    pub connect_rate: u64,
    pub request_timeout_ms: u64,
    pub source_ips: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeneratorInfo {
    pub os: String,
    pub logical_cpus: usize,
    pub fd_soft_limit: Option<u64>,
    pub ephemeral_port_capacity: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunSummary {
    pub connections_requested: usize,
    pub connections_opened: usize,
    pub connections_failed: usize,
    pub unexpected_disconnects: usize,
    pub reconnects: usize,
    pub requests_sent: u64,
    pub responses_ok: u64,
    pub responses_failed: u64,
    pub socket_errors: u64,
    pub elapsed_ms: u128,
    pub achieved_rps: f64,
    pub connection_latency: LatencyStats,
    pub request_latency: LatencyStats,
    #[serde(default)]
    pub errors: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct LatencyStats {
    pub samples: u64,
    pub min_us: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub mean_us: f64,
}

impl LatencyStats {
    pub fn from_histogram(histogram: &Histogram<u64>) -> Self {
        if histogram.is_empty() {
            return Self::default();
        }
        Self {
            samples: histogram.len(),
            min_us: histogram.min(),
            p50_us: histogram.value_at_quantile(0.50),
            p95_us: histogram.value_at_quantile(0.95),
            p99_us: histogram.value_at_quantile(0.99),
            max_us: histogram.max(),
            mean_us: histogram.mean(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerSample {
    pub elapsed_ms: u128,
    pub cpu_percent: Option<f64>,
    pub rss_kib: Option<u64>,
    pub pss_kib: Option<u64>,
    pub file_descriptors: Option<u64>,
    pub established_connections: Option<u64>,
    pub tcp_memory_pages: Option<u64>,
    pub rx_bytes: Option<u64>,
    pub tx_bytes: Option<u64>,
    pub tcp_resets: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_percentiles_are_deterministic() {
        let mut histogram = Histogram::<u64>::new_with_max(1_000_000, 3).unwrap();
        for value in 1..=100 {
            histogram.record(value).unwrap();
        }
        let stats = LatencyStats::from_histogram(&histogram);
        assert_eq!(stats.samples, 100);
        assert_eq!(stats.p50_us, 50);
        assert_eq!(stats.p95_us, 95);
        assert_eq!(stats.p99_us, 99);
        assert_eq!(stats.max_us, 100);
    }
}
