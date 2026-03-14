use crate::collector::config::PoolFetchConfig;
use crate::collector::event_processor::{fetch_events_with_retry, PendingEvent};
use crate::collector::event_queue::EventQueue;
use crate::collector::metrics::CollectorMetrics;
use crate::collector::pool_fetcher::{fetch_pool, identify_pool_type};
use crate::collector::unified_pool_updater::{UnifiedPoolUpdater, UpdaterMode};
use crate::collector::websocket_listener::WebsocketListener;
use crate::collector::CollectorConfig;
use crate::{PoolInterface, PoolRegistry, PoolType, TokenInfo};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::Log;
use anyhow::Result;
use futures_util::future::join_all;
use log::{error, info, warn};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Duration;

/// Handle for a running collector. Returned by [`start_collector`](super::start_collector).
///
/// Provides [`stop`](CollectorHandle::stop) to halt the collector and
/// [`add_pools`](CollectorHandle::add_pools) to dynamically register new pool
/// addresses while keeping all pool states consistent.
pub struct CollectorHandle<P: Provider + Send + Sync + Clone + 'static> {
    cancel_tx: Option<oneshot::Sender<()>>,
    /// JoinHandle for the running updater task. Awaited in `stop_updater` to
    /// guarantee `last_processed_block` is fully committed before it is read.
    updater_handle: Option<tokio::task::JoinHandle<()>>,

    // WS-specific (empty in HTTP / PendingBlock mode)
    ws_listeners: Vec<Arc<WebsocketListener>>,
    ws_urls: Vec<String>,

    // Stored for collector restart
    provider: Arc<P>,
    pool_registry: Arc<PoolRegistry>,
    metrics: Option<Arc<dyn CollectorMetrics>>,
    swap_event_tx: Option<mpsc::Sender<PendingEvent>>,
    collector_config: CollectorConfig,
}

