use crate::PoolInterface;
use crate::PoolRegistry;
use crate::Topic;
use alloy::eips::BlockNumberOrTag;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::Log;
use anyhow::Result;
use chrono::Utc;
use log::{debug, error, info};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::RwLock;

use super::fetch_events;
use super::metrics::CollectorMetrics;

/// Event sent from updater to a downstream consumer, carrying the log and any speculative pool state.
pub struct PendingEvent {
    pub event: Log,
    /// For pending block mode: cloned pools with speculative state applied.
    /// For confirmed modes: empty map (consumer reads directly from registry).
    pub modified_pools: Arc<RwLock<HashMap<Address, Box<dyn PoolInterface + Send + Sync>>>>,
}

/// Shared event processing logic used by all pool updater modes.
///
/// Handles: applying events to pools, filtering swap events, recording metrics,
/// and optionally sending events to a downstream consumer.
pub struct EventProcessor {
    pool_registry: Arc<PoolRegistry>,
    metrics: Option<Arc<dyn CollectorMetrics>>,
    swap_event_tx: Option<mpsc::Sender<PendingEvent>>,
    profitable_topics: Arc<HashSet<Topic>>,
}

impl EventProcessor {
    pub fn new(
        pool_registry: Arc<PoolRegistry>,
        metrics: Option<Arc<dyn CollectorMetrics>>,
        swap_event_tx: Option<mpsc::Sender<PendingEvent>>,
        profitable_topics: Arc<HashSet<Topic>>,
    ) -> Self {
        Self {
            pool_registry,
            metrics,
            swap_event_tx,
            profitable_topics,
        }
    }

    /// Apply events directly to the pool registry without sending swap events.
    ///
    /// Used during bootstrap/catch-up phases (confirmed blocks that are not the latest).
    /// Groups events by pool address to minimize lock acquisitions.
    pub async fn apply_events_to_registry(&self, events: &[Log]) {
        debug!("apply_events_to_registry: applying {} events", events.len());

        // Group events by pool address
        let mut events_by_pool: HashMap<Address, Vec<&Log>> = HashMap::new();
        for event in events {
            events_by_pool
                .entry(event.address())
                .or_default()
                .push(event);
        }

        for (address, pool_events) in &events_by_pool {
            if let Some(pool) = self.pool_registry.get_pool(address) {
                debug!(
                    "apply_events_to_registry: acquiring write lock for pool {} ({} events)",
                    address,
                    pool_events.len()
                );
                let mut pool_guard = pool.write().await;
                for event in pool_events {
                    if let Err(e) = pool_guard.apply_log(event) {
                        error!(
                            "Error applying event {} for pool {}, event {}",
                            e,
                            event.address(),
                            event.transaction_hash.unwrap()
                        );
                    }
                }
            }
        }
        debug!("apply_events_to_registry: done");
    }

    /// Apply events to the registry and send profitable swap events to the downstream consumer.
    ///
    /// Used by latest block and websocket modes for live event processing.
    /// Groups events by pool address to minimize lock acquisitions.
    pub async fn process_confirmed_events(&self, events: Vec<Log>) {
        let received_at = Utc::now().timestamp_millis() as u64;
        let event_count = events.len();
        info!(
            "process_confirmed_events: processing {} events",
            event_count
        );

        // Group events by pool address (preserve order within each pool)
        let mut events_by_pool: HashMap<Address, Vec<Log>> = HashMap::new();
        let mut pool_order: Vec<Address> = Vec::new();
        for event in events {
            let addr = event.address();
            if !events_by_pool.contains_key(&addr) {
                pool_order.push(addr);
            }
            events_by_pool.entry(addr).or_default().push(event);
        }

        let collect_swaps = self.swap_event_tx.is_some();
        let mut swap_events = Vec::new();
        for address in &pool_order {
            let pool_events = events_by_pool.remove(address).unwrap();
            if let Some(pool) = self.pool_registry.get_pool(address) {
                debug!(
                    "process_confirmed_events: acquiring write lock for pool {} ({} events)",
                    address,
                    pool_events.len()
                );
                let mut pool_guard = pool.write().await;
                for event in pool_events {
                    if let Err(e) = pool_guard.apply_log(&event) {
                        error!(
                            "Error applying event {} for pool {}, event {}",
                            e,
                            event.address(),
                            event.transaction_hash.unwrap()
                        );
                    }
                    if collect_swaps && self.profitable_topics.contains(event.topic0().unwrap()) {
                        swap_events.push(event);
                    }
                }
            }
        }

        if collect_swaps {
            info!(
                "process_confirmed_events: sending {} swap events",
                swap_events.len()
            );
            let empty_modified_pools = Arc::new(RwLock::new(HashMap::new()));
            for (i, event) in swap_events.into_iter().enumerate() {
                debug!("process_confirmed_events: sending swap event {}", i + 1);
                self.send_swap_event(event, Arc::clone(&empty_modified_pools), received_at)
                    .await;
            }
        }
        info!("process_confirmed_events: done");
    }

