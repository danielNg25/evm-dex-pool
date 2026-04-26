use crate::PoolRegistry;
use alloy::primitives::Address;
use alloy::providers::Provider;
use anyhow::Result;
use log::{debug, error, info, warn};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use super::algebra_fee_refetch::refetch_algebra_v3_fees;
use super::block_source::{
    BlockSource, EventBatch, LatestBlockSource, PendingBlockSource, ProcessingMode,
    WebsocketBlockSource,
};
use super::event_processor::{EventProcessor, PendingEvent};
use super::metrics::CollectorMetrics;
use super::multicall::resolve_multicall_address;
use super::EventQueue;

/// Updater mode configuration.
pub enum UpdaterMode {
    PendingBlock,
    LatestBlock { wait_time_ms: u64 },
    Websocket { event_queue: EventQueue },
}

/// Unified pool updater that composes a BlockSource with an EventProcessor.
///
/// Replaces the three separate updaters (PoolUpdater, PoolUpdaterLatestBlock,
/// PoolUpdaterLatestBlockWs) with a single implementation.
pub struct UnifiedPoolUpdater<P: Provider + Send + Sync + 'static> {
    provider: Arc<P>,
    source: Box<dyn BlockSource>,
    event_processor: EventProcessor,
    pool_registry: Arc<PoolRegistry>,
    chain_id: u64,
    cancel_rx: oneshot::Receiver<()>,
    /// If true, multicall `fee()` for every tracked Algebra V3 pool after each
    /// batch and write the fresh fee back into the registry.
    refetch_algebra_fee: bool,
    /// Resolved multicall3 address for this chain. Used by the post-batch
    /// Algebra fee refetch.
    multicall_address: Address,
}

impl<P: Provider + Send + Sync + 'static> UnifiedPoolUpdater<P> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<P>,
        pool_registry: Arc<PoolRegistry>,
        metrics: Option<Arc<dyn CollectorMetrics>>,
        swap_event_tx: Option<mpsc::Sender<PendingEvent>>,
        start_block: u64,
        max_blocks_per_batch: u64,
        mode: UpdaterMode,
        cancel_rx: oneshot::Receiver<()>,
        refetch_algebra_fee: bool,
    ) -> Self {
        let chain_id = pool_registry.get_network_id();

        // Initialize last_processed_block
        let current_block = pool_registry.get_last_processed_block();
        if current_block == 0 {
            pool_registry.set_last_processed_block(start_block);
            info!(
                "[Chain {}] Initialized last processed block to {}",
                chain_id, start_block
            );
        } else if start_block > 0 && start_block > current_block {
            pool_registry.set_last_processed_block(start_block);
            info!(
                "[Chain {}] Updated last processed block from {} to {}",
                chain_id, current_block, start_block
            );
        }

        let topics = Arc::new(pool_registry.get_topics().clone());
        let profitable_topics = Arc::new(pool_registry.get_profitable_topics().clone());

        let event_processor = EventProcessor::new(
            Arc::clone(&pool_registry),
            metrics,
            swap_event_tx,
            profitable_topics,
        );

        let source: Box<dyn BlockSource> = match mode {
            UpdaterMode::PendingBlock => Box::new(PendingBlockSource::new(
                Arc::clone(&provider),
                Arc::clone(&pool_registry),
                Arc::clone(&topics),
                max_blocks_per_batch,
            )),
            UpdaterMode::LatestBlock { wait_time_ms } => Box::new(LatestBlockSource::new(
                Arc::clone(&provider),
                Arc::clone(&pool_registry),
                Arc::clone(&topics),
                max_blocks_per_batch,
                wait_time_ms,
            )),
            UpdaterMode::Websocket { event_queue } => Box::new(WebsocketBlockSource::new(
                Arc::clone(&provider),
                event_queue,
                Arc::clone(&pool_registry),
                Arc::clone(&topics),
                max_blocks_per_batch,
            )),
        };

        let multicall_address = resolve_multicall_address(chain_id, None);

        Self {
            provider,
            source,
            event_processor,
            pool_registry,
            chain_id,
            cancel_rx,
            refetch_algebra_fee,
            multicall_address,
        }
    }

    /// Run the updater loop. This never returns under normal operation.
    pub async fn start(&mut self) -> Result<()> {
        let chain_id = self.chain_id;
        info!(
            "[Chain {}] UnifiedPoolUpdater: starting bootstrap...",
            chain_id
        );
        self.source.bootstrap().await?;
        info!(
            "[Chain {}] UnifiedPoolUpdater: bootstrap complete, entering main loop",
            chain_id
        );

        loop {
            debug!(
                "[Chain {}] UnifiedPoolUpdater: calling next_batch...",
                chain_id
            );
            let batch_result = tokio::select! {
                biased;
                result = &mut self.cancel_rx => {
                    match result {
                        Ok(()) => info!("[Chain {}] Collector received stop signal, shutting down", chain_id),
                        Err(_)  => warn!("[Chain {}] Collector stopping: CollectorHandle was dropped (cancel_tx gone)", chain_id),
                    }
                    return Ok(());
                }
                result = self.source.next_batch() => result,
            };

            match batch_result {
                Ok(EventBatch {
                    events,
                    processing_mode,
                    processed_through_block,
                }) => {
                    let event_count = events.len();
                    debug!(
                        "[Chain {}] UnifiedPoolUpdater: received batch with {} events",
                        chain_id, event_count
                    );

                    match processing_mode {
                        ProcessingMode::ApplyOnly => {
                            debug!(
                                "[Chain {}] UnifiedPoolUpdater: applying {} events (ApplyOnly)",
                                chain_id, event_count
                            );
                            self.event_processor.apply_events_to_registry(&events).await;
                        }
                        ProcessingMode::ConfirmedWithSwaps => {
                            debug!(
                                "[Chain {}] UnifiedPoolUpdater: processing {} events (ConfirmedWithSwaps)",
                                chain_id, event_count
                            );
                            self.event_processor.process_confirmed_events(events).await;
                        }
                        ProcessingMode::Pending => {
                            debug!(
                                "[Chain {}] UnifiedPoolUpdater: processing {} events (Pending)",
                                chain_id, event_count
                            );
                            self.event_processor.process_pending_events(events).await;
                        }
                    }

                    if self.refetch_algebra_fee {
                        if let Some(block) = processed_through_block {
                            let addresses = self.pool_registry.get_algebra_v3_addresses();
                            if !addresses.is_empty() {
                                if let Err(e) = refetch_algebra_v3_fees(
                                    &self.provider,
                                    &self.pool_registry,
                                    &addresses,
                                    self.multicall_address,
                                    chain_id,
                                    block,
                                )
                                .await
                                {
                                    warn!(
                                        "[Chain {}] Algebra V3 fee refetch failed: {}",
                                        chain_id, e
                                    );
                                }
                            }
                        }
                    }

                    if let Some(block) = processed_through_block {
                        self.pool_registry.set_last_processed_block(block);
                        info!(
                            "[Chain {}] Successfully processed through block {} with {} events",
                            chain_id, block, event_count
                        );
                    }
                    debug!(
                        "[Chain {}] UnifiedPoolUpdater: batch processing complete",
                        chain_id
                    );
                }
                Err(e) => {
                    error!("[Chain {}] Error fetching event batch: {}", chain_id, e);
                }
            }
        }
    }
}