impl<P: Provider + Send + Sync + Clone + 'static> CollectorHandle<P> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cancel_tx: oneshot::Sender<()>,
        updater_handle: tokio::task::JoinHandle<()>,
        ws_listeners: Vec<Arc<WebsocketListener>>,
        ws_urls: Vec<String>,
        provider: Arc<P>,
        pool_registry: Arc<PoolRegistry>,
        metrics: Option<Arc<dyn CollectorMetrics>>,
        swap_event_tx: Option<mpsc::Sender<PendingEvent>>,
        collector_config: CollectorConfig,
    ) -> Self {
        Self {
            cancel_tx: Some(cancel_tx),
            updater_handle: Some(updater_handle),
            ws_listeners,
            ws_urls,
            provider,
            pool_registry,
            metrics,
            swap_event_tx,
            collector_config,
        }
    }

    /// Stop the running collector (updater loop and all WS listeners).
    pub async fn stop(&mut self) {
        // Stop WS listeners first so the EventQueue channel stays open while
        // we wait for the updater task to drain and exit. Stopping the updater
        // first would drop the EventQueue receiver (closing the channel) before
        // the listeners are told to stop, causing "channel closed" error spam.
        for listener in &self.ws_listeners {
            if let Err(e) = listener.stop().await {
                error!("Error stopping WS listener: {}", e);
            }
        }
        self.ws_listeners.clear();
        self.stop_updater().await;
    }

    /// Dynamically add new pool addresses to the running collector.
    ///
    /// Pools already present in the registry are silently skipped.
    ///
    /// The method fetches each pool's on-chain state in memory, applies a
    /// block-level catchup so new pools are consistent with the existing
    /// registry, then registers them. The collector is paused only for the
    /// brief synchronisation step, not during the (potentially slow) RPC
    /// fetch phase.
    pub async fn add_pools<T: TokenInfo>(
        &mut self,
        new_addresses: Vec<Address>,
        fetch_config: &PoolFetchConfig,
        token_info: &T,
    ) -> Result<()> {
        if new_addresses.is_empty() {
            return Ok(());
        }

        // Skip addresses already present in the registry
        let addresses: Vec<Address> = new_addresses
            .into_iter()
            .filter(|a| self.pool_registry.get_pool(a).is_none())
            .collect();

        if addresses.is_empty() {
            info!(
                "[Chain {}] add_pools: all requested pools already in registry",
                fetch_config.chain_id
            );
            return Ok(());
        }

        if self.collector_config.use_websocket {
            self.add_pools_ws(&addresses, fetch_config, token_info)
                .await
        } else {
            self.add_pools_http(&addresses, fetch_config, token_info)
                .await
        }
    }

    /// Remove pool addresses from the registry.
    ///
    /// The collector continues running — events for removed pools are
    /// silently skipped by the `EventProcessor`. In RPC modes the
    /// address filter narrows on the next batch; in WebSocket mode the
    /// subscription remains unchanged (events arrive but are ignored).
    ///
    /// Returns the number of pools actually removed (addresses not in
    /// the registry are silently skipped).
    pub fn remove_pools(&self, addresses: &[Address]) -> usize {
        let chain_id = self.pool_registry.get_network_id();
        let mut removed = 0usize;
        for addr in addresses {
            if self.pool_registry.remove_pool(addr).is_some() {
                removed += 1;
            }
        }
        if removed > 0 {
            info!(
                "[Chain {}] remove_pools: removed {} of {} requested pools ({} remaining)",
                chain_id,
                removed,
                addresses.len(),
                self.pool_registry.pool_count(),
            );
        }
        removed
    }

    // -------------------------------------------------------------------------
    // HTTP / PendingBlock mode
    // -------------------------------------------------------------------------

    async fn add_pools_http<T: TokenInfo>(
        &mut self,
        addresses: &[Address],
        fetch_config: &PoolFetchConfig,
        token_info: &T,
    ) -> Result<()> {
        let chain_id = fetch_config.chain_id;

        // 1. Record the block at which we start fetching (collector still running).
        let fetch_block = self.pool_registry.get_last_processed_block();
        info!(
            "[Chain {}] add_pools: fetching {} pools in memory at block {}",
            chain_id,
            addresses.len(),
            fetch_block
        );

        // 2. Fetch pool state into memory — NOT into the registry yet.
        //    The running collector excludes these addresses, so no race on events.
        let mut pools = fetch_pools_in_memory(
            &self.provider,
            addresses,
            BlockId::Number(BlockNumberOrTag::Number(fetch_block)),
            token_info,
            fetch_config,
        )
        .await?;

        info!(
            "[Chain {}] add_pools: fetched {} pools, stopping collector",
            chain_id,
            pools.len()
        );

        // 3. Stop the updater and wait for it to exit. last_processed_block is now stable.
        self.stop_updater().await;

        // 4. Read the final block the collector reached before stopping.
        let final_block = self.pool_registry.get_last_processed_block();

        // 5. Catchup: apply events for the new pool addresses only, from
        //    fetch_block+1 to final_block. Existing pool addresses are NOT
        //    included in the filter, so no double-apply risk.
        if final_block > fetch_block {
            info!(
                "[Chain {}] add_pools: catching up new pools blocks {}..={}",
                chain_id,
                fetch_block + 1,
                final_block
            );
            apply_catchup_events_in_memory(
                &self.provider,
                &mut pools,
                self.pool_registry.get_topics(),
                fetch_block + 1,
                final_block,
                chain_id,
            )
            .await?;
        }

        // 6. Register pools and any new event topics.
        register_pools_and_topics(&self.pool_registry, pools);

        info!(
            "[Chain {}] add_pools: restarting collector from block {}",
            chain_id, final_block
        );

        // 7. Restart the updater from final_block.
        let mode = if self.collector_config.use_pending_blocks {
            UpdaterMode::PendingBlock
        } else {
            UpdaterMode::LatestBlock {
                wait_time_ms: self.collector_config.wait_time,
            }
        };
        self.spawn_updater(mode, final_block);

        info!(
            "[Chain {}] add_pools: done — {} new pools now active",
            chain_id,
            addresses.len()
        );
        Ok(())
    }

    // -------------------------------------------------------------------------
    // WebSocket mode
    // -------------------------------------------------------------------------

    async fn add_pools_ws<T: TokenInfo>(
        &mut self,
        addresses: &[Address],
        fetch_config: &PoolFetchConfig,
        token_info: &T,
    ) -> Result<()> {
        let chain_id = fetch_config.chain_id;

        // 1. Stop old WS listeners first so the EventQueue channel stays open
        //    while we wait for the updater to drain and exit (step 2 below).
        //    Stopping the updater first would drop the EventQueue receiver,
        //    closing the channel and causing "channel closed" error spam from
        //    listeners that haven't been told to stop yet.
        for listener in &self.ws_listeners {
            if let Err(e) = listener.stop().await {
                error!("[Chain {}] Error stopping WS listener: {}", chain_id, e);
            }
        }
        self.ws_listeners.clear();

        // 2. Stop the updater and wait for it to exit. last_processed_block is now stable.
        self.stop_updater().await;
        let stop_block = self.pool_registry.get_last_processed_block();
        info!(
            "[Chain {}] add_pools(ws): updater stopped at block {}",
            chain_id, stop_block
        );

        // 3. Create a fresh EventQueue.
        let new_event_queue = EventQueue::new(1000, 1000, chain_id);
        let event_sender = new_event_queue.get_sender();

        // 4. Start new WS listeners for ALL addresses (existing + new) NOW,
        //    so they buffer events during the upcoming (slow) pool fetch phase.
        //    This mirrors the WebsocketBlockSource::bootstrap pattern: listeners
        //    start first so no events are missed between fetch and subscription.
        let all_current_addresses = self.pool_registry.get_all_addresses();
        let all_addresses: Vec<Address> = {
            let mut set: HashSet<Address> = all_current_addresses.iter().copied().collect();
            set.extend(addresses.iter().copied());
            set.into_iter().collect()
        };
        let topics = self.pool_registry.get_topics();
        let mut new_ws_listeners: Vec<Arc<WebsocketListener>> =
            Vec::with_capacity(self.ws_urls.len());
        for url in &self.ws_urls {
            let listener = Arc::new(WebsocketListener::new(
                url.clone(),
                all_addresses.clone(),
                Arc::clone(&event_sender),
                topics.clone(),
                chain_id,
            ));
            if let Err(e) = listener.start().await {
                error!(
                    "[Chain {}] Error starting WS listener for {}: {}",
                    chain_id, url, e
                );
            }
            new_ws_listeners.push(listener);
        }
        self.ws_listeners = new_ws_listeners;

        // 5. Fetch new pool state into memory at stop_block.
        //    WS listeners are already buffering events in the background.
        info!(
            "[Chain {}] add_pools(ws): fetching {} pools at block {}",
            chain_id,
            addresses.len(),
            stop_block
        );
        let pools = fetch_pools_in_memory(
            &self.provider,
            addresses,
            BlockId::Number(BlockNumberOrTag::Number(stop_block)),
            token_info,
            fetch_config,
        )
        .await?;

        // 6. Register pools and any new event topics.
        //    Pool state is at stop_block — consistent with existing pools.
        register_pools_and_topics(&self.pool_registry, pools);

        // 7. last_processed_block stays at stop_block (unchanged).
        //    WebsocketBlockSource::bootstrap() will drain the EventQueue
        //    (populated during step 5) and RPC-catch up ALL pools (old + new)
        //    from stop_block to first_event_block, then apply buffered events.

        // 8. Restart the updater in Websocket mode with the new EventQueue.
        self.spawn_updater(
            UpdaterMode::Websocket {
                event_queue: new_event_queue,
            },
            stop_block,
        );

        info!(
            "[Chain {}] add_pools(ws): done — {} new pools registered, updater restarted from block {}",
            chain_id,
            addresses.len(),
            stop_block
        );
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    /// Send the cancel signal and wait for the updater task to fully exit.
    ///
    /// Awaiting the `JoinHandle` guarantees that the updater's final
    /// `set_last_processed_block` call has completed before we read the
    /// registry's block cursor, preventing a stale-read race condition.
    async fn stop_updater(&mut self) {
        if let Some(tx) = self.cancel_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.updater_handle.take() {
            let _ = handle.await;
        }
    }

    /// Create and spawn a new `UnifiedPoolUpdater` task, storing the cancel
    /// sender and the `JoinHandle` for later awaiting in `stop_updater`.
    fn spawn_updater(&mut self, mode: UpdaterMode, start_block: u64) {
        let chain_id = self.pool_registry.get_network_id();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let mut updater = UnifiedPoolUpdater::new(
            Arc::clone(&self.provider),
            Arc::clone(&self.pool_registry),
            self.metrics.clone(),
            self.swap_event_tx.clone(),
            start_block,
            self.collector_config.max_blocks_per_batch,
            mode,
            cancel_rx,
        );
        let handle = tokio::spawn(async move {
            if let Err(e) = updater.start().await {
                error!("[Chain {}] Collector error after restart: {}", chain_id, e);
            }
        });
        self.updater_handle = Some(handle);
        self.cancel_tx = Some(cancel_tx);
    }
}

