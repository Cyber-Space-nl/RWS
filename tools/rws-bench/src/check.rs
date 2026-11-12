use crate::{http::HttpEndpoint, model::GeneratorInfo};
use anyhow::Result;
use serde::Serialize;
use std::{fs, path::Path};

#[derive(Debug, Serialize)]
pub struct CheckReport {
    pub generator: GeneratorInfo,
    pub target_addresses: Vec<String>,
    pub requested_connections: usize,
    pub kernel: KernelLimits,
    pub warnings: Vec<String>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct KernelLimits {
    pub ephemeral_port_low: Option<u64>,
    pub ephemeral_port_high: Option<u64>,
    pub somaxconn: Option<u64>,
    pub tcp_max_syn_backlog: Option<u64>,
    pub file_max: Option<u64>,
}

pub fn generator_info() -> GeneratorInfo {
    let (low, high) = ephemeral_range().unwrap_or((0, 0));
    GeneratorInfo {
        os: std::env::consts::OS.to_owned(),
        logical_cpus: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
        fd_soft_limit: fd_soft_limit(),
        ephemeral_port_capacity: (high >= low && high > 0).then_some(high - low + 1),
    }
}

pub async fn inspect(
    target: Option<&str>,
    connections: usize,
    source_ip_count: usize,
) -> Result<CheckReport> {
    let generator = generator_info();
    let (low, high) = ephemeral_range().unwrap_or((0, 0));
    let kernel = KernelLimits {
        ephemeral_port_low: (low > 0).then_some(low),
        ephemeral_port_high: (high > 0).then_some(high),
        somaxconn: read_u64("/proc/sys/net/core/somaxconn"),
        tcp_max_syn_backlog: read_u64("/proc/sys/net/ipv4/tcp_max_syn_backlog"),
        file_max: read_u64("/proc/sys/fs/file-max"),
    };
    let target_addresses = if let Some(target) = target {
        HttpEndpoint::parse(target)?
            .resolve()
            .await?
            .into_iter()
            .map(|value| value.to_string())
            .collect()
    } else {
        Vec::new()
    };
    let mut warnings = Vec::new();
    let mut recommendations = Vec::new();
    if !cfg!(target_os = "linux") {
        warnings.push("The load generator is not Linux; Linux-specific fd, TCP and epoll checks are unavailable.".to_owned());
    }
    if let Some(limit) = generator.fd_soft_limit
        && connections.saturating_add(256) as u64 > limit
    {
        warnings.push(format!(
            "Requested sockets plus headroom exceed the soft fd limit ({limit})."
        ));
        recommendations.push("Raise the process file-descriptor limit before the run; do not change it silently from the benchmark.".to_owned());
    }
    if let Some(capacity) = generator.ephemeral_port_capacity {
        let sources = source_ip_count.max(1) as u64;
        if connections as u64 > capacity.saturating_mul(sources) {
            warnings.push(format!(
                "Requested connections exceed the approximate ephemeral-port capacity of {} across {} source IP(s).",
                capacity.saturating_mul(sources), sources
            ));
            recommendations.push("Use additional assigned source IPs or multiple load generators and merge their result files.".to_owned());
        }
    }
    if connections >= 50_000 && generator.logical_cpus < 2 {
        warnings.push(
            "The load generator has fewer than two logical CPUs; it may become the bottleneck."
                .to_owned(),
        );
    }
    recommendations.push(
        "Watch generator CPU, RAM, packet loss and NIC throughput separately from server metrics."
            .to_owned(),
    );
    recommendations
        .push("Confirm the server keep-alive timeout exceeds the complete idle phase.".to_owned());
    Ok(CheckReport {
        generator,
        target_addresses,
        requested_connections: connections,
        kernel,
        warnings,
        recommendations,
    })
}

pub fn print_human(report: &CheckReport) {
    println!("os={}", report.generator.os);
    println!("logical_cpus={}", report.generator.logical_cpus);
    println!("fd_soft_limit={}", optional(report.generator.fd_soft_limit));
    println!(
        "ephemeral_port_capacity_per_source_ip={}",
        optional(report.generator.ephemeral_port_capacity)
    );
    println!("somaxconn={}", optional(report.kernel.somaxconn));
    println!(
        "tcp_max_syn_backlog={}",
        optional(report.kernel.tcp_max_syn_backlog)
    );
    if !report.target_addresses.is_empty() {
        println!("target_addresses={}", report.target_addresses.join(","));
    }
    for warning in &report.warnings {
        println!("WARNING: {warning}");
    }
    for recommendation in &report.recommendations {
        println!("RECOMMENDATION: {recommendation}");
    }
}

fn optional(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn ephemeral_range() -> Option<(u64, u64)> {
    let text = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").ok()?;
    let mut values = text
        .split_whitespace()
        .filter_map(|value| value.parse().ok());
    Some((values.next()?, values.next()?))
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn fd_soft_limit() -> Option<u64> {
    let text = fs::read_to_string("/proc/self/limits").ok()?;
    let line = text
        .lines()
        .find(|line| line.starts_with("Max open files"))?;
    line.split_whitespace().nth(3)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generator_info_is_nonempty() {
        let info = generator_info();
        assert!(!info.os.is_empty());
        assert!(info.logical_cpus >= 1);
    }
}
