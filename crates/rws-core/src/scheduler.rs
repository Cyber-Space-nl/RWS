use crate::{config::WebSocketConfig, metrics::Metrics};
use axum::extract::ws::Message;
use bytes::Bytes;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Debug)]
pub enum OutboundMessage {
    Text(Bytes),
    Binary(Bytes),
    Pong(Bytes),
}

impl OutboundMessage {
    fn len(&self) -> usize {
        match self {
            Self::Text(bytes) | Self::Binary(bytes) | Self::Pong(bytes) => bytes.len().max(1),
        }
    }

    pub fn into_axum(self) -> Message {
        match self {
            Self::Text(bytes) => Message::Text(String::from_utf8_lossy(&bytes).into_owned().into()),
            Self::Binary(bytes) => Message::Binary(bytes),
            Self::Pong(bytes) => Message::Pong(bytes),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueError {
    UnknownConnection,
    QueueFull,
    SchedulerStopped,
}

struct Connection {
    writer: mpsc::Sender<OutboundMessage>,
    queue: VecDeque<OutboundMessage>,
    queued_bytes: usize,
    deficit: usize,
    ready: bool,
}

enum Command {
    Register {
        writer: mpsc::Sender<OutboundMessage>,
        reply: oneshot::Sender<u64>,
    },
    Enqueue {
        id: u64,
        message: OutboundMessage,
        reply: oneshot::Sender<Result<(), EnqueueError>>,
    },
    Unregister(u64),
}

#[derive(Clone)]
pub struct SchedulerHandle {
    commands: mpsc::Sender<Command>,
    writer_capacity: usize,
}

impl SchedulerHandle {
    pub fn start(config: WebSocketConfig, metrics: Arc<Metrics>) -> Self {
        let (commands, receiver) = mpsc::channel(1024);
        let writer_capacity = config.outbound_queue_messages;
        metrics
            .scheduler_quantum_bytes
            .store(config.scheduler_quantum_bytes, Ordering::Relaxed);
        tokio::spawn(run_scheduler(receiver, config, metrics));
        Self {
            commands,
            writer_capacity,
        }
    }

    pub fn writer_channel(
        &self,
    ) -> (
        mpsc::Sender<OutboundMessage>,
        mpsc::Receiver<OutboundMessage>,
    ) {
        mpsc::channel(self.writer_capacity)
    }

    pub async fn register(
        &self,
        writer: mpsc::Sender<OutboundMessage>,
    ) -> Result<u64, EnqueueError> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Register { writer, reply })
            .await
            .map_err(|_| EnqueueError::SchedulerStopped)?;
        receive.await.map_err(|_| EnqueueError::SchedulerStopped)
    }

    pub async fn enqueue(&self, id: u64, message: OutboundMessage) -> Result<(), EnqueueError> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Enqueue { id, message, reply })
            .await
            .map_err(|_| EnqueueError::SchedulerStopped)?;
        receive.await.map_err(|_| EnqueueError::SchedulerStopped)?
    }

    pub async fn unregister(&self, id: u64) {
        let _ = self.commands.send(Command::Unregister(id)).await;
    }
}

async fn run_scheduler(
    mut commands: mpsc::Receiver<Command>,
    config: WebSocketConfig,
    metrics: Arc<Metrics>,
) {
    let ids = AtomicU64::new(1);
    let mut connections: HashMap<u64, Connection> = HashMap::new();
    let mut ready = VecDeque::new();
    loop {
        if ready.is_empty() {
            let Some(command) = commands.recv().await else {
                break;
            };
            apply_command(
                command,
                &ids,
                &mut connections,
                &mut ready,
                &config,
                &metrics,
            );
            continue;
        }
        while let Ok(command) = commands.try_recv() {
            apply_command(
                command,
                &ids,
                &mut connections,
                &mut ready,
                &config,
                &metrics,
            );
        }
        let Some(id) = ready.pop_front() else {
            continue;
        };
        let Some(connection) = connections.get_mut(&id) else {
            continue;
        };
        connection.ready = false;
        let quantum = metrics
            .scheduler_quantum_bytes
            .load(Ordering::Relaxed)
            .max(1);
        connection.deficit = connection.deficit.saturating_add(quantum);
        let mut sent = 0usize;
        let mut writer_full = false;
        while sent < config.per_turn_messages {
            let Some(message) = connection.queue.front() else {
                break;
            };
            let cost = message.len();
            if cost > connection.deficit {
                break;
            }
            let message = connection.queue.pop_front().expect("front existed");
            match connection.writer.try_send(message) {
                Ok(()) => {
                    connection.deficit -= cost;
                    connection.queued_bytes -= cost;
                    sent += 1;
                    metrics
                        .websocket_messages_out_total
                        .fetch_add(1, Ordering::Relaxed);
                    metrics
                        .scheduler_queued_messages
                        .fetch_sub(1, Ordering::Relaxed);
                    metrics
                        .scheduler_queued_bytes
                        .fetch_sub(cost, Ordering::Relaxed);
                }
                Err(mpsc::error::TrySendError::Full(message)) => {
                    connection.queue.push_front(message);
                    writer_full = true;
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    let queued = connection.queue.len();
                    let bytes = connection.queued_bytes;
                    metrics
                        .scheduler_queued_messages
                        .fetch_sub(queued, Ordering::Relaxed);
                    metrics
                        .scheduler_queued_bytes
                        .fetch_sub(bytes, Ordering::Relaxed);
                    connections.remove(&id);
                    break;
                }
            }
        }
        if let Some(connection) = connections.get_mut(&id)
            && !connection.queue.is_empty()
            && !connection.ready
        {
            connection.ready = true;
            ready.push_back(id);
        }
        metrics
            .scheduler_ready_connections
            .store(ready.len(), Ordering::Relaxed);
        if writer_full {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        } else {
            tokio::task::yield_now().await;
        }
    }
}