// =============================================================================
// Free helper functions
// =============================================================================

/// Fetch pool objects into memory without registering them.
///
/// Mirrors the chunking / rate-limiting / retry behaviour of
/// `fetch_pools_into_registry` but keeps pools in a local `Vec` so callers
/// can apply in-memory catchup before adding them to the registry.
async fn fetch_pools_in_memory<P: Provider + Send + Sync, T: TokenInfo>(
    provider: &Arc<P>,
    addresses: &[Address],
    block_number: BlockId,
    token_info: &T,
    config: &PoolFetchConfig,
) -> Result<Vec<Box<dyn PoolInterface>>> {
    let chain_id = config.chain_id;
    let chunk_size = config.chunk_size.max(1);
    let chunk_count = addresses.len().div_ceil(chunk_size);
    let mut pools: Vec<Box<dyn PoolInterface>> = Vec::with_capacity(addresses.len());

    for (chunk_idx, chunk) in addresses.chunks(chunk_size).enumerate() {
        info!(
            "[Chain {}] fetch_pools_in_memory: chunk {}/{} ({} pools)",
            chain_id,
            chunk_idx + 1,
            chunk_count,
            chunk.len()
        );

        let results: Vec<Result<Box<dyn PoolInterface>>> = if config.parallel_fetch {
            let futures: Vec<_> = chunk
                .iter()
                .map(|&address| {
                    let provider = Arc::clone(provider);
                    async move {
                        let pool_type = identify_pool_type(&provider, address).await?;
                        fetch_pool(
                            &provider,
                            address,
                            block_number,
                            pool_type,
                            token_info,
                            config,
                        )
                        .await
                    }
                })
                .collect();
            join_all(futures).await
        } else {
            let mut seq = Vec::with_capacity(chunk.len());
            for &address in chunk {
                let pool_type = identify_pool_type(provider, address).await?;
                seq.push(
                    fetch_pool(
                        provider,
                        address,
                        block_number,
                        pool_type,
                        token_info,
                        config,
                    )
                    .await,
                );
            }
            seq
        };

        // Retry failed pools with exponential backoff
        let mut failed: Vec<(usize, Address)> = Vec::new();
        for (i, result) in results.into_iter().enumerate() {
            match result {
                Ok(pool) => pools.push(pool),
                Err(e) => {
                    error!(
                        "[Chain {}] Failed to fetch pool {}: {}",
                        chain_id, chunk[i], e
                    );
                    failed.push((i, chunk[i]));
                }
            }
        }

        for (_, address) in failed {
            let mut success = false;
            for attempt in 1..=config.max_retries {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                info!(
                    "[Chain {}] Retrying pool {} (attempt {}/{})",
                    chain_id, address, attempt, config.max_retries
                );
                tokio::time::sleep(delay).await;
                match async {
                    let pool_type = identify_pool_type(provider, address).await?;
                    fetch_pool(
                        provider,
                        address,
                        block_number,
                        pool_type,
                        token_info,
                        config,
                    )
                    .await
                }
                .await
                {
                    Ok(pool) => {
                        pools.push(pool);
                        success = true;
                        break;
                    }
                    Err(e) => {
                        error!(
                            "[Chain {}] Retry {}/{} failed for pool {}: {}",
                            chain_id, attempt, config.max_retries, address, e
                        );
                    }
                }
            }
            if !success {
                return Err(anyhow::anyhow!(
                    "[Chain {}] Failed to fetch pool {} after {} retries",
                    chain_id,
                    address,
                    config.max_retries
                ));
            }
        }

        if chunk_idx + 1 < chunk_count && config.wait_time_between_chunks > 0 {
            tokio::time::sleep(Duration::from_millis(config.wait_time_between_chunks)).await;
        }
    }

    Ok(pools)
}

