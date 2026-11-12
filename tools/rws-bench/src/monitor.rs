use crate::model::ServerSample;
use anyhow::{Context, Result, bail};
use std::sync::Arc;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::{
    process::Command,
    sync::{Mutex, watch},
    task::JoinHandle,
};

#[derive(Debug, Clone)]
pub struct MonitorConfig {
    pub ssh_target: Option<String>,
    pub process_name: String,
    pub server_port: u16,
    pub interval: Duration,
}

pub struct MonitorHandle {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
    samples: Arc<Mutex<Vec<ServerSample>>>,
}

impl MonitorHandle {
    pub fn start(config: MonitorConfig, started: Instant) -> Result<Self> {
        if !config
            .process_name
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.'))
        {
            bail!("monitor process name contains unsupported characters");
        }
        let (stop, mut stopped) = watch::channel(false);
        let samples = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&samples);
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(config.interval);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        match collect_sample(&config, started).await {
                            Ok(sample) => output.lock().await.push(sample),
                            Err(error) => eprintln!("monitor warning: {error:#}"),
                        }
                    }
                    changed = stopped.changed() => {
                        if changed.is_err() || *stopped.borrow() { break; }
                    }
                }
            }
        });
        Ok(Self {
            stop,
            task,
            samples,
        })
    }

    pub async fn finish(self) -> Vec<ServerSample> {
        let _ = self.stop.send(true);
        let _ = self.task.await;
        self.samples.lock().await.clone()
    }
}

async fn collect_sample(config: &MonitorConfig, started: Instant) -> Result<ServerSample> {
    let script = sample_script(&config.process_name, config.server_port);
    let output = if let Some(target) = &config.ssh_target {
        Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                target,
                &script,
            ])
            .output()
            .await
            .with_context(|| format!("failed to start ssh for monitor target {target}"))?
    } else {
        Command::new("sh")
            .args(["-c", &script])
            .output()
            .await
            .context("failed to start local monitoring shell")?
    };
    if !output.status.success() {
        bail!(
            "monitor command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let values = parse_values(&stdout);
    Ok(ServerSample {
        elapsed_ms: started.elapsed().as_millis(),
        cpu_percent: parse(&values, "cpu_percent"),
        rss_kib: parse(&values, "rss_kib"),
        pss_kib: parse(&values, "pss_kib"),
        file_descriptors: parse(&values, "file_descriptors"),
        established_connections: parse(&values, "established_connections"),
        tcp_memory_pages: parse(&values, "tcp_memory_pages"),
        rx_bytes: parse(&values, "rx_bytes"),
        tx_bytes: parse(&values, "tx_bytes"),
        tcp_resets: parse(&values, "tcp_resets"),
    })
}

fn sample_script(process: &str, port: u16) -> String {
    format!(
        r#"pids=$(pgrep -x '{process}' 2>/dev/null || true)
if [ -n "$pids" ]; then
  pid_csv=$(printf '%s\n' $pids | paste -sd, -)
  cpu_before=$(for pid in $pids; do awk '{{print $14+$15+$16+$17}}' /proc/$pid/stat 2>/dev/null; done | awk '{{s+=$1}} END {{print s+0}}')
  sleep 0.1
  cpu_after=$(for pid in $pids; do awk '{{print $14+$15+$16+$17}}' /proc/$pid/stat 2>/dev/null; done | awk '{{s+=$1}} END {{print s+0}}')
  clock_ticks=$(getconf CLK_TCK)
  echo cpu_percent=$(awk -v before="$cpu_before" -v after="$cpu_after" -v hz="$clock_ticks" 'BEGIN {{printf "%.2f", (after-before)*1000/hz}}')
  echo rss_kib=$(ps -p "$pid_csv" -o rss= 2>/dev/null | awk '{{s+=$1}} END {{print s+0}}')
  echo pss_kib=$(for pid in $pids; do awk '/^Pss:/ {{s+=$2}} END {{print s+0}}' /proc/$pid/smaps_rollup 2>/dev/null; done | awk '{{s+=$1}} END {{print s+0}}')
  echo file_descriptors=$(for pid in $pids; do find /proc/$pid/fd -maxdepth 1 -type l 2>/dev/null; done | wc -l)
fi
echo established_connections=$(ss -Htan state established 'sport = :{port}' 2>/dev/null | wc -l)
echo tcp_memory_pages=$(awk '/^TCP:/ {{for(i=1;i<=NF;i++) if($i=="mem") print $(i+1)}}' /proc/net/sockstat)
awk -F'[: ]+' 'NR>2 && $1!="lo" {{rx+=$3; tx+=$11}} END {{print "rx_bytes=" rx+0; print "tx_bytes=" tx+0}}' /proc/net/dev
echo tcp_resets=$(nstat -az TcpEstabResets 2>/dev/null | awk '$1=="TcpEstabResets" {{print $2}}')"#
    )
}

fn parse_values(text: &str) -> HashMap<&str, &str> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .collect()
}

fn parse<T: std::str::FromStr>(values: &HashMap<&str, &str>, key: &str) -> Option<T> {
    values.get(key)?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_monitor_output() {
        let values = parse_values("cpu_percent=12.5\nrss_kib=2048\n");
        assert_eq!(parse::<f64>(&values, "cpu_percent"), Some(12.5));
        assert_eq!(parse::<u64>(&values, "rss_kib"), Some(2048));
    }
}
