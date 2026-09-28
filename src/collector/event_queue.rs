use alloy::primitives::TxHash;
use alloy::rpc::types::Log;
use anyhow::{anyhow, Result};
use log::{debug, info};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;

/// Block times kept from the websocket's `newHeads` subscription.
const BLOCK_TIMES_KEPT: u64 = 64;

// Unique key for a Log event (transaction_hash and log_index)
#[derive(Debug)]
pub struct EventQueue {
    sender: Arc<EventSender>,
    receiver: Arc<Mutex<mpsc::Receiver<Log>>>,
    chain_id: u64,
}

#[derive(Debug)]
pub struct EventSender {
    inner: mpsc::Sender<Log>,
    recent_events: Arc<Mutex<HashMap<(TxHash, u64), Log>>>,
    event_order: Arc<Mutex<VecDeque<(TxHash, u64)>>>,
    /// Block number -> timestamp, from the listeners' `newHeads`
    /// subscriptions. A subscribed log carries no block time, and LB pools
    /// need it for their variable-fee clock; with the header already here, the
    /// LB timestamp enrichment needs no request of its own.
    block_times: std::sync::Mutex<BTreeMap<u64, u64>>,
    max_events: usize,
    chain_id: u64,
}

impl EventQueue {
    /// Creates a new event queue with the specified buffer size and max tracked events
    pub fn new(buffer_size: usize, max_events: usize, chain_id: u64) -> Self {
        let (sender, receiver) = mpsc::channel(buffer_size);
        let event_sender = Arc::new(EventSender {
            inner: sender,
            recent_events: Arc::new(Mutex::new(HashMap::with_capacity(max_events))),
            event_order: Arc::new(Mutex::new(VecDeque::with_capacity(max_events))),
            block_times: std::sync::Mutex::new(BTreeMap::new()),
            max_events,
            chain_id,
        });
        Self {
            sender: event_sender,
            receiver: Arc::new(Mutex::new(receiver)),
            chain_id,
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

        debug!(
            "[Chain {}] Retrieved {} events in batch",
            self.chain_id,
            events.len()
        );
        events
    }

    /// Retrieves all available events without blocking after the first
    pub async fn get_all_available_events(&self) -> Vec<Log> {
        let mut receiver = self.receiver.lock().await;
        let mut events = Vec::new();

        while let Ok(event) = receiver.try_recv() {
            info!(
                "[Chain {}] Received event: tx={}, log_index={}",
                self.chain_id,
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
            "[Chain {}] Retrieved {} events with {}ms batch timeout",
            self.chain_id,
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

    /// Block times the listeners have seen, for the LB timestamp enrichment.
    pub fn known_block_times(&self) -> HashMap<u64, u64> {
        let times = self.sender.block_times.lock().unwrap();
        times.iter().map(|(&n, &t)| (n, t)).collect()
    }
}

impl EventSender {
    /// Record a block's timestamp from a `newHeads` header, keeping only the
    /// newest `BLOCK_TIMES_KEPT` blocks.
    pub fn record_block_time(&self, number: u64, timestamp: u64) {
        let mut times = self.block_times.lock().unwrap();
        times.insert(number, timestamp);
        if let Some(&newest) = times.keys().next_back() {
            let oldest_kept = (newest + 1).saturating_sub(BLOCK_TIMES_KEPT);
            *times = times.split_off(&oldest_kept);
        }
    }

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
                    "[Chain {}] Skipped duplicate event: tx={}, log_index={}",
                    self.chain_id, transaction_hash, log_index
                );
                return Ok(());
            }

            if recent_events.len() >= self.max_events {
                if let Some(old_key) = event_order.pop_front() {
                    recent_events.remove(&old_key);
                    debug!(
                        "[Chain {}] Pruned oldest event: tx={}, log_index={}",
                        self.chain_id, old_key.0, old_key.1
                    );
                }
            }

            recent_events.insert(key, event.clone());
            event_order.push_back(key);
            info!(
                "[Chain {}] Added event to recent_events: tx={}, log_index={}",
                self.chain_id, transaction_hash, log_index
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

pub fn create_event_queue(
    buffer_size: usize,
    max_events: usize,
    chain_id: u64,
) -> (EventQueue, Arc<EventSender>) {
    let queue = EventQueue::new(buffer_size, max_events, chain_id);
    let sender = queue.get_sender();
    (queue, sender)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block time a listener records is what the queue's reader sees.
    #[test]
    fn a_recorded_block_time_is_known_to_the_queue() {
        let (queue, sender) = create_event_queue(8, 8, 43114);
        sender.record_block_time(100, 1_790_000_000);
        assert_eq!(
            queue.known_block_times(),
            HashMap::from([(100u64, 1_790_000_000u64)])
        );
    }

    /// Only the newest `BLOCK_TIMES_KEPT` blocks are kept, and a header for a
    /// block older than that window is not kept at all.
    #[test]
    fn block_times_are_kept_for_the_newest_blocks_only() {
        let (queue, sender) = create_event_queue(8, 8, 43114);
        for n in 1..=100u64 {
            sender.record_block_time(n, 1_790_000_000 + n);
        }
        let known = queue.known_block_times();
        assert_eq!(known.len(), BLOCK_TIMES_KEPT as usize);
        assert!(!known.contains_key(&(100 - BLOCK_TIMES_KEPT)));
        assert!(known.contains_key(&(101 - BLOCK_TIMES_KEPT)));

        sender.record_block_time(10, 1_790_000_010);
        assert!(!queue.known_block_times().contains_key(&10));
    }

    /// Headers can arrive out of order across listeners; an older block
    /// inside the window is still kept.
    #[test]
    fn an_older_header_inside_the_window_is_kept() {
        let (queue, sender) = create_event_queue(8, 8, 43114);
        sender.record_block_time(100, 1_790_000_100);
        sender.record_block_time(99, 1_790_000_099);
        assert_eq!(queue.known_block_times().len(), 2);
    }
}