/// Fetch events for the given block range (filtered to the pool addresses) and
/// apply them in log order to the in-memory pool objects.
///
/// Only events whose `log.address()` matches one of the in-memory pools are
/// applied, so existing registry pools are never touched.
async fn apply_catchup_events_in_memory<P: Provider + Send + Sync>(
    provider: &Arc<P>,
    pools: &mut [Box<dyn PoolInterface>],
    topics: Vec<crate::Topic>,
    from_block: u64,
    to_block: u64,
    chain_id: u64,
) -> Result<()> {
    let addresses: Vec<Address> = pools.iter().map(|p| p.address()).collect();
    let addr_set: HashSet<Address> = addresses.iter().copied().collect();

    let events: Vec<Log> = fetch_events_with_retry(
        provider,
        addresses,
        topics,
        BlockNumberOrTag::Number(from_block),
        BlockNumberOrTag::Number(to_block),
        chain_id,
    )
    .await?;

    info!(
        "[Chain {}] apply_catchup_events_in_memory: {} events over {} pools",
        chain_id,
        events.len(),
        pools.len()
    );

    // Group events by address in log order so each pool gets a sequential slice.
    let mut events_by_addr: HashMap<Address, Vec<&Log>> = HashMap::new();
    for event in &events {
        let addr = event.address();
        if addr_set.contains(&addr) {
            events_by_addr.entry(addr).or_default().push(event);
        }
    }

    for pool in pools.iter_mut() {
        if let Some(pool_events) = events_by_addr.get(&pool.address()) {
            for event in pool_events {
                if let Err(e) = pool.apply_log(event) {
                    warn!(
                        "[Chain {}] Catchup event error for pool {}: {}",
                        chain_id,
                        pool.address(),
                        e
                    );
                }
            }
        }
    }

    Ok(())
}

/// Insert pool objects into the registry and register any previously unseen
/// pool-type event topics.
fn register_pools_and_topics(registry: &Arc<PoolRegistry>, pools: Vec<Box<dyn PoolInterface>>) {
    let mut new_pool_types: HashSet<PoolType> = HashSet::new();
    for pool in pools {
        new_pool_types.insert(pool.pool_type());
        registry.add_pool(pool);
    }
    for pool_type in new_pool_types {
        registry.add_topics(pool_type.topics());
        registry.add_profitable_topics(pool_type.profitable_topics());
    }
}
