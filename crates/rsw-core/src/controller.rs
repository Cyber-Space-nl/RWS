use crate::{config::Config, metrics::Metrics};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tracing::info;

pub fn start_if_enabled(config: Arc<Config>, metrics: Arc<Metrics>) {
    if !config.adaptive.enabled {
        return;
    }
    tokio::spawn(async move {
        let adaptive = config.adaptive.clone();
        let mut interval = tokio::time::interval(Duration::from_millis(adaptive.interval_ms));
        loop {
            interval.tick().await;
            let queue = metrics.scheduler_queued_messages.load(Ordering::Relaxed);
            let current = metrics.scheduler_quantum_bytes.load(Ordering::Relaxed);
            let next = adjusted_quantum(
                current,
                queue,
                adaptive.queue_high_watermark,
                adaptive.min_quantum_bytes,
                adaptive.max_quantum_bytes,
            );
            if next != current {
                metrics
                    .scheduler_quantum_bytes
                    .store(next, Ordering::Relaxed);
                metrics
                    .controller_adjustments_total
                    .fetch_add(1, Ordering::Relaxed);
                info!(
                    previous = current,
                    current = next,
                    queue,
                    "adaptive scheduler quantum adjusted"
                );
            }
        }
    });
}

fn adjusted_quantum(current: usize, queue: usize, high: usize, min: usize, max: usize) -> usize {
    if queue > high {
        current.saturating_add((current / 4).max(1)).min(max)
    } else if queue == 0 {
        current.saturating_sub((current / 8).max(1)).max(min)
    } else {
        current.clamp(min, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_respects_bounds_and_holds_in_middle() {
        assert_eq!(adjusted_quantum(100, 1000, 10, 50, 120), 120);
        assert_eq!(adjusted_quantum(50, 0, 10, 50, 120), 50);
        assert_eq!(adjusted_quantum(80, 5, 10, 50, 120), 80);
    }
}
