use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

const COUNTER_SHARDS: usize = 16;
static NEXT_COUNTER_SHARD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNTER_SHARD: Cell<usize> = Cell::new(
        NEXT_COUNTER_SHARD.fetch_add(1, Ordering::Relaxed) % COUNTER_SHARDS
    );
}

pub struct Counter {
    shards: [AtomicU64; COUNTER_SHARDS],
}

impl Default for Counter {
    fn default() -> Self {
        Self {
            shards: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl Counter {
    pub fn fetch_add(&self, value: u64, ordering: Ordering) -> u64 {
        COUNTER_SHARD.with(|shard| self.shards[shard.get()].fetch_add(value, ordering))
    }

    pub fn load(&self, ordering: Ordering) -> u64 {
        self.shards.iter().map(|shard| shard.load(ordering)).sum()
    }
}

#[derive(Default)]
pub struct Metrics {
    pub http_requests_total: Counter,
    pub http_errors_total: Counter,
    pub static_cache_hits_total: Counter,
    pub static_cache_misses_total: Counter,
    pub static_cache_bypasses_total: Counter,
    pub static_cache_evictions_total: Counter,
    pub static_cache_invalidations_total: Counter,
    pub static_cache_revalidations_total: Counter,
    pub static_cache_entries: AtomicUsize,
    pub static_cache_bytes: AtomicUsize,
    pub open_websockets: AtomicUsize,
    pub websocket_messages_in_total: Counter,
    pub websocket_messages_out_total: Counter,
    pub websocket_queue_rejections_total: Counter,
    pub scheduler_ready_connections: AtomicUsize,
    pub scheduler_queued_messages: AtomicUsize,
    pub scheduler_queued_bytes: AtomicUsize,
    pub controller_adjustments_total: Counter,
    pub scheduler_quantum_bytes: AtomicUsize,
}

impl Metrics {
    pub fn render(&self) -> String {
        let u64v = |value: &Counter| value.load(Ordering::Relaxed);
        let usizev = |value: &AtomicUsize| value.load(Ordering::Relaxed);
        format!(
            concat!(
                "# TYPE rws_http_requests_total counter\n",
                "rws_http_requests_total {}\n",
                "# TYPE rws_http_errors_total counter\n",
                "rws_http_errors_total {}\n",
                "# TYPE rws_static_cache_hits_total counter\n",
                "rws_static_cache_hits_total {}\n",
                "# TYPE rws_static_cache_misses_total counter\n",
                "rws_static_cache_misses_total {}\n",
                "# TYPE rws_static_cache_bypasses_total counter\n",
                "rws_static_cache_bypasses_total {}\n",
                "# TYPE rws_static_cache_evictions_total counter\n",
                "rws_static_cache_evictions_total {}\n",
                "# TYPE rws_static_cache_invalidations_total counter\n",
                "rws_static_cache_invalidations_total {}\n",
                "# TYPE rws_static_cache_revalidations_total counter\n",
                "rws_static_cache_revalidations_total {}\n",
                "# TYPE rws_static_cache_entries gauge\n",
                "rws_static_cache_entries {}\n",
                "# TYPE rws_static_cache_bytes gauge\n",
                "rws_static_cache_bytes {}\n",
                "# TYPE rws_open_websockets gauge\n",
                "rws_open_websockets {}\n",
                "# TYPE rws_websocket_messages_in_total counter\n",
                "rws_websocket_messages_in_total {}\n",
                "# TYPE rws_websocket_messages_out_total counter\n",
                "rws_websocket_messages_out_total {}\n",
                "# TYPE rws_websocket_queue_rejections_total counter\n",
                "rws_websocket_queue_rejections_total {}\n",
                "# TYPE rws_scheduler_ready_connections gauge\n",
                "rws_scheduler_ready_connections {}\n",
                "# TYPE rws_scheduler_queued_messages gauge\n",
                "rws_scheduler_queued_messages {}\n",
                "# TYPE rws_scheduler_queued_bytes gauge\n",
                "rws_scheduler_queued_bytes {}\n",
                "# TYPE rws_scheduler_quantum_bytes gauge\n",
                "rws_scheduler_quantum_bytes {}\n",
                "# TYPE rws_controller_adjustments_total counter\n",
                "rws_controller_adjustments_total {}\n"
            ),
            u64v(&self.http_requests_total),
            u64v(&self.http_errors_total),
            u64v(&self.static_cache_hits_total),
            u64v(&self.static_cache_misses_total),
            u64v(&self.static_cache_bypasses_total),
            u64v(&self.static_cache_evictions_total),
            u64v(&self.static_cache_invalidations_total),
            u64v(&self.static_cache_revalidations_total),
            usizev(&self.static_cache_entries),
            usizev(&self.static_cache_bytes),
            usizev(&self.open_websockets),
            u64v(&self.websocket_messages_in_total),
            u64v(&self.websocket_messages_out_total),
            u64v(&self.websocket_queue_rejections_total),
            usizev(&self.scheduler_ready_connections),
            usizev(&self.scheduler_queued_messages),
            usizev(&self.scheduler_queued_bytes),
            usizev(&self.scheduler_quantum_bytes),
            u64v(&self.controller_adjustments_total),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn sharded_counter_sums_updates_from_threads() {
        let counter = Arc::new(Counter::default());
        let threads = (0..8)
            .map(|_| {
                let counter = Arc::clone(&counter);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::Relaxed), 8000);
    }
}