    /// Process pending block events: clone pools into speculative state, apply events
    /// to clones, and send swap events with the shared modified_pools reference.
    ///
    /// Used by pending block mode for speculative state processing.
    /// Skips entirely if no swap event sender is configured (no point in speculative cloning).
    pub async fn process_pending_events(&self, events: Vec<Log>) {
        if self.swap_event_tx.is_none() {
            return;
        }

        let received_at = Utc::now().timestamp_millis() as u64;
        let modified_pools: Arc<RwLock<HashMap<Address, Box<dyn PoolInterface + Send + Sync>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        for event in events {
            let address = event.address();

            // Get or create a deep copy of the pool
            let mut pools_guard = modified_pools.write().await;
            let pool: &mut Box<dyn PoolInterface + Send + Sync> =
                if let Some(pool) = pools_guard.get_mut(&address) {
                    pool
                } else {
                    if let Some(pool) = self.pool_registry.get_pool(&address) {
                        let pool_guard = pool.read().await;
                        let new_pool = pool_guard.clone_box();
                        drop(pool_guard);
                        pools_guard.insert(address, new_pool);
                        pools_guard.get_mut(&address).unwrap()
                    } else {
                        continue;
                    }
                };

            // Apply the event to the pool copy
            if let Err(e) = pool.apply_log(&event) {
                error!("Error applying pending event: {}", e);
            }
            drop(pools_guard);

            // Send swap events for pending block
            if self.profitable_topics.contains(event.topic0().unwrap()) {
                self.send_swap_event(event, Arc::clone(&modified_pools), received_at)
                    .await;
            }
        }
    }

    /// Record metrics and send a swap event to the downstream consumer.
    async fn send_swap_event(
        &self,
        event: Log,
        modified_pools: Arc<RwLock<HashMap<Address, Box<dyn PoolInterface + Send + Sync>>>>,
        received_at: u64,
    ) {
        let tx_hash = event.transaction_hash.unwrap();
        let log_index = event.log_index.unwrap();

        if let Some(metrics) = &self.metrics {
            debug!("send_swap_event: updating metrics...");
            metrics.add_opportunity(tx_hash, log_index, received_at);
            metrics.set_processed_at(tx_hash, log_index, Utc::now().timestamp_millis() as u64);
            debug!("send_swap_event: metrics updated");
        }

        if let Some(tx) = &self.swap_event_tx {
            debug!("send_swap_event: sending to channel...");
            if let Err(e) = tx
                .send(PendingEvent {
                    event,
                    modified_pools,
                })
                .await
            {
                error!("Error sending swap event: {}", e);
            }
            debug!("send_swap_event: sent");
        }
    }
}

/// Fetch events from provider with exponential backoff retry.
/// Includes a 30-second timeout per attempt to handle stale RPC connections.
pub async fn fetch_events_with_retry<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    addresses: Vec<Address>,
    topics: Vec<Topic>,
    from_block: BlockNumberOrTag,
    to_block: BlockNumberOrTag,
) -> Result<Vec<Log>> {
    let mut backoff = Duration::from_millis(50);
    let max_backoff = Duration::from_millis(500);
    let rpc_timeout = Duration::from_secs(30);
    let mut attempt = 0u32;

    loop {
        attempt += 1;
        debug!(
            "fetch_events_with_retry: attempt {} for blocks {:?} to {:?}",
            attempt, from_block, to_block
        );
        match tokio::time::timeout(
            rpc_timeout,
            fetch_events(provider, addresses.clone(), topics.clone(), from_block, to_block),
        )
        .await
        {
            Ok(Ok(events)) => {
                debug!(
                    "fetch_events_with_retry: got {} events (attempt {})",
                    events.len(),
                    attempt
                );
                return Ok(events);
            }
            Ok(Err(e)) => {
                error!(
                    "Error fetching events (attempt {}), retrying in {}ms: {}",
                    attempt,
                    backoff.as_millis(),
                    e
                );
            }
            Err(_) => {
                error!(
                    "Timeout fetching events (attempt {}, {}s), retrying in {}ms",
                    attempt,
                    rpc_timeout.as_secs(),
                    backoff.as_millis()
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff * 2, max_backoff);
    }
}
