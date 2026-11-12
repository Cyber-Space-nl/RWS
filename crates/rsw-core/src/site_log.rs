use anyhow::{Context, Result};
use std::{
    fs::OpenOptions,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    sync::mpsc,
    time::{Duration, MissedTickBehavior},
};
use tracing::{error, warn};

const LOG_QUEUE_CAPACITY: usize = 4096;
const LOG_BUFFER_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub(crate) struct SiteLog {
    sender: mpsc::Sender<String>,
    path: Arc<std::path::PathBuf>,
    dropped: Arc<AtomicU64>,
    closed_warning_emitted: Arc<AtomicBool>,
}

impl SiteLog {
    pub(crate) fn start(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open site log {}", path.display()))?;
        let path = Arc::new(path.to_path_buf());
        let writer_path = Arc::clone(&path);
        let (sender, mut receiver) = mpsc::channel::<String>(LOG_QUEUE_CAPACITY);
        tokio::spawn(async move {
            let mut file =
                BufWriter::with_capacity(LOG_BUFFER_BYTES, tokio::fs::File::from_std(file));
            let mut flush_interval = tokio::time::interval(Duration::from_secs(1));
            flush_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    line = receiver.recv() => {
                        let Some(line) = line else { break; };
                        if let Err(write_error) = file.write_all(line.as_bytes()).await {
                            error!(path = %writer_path.display(), %write_error, "failed to write site log");
                        }
                    }
                    _ = flush_interval.tick() => {
                        if let Err(flush_error) = file.flush().await {
                            error!(path = %writer_path.display(), %flush_error, "failed to flush site log");
                        }
                    }
                }
            }
            if let Err(flush_error) = file.flush().await {
                error!(path = %writer_path.display(), %flush_error, "failed to flush site log");
            }
        });
        Ok(Self {
            sender,
            path,
            dropped: Arc::new(AtomicU64::new(0)),
            closed_warning_emitted: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn write(&self, line: String) {
        match self.sender.try_send(line) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped.is_power_of_two() {
                    warn!(
                        path = %self.path.display(),
                        dropped,
                        "site log queue full; log entries dropped"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                if !self.closed_warning_emitted.swap(true, Ordering::Relaxed) {
                    warn!(path = %self.path.display(), "site log writer closed; dropping log entries");
                }
            }
        }
    }
}

pub(crate) fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(crate) fn quoted(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            value if value.is_control() => output.push('?'),
            value => output.push(value),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn quotes_untrusted_log_values() {
        assert_eq!(quoted("a\n\"b\\c"), "\"a\\n\\\"b\\\\c\"");
    }

    #[tokio::test]
    async fn writes_queued_lines() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("access.log");
        let log = SiteLog::start(&path).unwrap();
        log.write("request\n".to_owned());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if std::fs::read_to_string(&path).unwrap_or_default() == "request\n" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