fn apply_command(
    command: Command,
    ids: &AtomicU64,
    connections: &mut HashMap<u64, Connection>,
    ready: &mut VecDeque<u64>,
    config: &WebSocketConfig,
    metrics: &Metrics,
) {
    match command {
        Command::Register { writer, reply } => {
            let id = ids.fetch_add(1, Ordering::Relaxed);
            connections.insert(
                id,
                Connection {
                    writer,
                    queue: VecDeque::new(),
                    queued_bytes: 0,
                    deficit: 0,
                    ready: false,
                },
            );
            let _ = reply.send(id);
        }
        Command::Enqueue { id, message, reply } => {
            let Some(connection) = connections.get_mut(&id) else {
                let _ = reply.send(Err(EnqueueError::UnknownConnection));
                return;
            };
            let cost = message.len();
            if connection.queue.len() >= config.outbound_queue_messages
                || connection.queued_bytes.saturating_add(cost) > config.outbound_queue_bytes
            {
                metrics
                    .websocket_queue_rejections_total
                    .fetch_add(1, Ordering::Relaxed);
                let _ = reply.send(Err(EnqueueError::QueueFull));
                return;
            }
            connection.queued_bytes += cost;
            connection.queue.push_back(message);
            metrics
                .scheduler_queued_messages
                .fetch_add(1, Ordering::Relaxed);
            metrics
                .scheduler_queued_bytes
                .fetch_add(cost, Ordering::Relaxed);
            if !connection.ready {
                connection.ready = true;
                ready.push_back(id);
                metrics
                    .scheduler_ready_connections
                    .store(ready.len(), Ordering::Relaxed);
            }
            let _ = reply.send(Ok(()));
        }
        Command::Unregister(id) => {
            if let Some(connection) = connections.remove(&id) {
                metrics
                    .scheduler_queued_messages
                    .fetch_sub(connection.queue.len(), Ordering::Relaxed);
                metrics
                    .scheduler_queued_bytes
                    .fetch_sub(connection.queued_bytes, Ordering::Relaxed);
            }
            ready.retain(|queued_id| *queued_id != id);
            metrics
                .scheduler_ready_connections
                .store(ready.len(), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_over_limit_without_unbounded_growth() {
        let config = WebSocketConfig {
            outbound_queue_messages: 1,
            outbound_queue_bytes: 4,
            scheduler_quantum_bytes: 1,
            per_turn_messages: 1,
            ..WebSocketConfig::default()
        };
        let scheduler = SchedulerHandle::start(config, Arc::new(Metrics::default()));
        let (writer, _reader) = scheduler.writer_channel();
        let id = scheduler.register(writer).await.unwrap();
        scheduler
            .enqueue(id, OutboundMessage::Binary(Bytes::from_static(b"abcd")))
            .await
            .unwrap();
        let result = scheduler
            .enqueue(id, OutboundMessage::Binary(Bytes::from_static(b"x")))
            .await;
        assert_eq!(result, Err(EnqueueError::QueueFull));
    }

    #[tokio::test]
    async fn slow_writer_does_not_prevent_another_connection_from_receiving() {
        let config = WebSocketConfig {
            outbound_queue_messages: 1,
            outbound_queue_bytes: 64,
            ..WebSocketConfig::default()
        };
        let scheduler = SchedulerHandle::start(config, Arc::new(Metrics::default()));
        let (slow_writer, slow_reader) = scheduler.writer_channel();
        let slow = scheduler.register(slow_writer).await.unwrap();
        scheduler
            .enqueue(slow, OutboundMessage::Binary(Bytes::from_static(b"one")))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while slow_reader.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        scheduler
            .enqueue(slow, OutboundMessage::Binary(Bytes::from_static(b"two")))
            .await
            .unwrap();
        let (fast_writer, mut fast_reader) = scheduler.writer_channel();
        let fast = scheduler.register(fast_writer).await.unwrap();
        scheduler
            .enqueue(fast, OutboundMessage::Binary(Bytes::from_static(b"ok")))
            .await
            .unwrap();
        let message = tokio::time::timeout(std::time::Duration::from_secs(1), fast_reader.recv())
            .await
            .unwrap();
        assert!(matches!(message, Some(OutboundMessage::Binary(bytes)) if bytes == "ok"));
        scheduler.unregister(slow).await;
        scheduler.unregister(fast).await;
    }
}
