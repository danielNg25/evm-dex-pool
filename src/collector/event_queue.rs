use alloy::primitives::TxHash;
use alloy::rpc::types::Log;
use anyhow::{anyhow, Result};
use log::{debug, info};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;

// Unique key for a Log event (transaction_hash and log_index)
#[derive(Debug)]
pub struct EventQueue {
    sender: Arc<EventSender>,
    receiver: Arc<Mutex<mpsc::Receiver<Log>>>,
}

#[derive(Debug)]
pub struct EventSender {
    inner: mpsc::Sender<Log>,
    recent_events: Arc<Mutex<HashMap<(TxHash, u64), Log>>>,
    event_order: Arc<Mutex<VecDeque<(TxHash, u64)>>>,
    max_events: usize,
}

impl EventQueue {
    /// Creates a new event queue with the specified buffer size and max tracked events
    pub fn new(buffer_size: usize, max_events: usize) -> Self {
        let (sender, receiver) = mpsc::channel(buffer_size);
        let event_sender = Arc::new(EventSender {
            inner: sender,
            recent_events: Arc::new(Mutex::new(HashMap::with_capacity(max_events))),
            event_order: Arc::new(Mutex::new(VecDeque::with_capacity(max_events))),
            max_events,
        });
        Self {
            sender: event_sender,
            receiver: Arc::new(Mutex::new(receiver)),
        }
    }

    /// Returns a clonable sender for multiple WebSocket feeds
    pub fn get_sender(&self) -> Arc<EventSender> {
        self.sender.clone()
    }

    /// Retrieves the next event, blocking until one is available
    pub async fn next_event(&self) -> Option<Log> {
        self.receiver.lock().await.recv().await
    }

    /// Retrieves up to max_events without blocking after the first
    pub async fn get_events_batch(&self, max_events: usize) -> Vec<Log> {
        let mut receiver = self.receiver.lock().await;
        let mut events = Vec::with_capacity(max_events);

        if let Some(event) = receiver.recv().await {
            events.push(event);
            for _ in 1..max_events {
                if let Ok(event) = receiver.try_recv() {
                    events.push(event);
                } else {
                    break;
                }
            }
        }

        debug!("Retrieved {} events in batch", events.len());
        events
    }

    /// Retrieves all available events without blocking after the first
    pub async fn get_all_available_events(&self) -> Vec<Log> {
        let mut receiver = self.receiver.lock().await;
        let mut events = Vec::new();

        while let Ok(event) = receiver.try_recv() {
            info!(
                "Received event: tx={}, log_index={}",
                event.transaction_hash.unwrap_or_default(),
                event.log_index.unwrap_or_default()
            );
            events.push(event);
        }

        events
    }

    /// Retrieves events with a timeout to batch multiple events
    pub async fn get_events_with_batching(&self, batch_timeout: Duration) -> Vec<Log> {
        let mut receiver = self.receiver.lock().await;
        let mut events = Vec::new();

        if let Some(event) = receiver.recv().await {
            events.push(event);
            tokio::time::sleep(batch_timeout).await;
            while let Ok(event) = receiver.try_recv() {
                events.push(event);
            }
        }

        debug!(
            "Retrieved {} events with {}ms batch timeout",
            events.len(),
            batch_timeout.as_millis()
        );
        events
    }

    /// Checks if an event with the given transaction hash and log index exists
    pub async fn has_event(&self, transaction_hash: TxHash, log_index: u64) -> bool {
        self.sender
            .recent_events
            .lock()
            .await
            .contains_key(&(transaction_hash, log_index))
    }
}

impl EventSender {
    /// Sends an event, checking for duplicates and updating the recent events HashMap
    pub async fn send(&self, event: Log) -> Result<()> {
        let transaction_hash = event
            .transaction_hash
            .ok_or_else(|| anyhow!("Log missing transaction hash"))?;
        let log_index = event
            .log_index
            .ok_or_else(|| anyhow!("Log missing log index"))?;

        // Check for duplicate and update recent events in a single lock scope
        {
            let mut recent_events = self.recent_events.lock().await;
            let mut event_order = self.event_order.lock().await;

            let key = (transaction_hash, log_index);
            if recent_events.contains_key(&key) {
                debug!(
                    "Skipped duplicate event: tx={}, log_index={}",
                    transaction_hash, log_index
                );
                return Ok(());
            }

            if recent_events.len() >= self.max_events {
                if let Some(old_key) = event_order.pop_front() {
                    recent_events.remove(&old_key);
                    debug!(
                        "Pruned oldest event: tx={}, log_index={}",
                        old_key.0, old_key.1
                    );
                }
            }

            recent_events.insert(key, event.clone());
            event_order.push_back(key);
            info!(
                "Added event to recent_events: tx={}, log_index={}",
                transaction_hash, log_index
            );
        } // Release locks before sending to reduce contention

        // Send to mpsc channel
        self.inner
            .send(event)
            .await
            .map_err(|e| anyhow!("Failed to send event: {}", e))?;
        Ok(())
    }
}

pub fn create_event_queue(buffer_size: usize, max_events: usize) -> (EventQueue, Arc<EventSender>) {
    let queue = EventQueue::new(buffer_size, max_events);
    let sender = queue.get_sender();
    (queue, sender)
}
